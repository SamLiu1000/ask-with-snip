//! Minimal CDP (Chrome DevTools Protocol) client that does exactly three things:
//! find the page target (HTTP), Runtime.evaluate (run a snippet of JS in the page),
//! and Input.insertText / dispatchKeyEvent (**the real input pipeline**).
//!
//! Why it is needed: the editors used by Gemini/Claude (Quill/tiptap) maintain their
//! own content state; synthetic events (paste / execCommand / setting textContent)
//! all get rolled back — in practice all three injection methods produce text that
//! vanishes within 2 seconds and the send button never lights up. CDP's
//! Input.insertText goes through the browser's real input pipeline, so editors
//! cannot distinguish it from a human typing and none of them can block it.
//!
//! Channel: after WebView2 is started with `--remote-debugging-port`, every account
//! window (one separate browser process per data directory) has a debug port on
//! 127.0.0.1. HTTP /json/list finds the page, CDP commands are sent over WebSocket.
//! **Only listens on 127.0.0.1**, never leaves the machine.
//!
//! The WebSocket is a hand-written minimal implementation: sends text frames only
//! (client frames must be masked) and receives text frames only (handles
//! fragmentation and ping; everything else is ignored). Good enough, no dependencies.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime};

/// Compute the debug port for an account: the account name is hashed into a small
/// range so that "when creating the window" and "when using it" produce the same
/// number. On a port collision (very unlikely) WebView2 fails to open the debug
/// channel and the driver falls back to pasting; asking still works.
pub fn cdp_port(account: &str) -> u16 {
    let mut h: u32 = 5381;
    for b in account.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    9333 + (h % 400) as u16
}

fn http_get_json(port: u16, path: &str) -> Result<serde_json::Value, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("cannot connect to debug port {port} (window not up yet?): {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    // Request line/headers must be CRLF: the DevTools server simply does not respond
    // to requests with bare LF (this was the root cause of the earlier "empty
    // response body" — the newlines in a multi-line string literal were \n not \r\n).
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
        .map_err(|e| e.to_string())?;
    // The DevTools server ignores Connection: close, so read_to_string would block
    // until timeout — follow the HTTP rules instead: read until the end of the
    // headers, then read the body exactly according to Content-Length.
    let mut raw: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read_exact(&mut byte).is_err() {
            break;
        }
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&raw).to_string();
    let clen = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut rest = Vec::new();
    if clen > 0 {
        rest.resize(clen, 0);
        stream.read_exact(&mut rest).map_err(|e| e.to_string())?;
    } else {
        let _ = stream.read_to_end(&mut rest);
    }
    let body = String::from_utf8_lossy(&rest);
    if body.trim().is_empty() {
        return Err(format!("debug endpoint returned an empty body, headers: {}", head.lines().map(str::trim).collect::<Vec<_>>().join(" | ")));
    }
    serde_json::from_str(body.trim()).map_err(|e| format!("debug endpoint response is not JSON: {e} ({})", &body[..body.len().min(160)]))
}

struct Ws {
    stream: TcpStream,
    mask: [u8; 4],
}

impl Ws {
    fn connect(ws_url: &str) -> Result<Ws, String> {
        use base64::Engine as _;
        let rest = ws_url
            .strip_prefix("ws://")
            .ok_or_else(|| format!("debug URL does not start with ws://: {ws_url}"))?;
        let (hostport, path) = rest
            .split_once('/')
            .map(|(h, p)| (h, format!("/{}", p)))
            .unwrap_or((rest, "/".into()));
        let mut stream = TcpStream::connect(hostport)
            .map_err(|e| format!("cannot connect to debug WebSocket ({hostport}): {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|e| e.to_string())?;
        // Mask key: time nanoseconds are good enough (loopback on this machine, not
        // an anti-attack measure, just "don't be all zeros")
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(42);
        let mask = [
            (nanos & 0xff) as u8,
            (nanos >> 8) as u8,
            (nanos >> 16) as u8,
            (nanos >> 24 | 1) as u8,
        ];
        let key = base64::engine::general_purpose::STANDARD
            .encode([mask[0], mask[1], mask[2], mask[3], 0x5a, 0xa5, 0x5a, 0xa5, mask[0], mask[1], mask[2], mask[3], 0x11, 0x22, 0x33, 0x44]);
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .map_err(|e| e.to_string())?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).map_err(|e| e.to_string())?;
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&head);
        if !head.starts_with("HTTP/1.1 101") && !head.starts_with("HTTP/1.0 101") {
            return Err(format!("debug WebSocket handshake rejected: {}", head.lines().next().unwrap_or("")));
        }
        Ok(Ws { stream, mask })
    }

    fn send_frame(&mut self, payload: &[u8], opcode: u8) -> Result<(), String> {
        let mut head = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            head.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            head.push(0x80 | 126);
            head.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            head.push(0x80 | 127);
            head.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ self.mask[i % 4])
            .collect();
        self.stream
            .write_all(&head)
            .and_then(|_| self.stream.write_all(&self.mask))
            .and_then(|_| self.stream.write_all(&masked))
            .map_err(|e| format!("failed to send debug frame: {e}"))
    }

    /// Read one complete message (assembles fragments, replies to pings).
    fn read_message(&mut self) -> Result<Vec<u8>, String> {
        let mut assembled: Vec<u8> = Vec::new();
        loop {
            let mut head = [0u8; 2];
            self.stream
                .read_exact(&mut head)
                .map_err(|e| format!("failed to read debug frame: {e}"))?;
            let opcode = head[0] & 0x0f;
            let fin = head[0] & 0x80 != 0;
            let masked = head[1] & 0x80 != 0;
            let mut len = (head[1] & 0x7f) as usize;
            if len == 126 {
                let mut ext = [0u8; 2];
                self.stream.read_exact(&mut ext).map_err(|e| e.to_string())?;
                len = u16::from_be_bytes(ext) as usize;
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.stream.read_exact(&mut ext).map_err(|e| e.to_string())?;
                len = u64::from_be_bytes(ext) as usize;
            }
            let mut mask_key = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut mask_key).map_err(|e| e.to_string())?;
            }
            let mut payload = vec![0u8; len];
            if len > 0 {
                self.stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
            }
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask_key[i % 4];
                }
            }
            match opcode {
                0x9 => {
                    // ping → pong (echo the payload back as-is)
                    let mut pong = vec![0x8a];
                    pong.push(payload.len() as u8);
                    self.stream
                        .write_all(&pong)
                        .and_then(|_| self.stream.write_all(&payload))
                        .map_err(|e| e.to_string())?;
                }
                0x8 => return Err("debug WebSocket was closed".into()),
                0x1 | 0x0 => {
                    assembled.extend_from_slice(&payload);
                    if fin {
                        return Ok(assembled);
                    }
                }
                _ => {} // binary etc.: ignore
            }
        }
    }
}

/// An established debug session (attached to an account's page).
pub struct Cdp {
    ws: Ws,
    next_id: u64,
}

impl Cdp {
    /// Connect to the account window's debug port, pick out the page target and
    /// complete the handshake.
    ///
    /// `url_must_contain` is a key fragment of the site's domain: the port is a fixed
    /// value computed per account, so if it collides with another browser process
    /// (or an old process has not fully exited), a URL mismatch means hands off —
    /// otherwise typing would land in another account's page. Retry the connection
    /// a few times on failure (right after the browser process starts the debug
    /// port may not be ready yet; in practice we hit "connection refused/timeout").
    pub fn connect(port: u16, url_must_contain: &str) -> Result<Cdp, String> {
        let mut last_err = String::new();
        for _ in 0..20 {
            match Self::connect_once(port, url_must_contain) {
                Ok(cdp) => return Ok(cdp),
                Err(err) => {
                    last_err = err;
                    std::thread::sleep(Duration::from_millis(1000));
                }
            }
        }
        Err(last_err)
    }

    fn connect_once(port: u16, url_must_contain: &str) -> Result<Cdp, String> {
        let list = http_get_json(port, "/json/list")?;
        let pages = list
            .as_array()
            .ok_or("debug target list is not an array")?;
        let page = pages
            .iter()
            .find(|t| {
                t.get("type").and_then(|v| v.as_str()) == Some("page")
                    && t.get("url")
                        .and_then(|v| v.as_str())
                        .map(|u| u.contains(url_must_contain))
                        .unwrap_or(false)
            })
            .ok_or_else(|| {
                format!(
                    "no page target for {} on the debug port (the port may be held by another browser process)",
                    url_must_contain
                )
            })?;
        let ws_url = page
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or("debug target has no webSocketDebuggerUrl")?;
        let ws = Ws::connect(ws_url)?;
        Ok(Cdp { ws, next_id: 1 })
    }

    /// Send a CDP command and wait for its reply (interleaved events are skipped).
    pub fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let text = serde_json::json!({ "id": id, "method": method, "params": params }).to_string();
        self.ws.send_frame(text.as_bytes(), 0x1)?;
        loop {
            let msg = self.ws.read_message()?;
            let value: serde_json::Value =
                serde_json::from_slice(&msg).map_err(|e| format!("debug reply is not JSON: {e}"))?;
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = value.get("error") {
                    return Err(format!("CDP {method} failed: {err}"));
                }
                return Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null));
            }
        }
    }

    /// **Actually** type a piece of text into the currently focused element
    /// (via the browser input pipeline; editors cannot block it).
    pub fn insert_text(&mut self, text: &str) -> Result<(), String> {
        self.call("Input.insertText", serde_json::json!({ "text": text }))
            .map(|_| ())
    }

    /// Press Enter (real key events): all three sites' input boxes are "Enter sends".
    pub fn press_enter(&mut self) -> Result<(), String> {
        self.call(
            "Input.dispatchKeyEvent",
            serde_json::json!({
                "type": "keyDown", "key": "Enter", "code": "Enter",
                "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13, "text": "\r",
            }),
        )?;
        self.call(
            "Input.dispatchKeyEvent",
            serde_json::json!({
                "type": "keyUp", "key": "Enter", "code": "Enter",
                "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13,
            }),
        )?;
        Ok(())
    }

    /// Natively push local files into the page's file input (DOM.setFileInputFiles).
    ///
    /// `pick_js` is a snippet of JS returning an **index number**: pages often have
    /// multiple input[type=file] elements (5 on chatgpt in practice), picking the
    /// wrong one makes the injection useless. JS does the picking, CDP does the
    /// setting — this is the browser-native file-setting pipeline, and the site has
    /// no synthetic event to intercept (the image gets in "for real").
    pub fn set_file_input_files(
        &mut self,
        pick_selector: &str,
        files: &[String],
    ) -> Result<(), String> {
        // Get a **fresh** nodeId: getDocument → querySelector → setFileInputFiles
        // done back to back. React can re-render and swap nodes at any moment; with
        // a stale node the files land on a detached node and are silently lost
        // (in practice files.length stayed 0).
        let _ = self.call("DOM.enable", serde_json::json!({}));
        let doc = self.call("DOM.getDocument", serde_json::json!({}))?;
        let root = doc
            .pointer("/root/nodeId")
            .and_then(|v| v.as_i64())
            .ok_or("DOM.getDocument returned no root node")?;
        // A CSS attribute-substring selector can express "accept contains image":
        // input[type=file][accept*=image]
        let mut node_id = None;
        for selector in [pick_selector, "input[type=file]"] {
            let q = self.call(
                "DOM.querySelector",
                serde_json::json!({ "nodeId": root, "selector": selector }),
            );
            if let Ok(q) = q {
                if let Some(id) = q.get("nodeId").and_then(|v| v.as_i64()) {
                    if id > 0 {
                        node_id = Some(id);
                        break;
                    }
                }
            }
        }
        let node_id = node_id.ok_or("no file input on the page that can take images")?;
        self.call(
            "DOM.setFileInputFiles",
            serde_json::json!({ "files": files, "nodeId": node_id }),
        )?;
        // Chromium does not necessarily dispatch a change event after setting files,
        // and React's onChange would then never upload. Fire both events, then
        // self-check that the files are really there.
        let _ = self.evaluate(
            "(function(){var el=document.querySelectorAll('input[type=file]');\
             for(const e of el){e.dispatchEvent(new Event('input',{bubbles:true}));e.dispatchEvent(new Event('change',{bubbles:true}));}return 'fired';})()",
        )?;
        let check = self.evaluate(
            "(function(){var n=0;for(const el of document.querySelectorAll('input[type=file]')){if(el.files)n+=el.files.length;}return 'ok:'+n;})()",
        )?;
        match check.get("value").and_then(|v| v.as_str()) {
            Some(s) if s.starts_with("ok:") && !s.ends_with(":0") => Ok(()),
            Some(s) => Err(format!("files were set into the input but the site did not accept them ({s})")),
            None => Err("self-check reply is malformed".into()),
        }
    }

    /// Press Ctrl+V (real key events): the image in the system clipboard enters the
    /// editor as a **native paste event**, through the same browser pipeline as a
    /// human pasting — nobody can block it or tell the difference.
    pub fn press_ctrl_v(&mut self) -> Result<(), String> {
        for (typ, key) in [("keyDown", "v"), ("keyUp", "v")] {
            self.call(
                "Input.dispatchKeyEvent",
                serde_json::json!({
                    "type": typ, "key": key, "code": "KeyV",
                    "windowsVirtualKeyCode": 86, "nativeVirtualKeyCode": 86,
                    "modifiers": 2,
                }),
            )?;
        }
        Ok(())
    }

    /// Put PNG bytes into the system clipboard (RGBA bitmap).
    pub fn clipboard_set_png(bytes: &[u8]) -> Result<(), String> {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder
            .read_info()
            .map_err(|e| format!("failed to decode PNG: {e}"))?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| format!("failed to decode PNG frame: {e}"))?;
        let (w, h) = (info.width as usize, info.height as usize);
        // The decoded data may be RGB/grayscale etc.; normalize to RGBA (arboard only
        // accepts that)
        let src = &buf[..info.buffer_size()];
        let channels = info.color_type.samples() as usize;
        let rgba: Vec<u8> = if channels == 4 {
            src.to_vec()
        } else {
            let mut out = Vec::with_capacity(w * h * 4);
            for px in src.chunks(channels) {
                out.extend_from_slice(px);
                if channels < 3 {
                    out.push(*px.last().unwrap_or(&255));
                    out.extend(std::iter::repeat(*px.last().unwrap_or(&255)).take(3 - channels));
                } else {
                    out.push(255);
                }
            }
            out
        };
        arboard::Clipboard::new()
            .and_then(|mut cb| {
                cb.set_image(arboard::ImageData {
                    width: w,
                    height: h,
                    bytes: std::borrow::Cow::Owned(rgba.into()),
                })
            })
            .map_err(|e| format!("failed to write clipboard: {e}"))
    }

    /// Put an image on the clipboard, **format-adaptive**. The screenshot path
    /// produces JPEG (the max_image_edge/jpeg_quality encoding path); this used to
    /// only accept PNG, so JPEG came in and died instantly with "Invalid PNG
    /// signature" (in practice: once the file-picker route stopped working, the
    /// clipboard fallback also failed to start and images never went up — only the
    /// text got sent). Let image::load_from_memory detect the format (accepts both
    /// PNG and JPEG) and decode uniformly to RGBA.
    pub fn clipboard_set_image(bytes: &[u8]) -> Result<(), String> {
        let img = image::load_from_memory(bytes)
            .map_err(|e| format!("failed to decode image (neither PNG nor JPEG): {e}"))?;
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width() as usize, rgba.height() as usize);
        arboard::Clipboard::new()
            .and_then(|mut cb| {
                cb.set_image(arboard::ImageData {
                    width: w,
                    height: h,
                    bytes: std::borrow::Cow::Owned(rgba.into_raw()),
                })
            })
            .map_err(|e| format!("failed to write clipboard: {e}"))
    }

    /// Wait for one CDP event (call() skips events; here it is the other way
    /// around: only wait for the event).
    fn wait_event(&mut self, method: &str, secs: u64) -> Option<serde_json::Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        self.ws.stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        let result = loop {
            if std::time::Instant::now() > deadline {
                break None;
            }
            match self.ws.read_message() {
                Ok(msg) => {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg) {
                        if v.get("method").and_then(|m| m.as_str()) == Some(method) {
                            break Some(v.get("params").cloned().unwrap_or_default());
                        }
                    }
                }
                Err(_) if std::time::Instant::now() > deadline => break None,
                Err(_) => {}
            }
        };
        self.ws.stream.set_read_timeout(Some(Duration::from_secs(20))).ok();
        result
    }

    /// Hand files to the page via the **real upload flow**: intercept the file
    /// chooser → use a real CDP mouse click on the upload entry (even a hidden file
    /// input needs a real click; a programmatic click has no user activation and the
    /// chooser never opens) → wait for Page.fileChooserOpened to yield the target
    /// node → set files natively. This is the site's own upload pipeline receiving
    /// the files (React can handle them however it likes); in practice it is immune
    /// to "we just set them directly".
    pub fn upload_via_file_chooser(
        &mut self,
        rect_steps: &[String],
        files: &[String],
    ) -> Result<(), String> {
        let _ = self.call("Page.enable", serde_json::json!({}));
        let _ = self.call(
            "Page.setInterceptFileChooserDialog",
            serde_json::json!({ "enabled": true }),
        );
        let mut last_err = String::from("no upload entry was given");
        for (i, rect_js) in rect_steps.iter().enumerate() {
            // Menus/animations can be slower than expected: on each step, if the
            // element is not found wait 800ms and retry, at most 5 times
            let mut coords: Vec<f64> = Vec::new();
            for _ in 0..5 {
                let rect = self.evaluate(rect_js)?;
                coords = rect
                    .get("value")
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                if coords.len() == 2 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(800));
            }
            if coords.len() != 2 {
                // Bring back the text of the visible "clickable elements" on the page
                // to calibrate the menu selector
                let dump = self.evaluate("(function(){return JSON.stringify([...document.querySelectorAll('[role=menuitem],[role=menu] *,[role=dialog] *')].filter(function(e){return e.getBoundingClientRect().width>0&&(e.innerText||'').trim()&&e.children.length<3}).slice(0,15).map(function(e){return (e.tagName+'|'+(e.getAttribute('role')||'')+'|'+(e.innerText||'').trim().slice(0,20))}))})()")
                    .ok()
                    .and_then(|v| v.get("value").and_then(|x| x.as_str()).map(String::from))
                    .unwrap_or_default();
                last_err = format!("step {} could not find the upload entry (visible elements: {dump})", i + 1);
                break;
            }
            let (x, y) = (coords[0], coords[1]);
            let _ = self.call("Input.dispatchMouseEvent", serde_json::json!({ "type": "mouseMoved", "x": x, "y": y }));
            for typ in ["mousePressed", "mouseReleased"] {
                self.call(
                    "Input.dispatchMouseEvent",
                    serde_json::json!({
                        "type": typ, "x": x, "y": y,
                        "button": "left", "clickCount": 1,
                    }),
                )?;
            }
            // Let the menu/animation settle
            std::thread::sleep(Duration::from_millis(1100));
        }
        let opened = self.wait_event("Page.fileChooserOpened", 6);
        let _ = self.call(
            "Page.setInterceptFileChooserDialog",
            serde_json::json!({ "enabled": false }),
        );
        let backend = opened
            .as_ref()
            .and_then(|p| p.get("backendNodeId").and_then(|b| b.as_i64()))
            .ok_or_else(|| {
                format!("{last_err}; or the real click happened but the file chooser never opened (waited 6 seconds)")
            })?;
        self.call(
            "DOM.setFileInputFiles",
            serde_json::json!({ "files": files, "backendNodeId": backend }),
        )?;
        Ok(())
    }

    /// Execute a snippet of JS in the page and return the evaluation result.
    pub fn evaluate(&mut self, expression: &str) -> Result<serde_json::Value, String> {
        let result = self.call(
            "Runtime.evaluate",
            serde_json::json!({ "expression": expression, "returnByValue": true }),
        )?;
        if let Some(details) = result.get("exceptionDetails") {
            return Err(format!(
                "error while executing in page: {}",
                details
                    .pointer("/exceptionDetails/text")
                    .or_else(|| details.pointer("/text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no details)")
            ));
        }
        Ok(result.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }
}
