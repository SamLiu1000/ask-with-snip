//! Web UI probe: verifies that the embedded window can actually load the
//! remote page and render content.
//!
//! Usage (from the src-tauri directory):
//!     cargo run --features probe --bin webchat_probe
//!
//! Why a bin and not an example: Tauri's Windows application manifest hangs
//! off the bin target; without it an example dies at load time
//! (STATUS_ENTRYPOINT_NOT_FOUND). So this probe is gated by required-features
//! and normal cargo build / tauri build never builds it.
//!
//! Why it exists: the web UI path is the only one that depends on external
//! sites — a changed URL, an anti-scraping block, a new verification step all
//! show up as "the window opened but it's blank", which cannot be guessed from
//! code alone. Running this shows which URL the page actually landed on and
//! what text it rendered. **It opens a window on screen and closes itself when
//! done.**
//!
//! Login state is not part of the verification: that is one click the user
//! does in the window themselves.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use tauri::webview::PageLoadEvent;
use tauri::{WebviewUrl, WebviewWindowBuilder};

/// Keep in sync with WEBCHAT_URL in lib.rs.
const URL: &str = "https://chat.deepseek.com/";
/// Wait a bit longer after the page finishes loading: this is a JS SPA, and
/// the load event does not mean the content is out.
const SETTLE: Duration = Duration::from_secs(6);
/// How long to wait between injecting JS and a document.title change. The
/// in-page reconnaissance happens in steps (2.5s for the image, 3s more after
/// opening an old conversation), so this must be generous.
const TITLE_WAIT: Duration = Duration::from_secs(14);
/// The page uses this separator to join its results into one line (newlines
/// get eaten in window titles)
const SEP: &str = " | ";

/// The probe's own payloads use `PROBE|`; the driver's `collect_js` sends
/// `SNAPI|` — both must be recognized.
///
/// A pitfall from here: `webchat::parse_signal` only recognizes `SNAPI|`, the
/// probe wrote all results as `PROBE|`, so every return value was treated as
/// "the site changing its own title" and dropped — the symptom was **no report
/// at all**, which initially looked like the page not rendering. So parsing
/// must be split by tag.
fn parse_any(title: &str) -> Option<serde_json::Value> {
    let body = title
        .strip_prefix("PROBE|")
        .or_else(|| title.strip_prefix(ask_with_snip_lib::webchat::TAG))?;
    serde_json::from_str(body).ok()
}

/// Run a piece of DOM reconnaissance in the page and write the result into
/// document.title for the Rust side to read back.
///
/// Only plain DOM operations run in the page; no Tauri API is touched — the
/// remote page can't call it anyway.
///
/// ` | ` is used as the separator instead of newlines: **newlines get eaten in
/// window titles** (bitten on the very first attempt — everything became one
/// line and parsing broke). The payload must also stay short; the title has a
/// length limit.
///
/// Three steps:
///   1. Whether the composer / login token / file inputs are present.
///   2. Stuff an image into the hidden file input (simulating "hand the
///      screenshot to the web UI") — the foundation.
///   3. Open an old conversation (**read-only**, nothing is sent) and see how
///      messages are organized; the driver reads answers according to this
///      structure. **Send is never clicked.**
const PROBE_JS: &str = r#"
(() => {
  const out = [];
  const SEP = ' | ';
  const vis = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const cls = (el) => (el && el.className && typeof el.className === 'string')
    ? el.className.split(/\s+/).filter(Boolean).join('.')
    : '';
  const short = (el) => el ? el.tagName.toLowerCase() + '.' + cls(el).slice(0, 46) : '(none)';

  // "发送消息" matches the real page's Chinese placeholder — keep for matching.
  const ta = document.querySelector('textarea[placeholder*="发送消息"]');
  const fileInputs = [...document.querySelectorAll('input[type=file]')];
  out.push('composer: ' + (ta ? 'yes' : 'MISSING!') +
    SEP + 'login token: ' + (localStorage.getItem('userToken') ? 'yes' : 'no') +
    SEP + 'file input: ' + fileInputs.length);

  // Find the left conversation list: guessed by href shape, steadier than
  // hashed class names
  const links = [...document.querySelectorAll('a[href]')]
    .filter((el) => vis(el) && el.getBoundingClientRect().left < window.innerWidth * 0.4);
  out.push('left links: ' + links.length + ' e.g. ' +
    links.slice(0, 3).map((el) => (el.getAttribute('href') || '').slice(0, 40)).join(','));

  // ---- Step 2: stuff the image ----
  const composer = ta ? ta.closest('form') || ta.parentElement.parentElement.parentElement : null;
  const shapes = () => composer
    ? 'img=' + composer.querySelectorAll('img').length + ',nodes=' + composer.querySelectorAll('*').length
    : '(no container)';
  out.push('before image: ' + shapes());

  const canvas = document.createElement('canvas');
  canvas.width = 240;
  canvas.height = 120;
  const ctx = canvas.getContext('2d');
  ctx.fillStyle = '#d81e28';
  ctx.fillRect(0, 0, 240, 120);

  canvas.toBlob((blob) => {
    try {
      const input = fileInputs[0];
      if (input) {
        const dt = new DataTransfer();
        dt.items.add(new File([blob], 'probe.png', { type: 'image/png' }));
        const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'files').set;
        setter.call(input, dt.files);
        input.dispatchEvent(new Event('change', { bubbles: true }));
        out.push('image: change dispatched');
      } else {
        out.push('image: no file input');
      }
    } catch (e) {
      out.push('image failed: ' + e.name);
    }

    setTimeout(() => {
      out.push('after image: ' + shapes());

      // ---- Step 3: open an old conversation, inspect the message structure (read-only) ----
      // Conversation items have hrefs like /a/chat/s/<uuid>. The sidebar is
      // lazy-loaded — found one run, zero the next — so poll for it.
      let tries = 0;
      const lookForChat = () => {
        const links = [...document.querySelectorAll('a[href*="/a/chat/s/"]')].filter(vis);
        if (links.length) {
          out.push('conversation items: ' + links.length);
          out.push('opened: ' + (links[0].getAttribute('href') || '').slice(-12));
          // click() instead of location.href: this is a SPA, routing
          // intercepts the click for a front-end navigation, the page does not
          // reload, and the injected script (with the accumulated out) survives
          // to the message-reading step.
          links[0].click();
          setTimeout(dumpMessages, 3500);
          return;
        }
        if (tries++ < 20) {
          setTimeout(lookForChat, 1000);
          return;
        }
        out.push('conversation items: 0 (sidebar never appeared after 20s)');
        document.title = 'PROBE|' + out.join(SEP);
      };

      const dumpMessages = () => {
        const msgs = [...document.querySelectorAll('[class*=message], [data-message-author-role], [class*=chat-item]')]
          .filter(vis);
        out.push('message candidates: ' + msgs.length + ' class examples: ' +
          msgs.slice(0, 2).map((el) => cls(el).slice(0, 34)).join(' ;; '));

        // The longest visible text block on the page is most likely the node
        // holding the answer body
        const blocks = [...document.querySelectorAll('div, p')]
          .filter((el) => vis(el) && (el.innerText || '').trim().length > 60)
          .sort((a, b) => (b.innerText || '').trim().length - (a.innerText || '').trim().length);
        out.push('longest text block: ' + (blocks[0] ? cls(blocks[0]).slice(0, 34) + ' len=' + (blocks[0].innerText || '').trim().length : '(none)'));
        out.push('body: ' + JSON.stringify(document.body.innerText.replace(/\s+/g, ' ').trim().slice(0, 80)));
        document.title = 'PROBE|' + out.join(SEP);
      };

      lookForChat();

      setTimeout(() => {
        const msgs = [...document.querySelectorAll('[class*=message], [data-message-author-role], [class*=chat-item]')]
          .filter(vis);
        out.push('message candidates: ' + msgs.length + ' e.g.: ' +
          msgs.slice(0, 3).map(short).join(' ;; '));

        // The longest visible text block is most likely the answer body
        const blocks = [...document.querySelectorAll('div')]
          .filter((el) => vis(el) && el.children.length === 0 && (el.innerText || '').length > 30)
          .sort((a, b) => (b.innerText || '').length - (a.innerText || '').length);
        out.push('longest text block: ' + short(blocks[0]) +
          ' len=' + (blocks[0] ? (blocks[0].innerText || '').length : 0));
        out.push('body: ' + JSON.stringify(
          document.body.innerText.replace(/\s+/g, ' ').trim().slice(0, 80)
        ));
        document.title = 'PROBE|' + out.join(SEP);
      }, 3000);
    }, 2500);
  }, 'image/png');
})()
"#;

/// Dry run: use the **driver's own JS** (collect/compose from the webchat
/// module) to walk "wait for readiness -> paste image -> type the question",
/// except that **send is never clicked**.
/// This is the only way to verify the driver's first steps without sending
/// anything to the site. Afterwards it reports how many characters are in the
/// composer and how many images the container holds — an accepted image shows
/// up as an extra thumbnail. The actual send step can only be exercised by the
/// user clicking from the panel.
fn dry_run(win: &tauri::WebviewWindow, rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>) {
    use ask_with_snip_lib::webchat;

    let wait_signal = |timeout: Duration| -> Option<serde_json::Value> {
        let rx = rx.lock().ok()?;
        match rx.recv_timeout(timeout) {
            Ok(title) => parse_any(&title),
            Err(_) => None,
        }
    };
    // The channel accumulates receipts from earlier steps (compose replies
    // too), so reading must be able to pick out the wanted field
    let wait_for_field = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            if let Some(value) = wait_signal(Duration::from_secs(2)) {
                if value.get(field).is_some() {
                    return Some(value);
                }
            }
        }
        None
    };

    println!("== dry run: waiting for the composer to be ready ==");
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    let mut ready = false;
    while std::time::Instant::now() < deadline && !ready {
        if let Err(err) = win.eval(webchat::collect_js()) {
            println!("injection failed: {err}");
            return;
        }
        if let Some(value) = wait_signal(Duration::from_secs(2)) {
            if value.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
                ready = true;
                println!("  composer present, page ready");
            }
        }
    }
    if !ready {
        println!("  composer never appeared: the page is probably a login or verification page");
        return;
    }

    // Start a new conversation: click the button, or (with the sidebar
    // collapsed) reload the home page. Neither route sends anything.
    println!("== dry run: starting a new conversation ==");
    let _ = win.eval(webchat::new_chat_js(webchat::probe_site()));
    std::thread::sleep(Duration::from_millis(900));
    let mut route = String::from("(no report)");
    for _ in 0..5 {
        match wait_signal(Duration::from_millis(600)) {
            Some(value) => {
                if let Some(kind) = value.get("newchat").and_then(|v| v.as_str()) {
                    route = match kind {
                        "clicked" => "clicked the \"new chat\" button".into(),
                        "fallback" => "button not clickable, fell back to the home page (page reloads)".into(),
                        other => other.to_string(),
                    };
                    break;
                }
            }
            None => break,
        }
    }
    println!("  {route}");
    if route.contains("home page") {
        // The page reloaded; wait a moment and confirm the composer is back
        std::thread::sleep(Duration::from_secs(4));
        let _ = win.eval(webchat::collect_js());
        let ready_again = wait_for_field("ready", 6)
            .and_then(|v| v.get("ready").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        println!("  composer after reload: {}", if ready_again { "back" } else { "STILL MISSING!" });
    }

    // Build **two** different PNGs as "screenshots": the dry run also verifies
    // the page accepts multiple images in one go
    let make_png = |left: u8| -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(160, 90, image::Rgba([left, 30, 40, 255]));
        let mut png = Vec::new();
        {
            use image::ImageEncoder as _;
            let encoder = image::codecs::png::PngEncoder::new(&mut png);
            encoder
                .write_image(&img, 160, 90, image::ExtendedColorType::Rgba8)
                .expect("failed to encode png");
        }
        png
    };
    let pngs = vec![make_png(216), make_png(40)];

    println!("== dry run: paste {} image(s) + type the question (no send) ==", pngs.len());
    let turn = webchat::Turn {
        prompt: "probe dry run, do not send".into(),
        images: pngs.iter().map(|p| base64_of(p)).collect(),
        new_chat: false,
        resume: None,
    };
    if let Err(err) = win.eval(webchat::compose_js(webchat::probe_site(), &turn)) {
        println!("injection failed: {err}");
        return;
    }

    std::thread::sleep(Duration::from_millis(2500));
    if let Err(err) = win.eval(webchat::collect_js()) {
        println!("injection failed: {err}");
        return;
    }

    match wait_for_field("input", 8) {
        None => println!("  no report from the page"),
        Some(value) => {
            let input = value.get("input").and_then(|v| v.as_i64()).unwrap_or(-1);
            let imgs = value.get("imgs").and_then(|v| v.as_i64()).unwrap_or(-1);
            let answers = value.get("answers").and_then(|v| v.as_i64()).unwrap_or(-1);
            println!("  characters in the composer: {input} (nonzero means the question was typed in)");
            println!("  images in the composer container: {imgs} (thumbnails appearing means the images were accepted)");
            println!("  answer count on the page: {answers}");
            println!();
            if input > 0 && imgs >= pngs.len() as i64 {
                println!("Conclusion: pasting ({imgs} received, {} expected) and typing both hold on the real page.", pngs.len());
                println!("          Only \"click send\" remains unverified — that step sends data to the site, so only you can click it.");
            } else if input <= 0 {
                println!("Conclusion: the question never made it into the composer; the driver's compose step is broken.");
            } else {
                println!("Conclusion: text was typed in, but no image appeared in the container — the file-stuffing step doesn't hold on the real page.");
            }
        }
    }
}

/// Converter verification: open an **existing** old conversation (read-only,
/// nothing is sent) and convert the last answer to Markdown for printing.
///
/// This is the driver's most error-prone and least self-checkable step —
/// whether "the answer read back is right" can only be judged against a real
/// answer. Result slices come back via the window title (length-limited), so
/// they are fetched slice by slice by offset here.
fn convert_run(win: &tauri::WebviewWindow, rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>) {
    use ask_with_snip_lib::webchat;

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(title) => {
                    if let Some(value) = parse_any(&title) {
                        if value.get(field).is_some() {
                            return Some(value);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    };

    println!("== waiting for the page to be ready ==");
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    let mut ready = false;
    while std::time::Instant::now() < deadline && !ready {
        if win.eval(webchat::collect_js()).is_err() {
            println!("injection failed");
            return;
        }
        if let Some(value) = wait_for("ready", 2) {
            ready = value.get("ready").and_then(|v| v.as_bool()).unwrap_or(false);
        }
    }
    if !ready {
        println!("  page not ready (maybe not logged in)");
        return;
    }
    println!("  ready");

    // Open an old conversation: conversation items have hrefs like
    // /a/chat/s/<uuid> and the sidebar is lazy-loaded, so retry until answer
    // nodes really appear on the page (judged by the driver collect's answers)
    println!("== opening an old conversation (read-only) ==");
    // Diagnose once first: do conversation anchors exist at all, are they
    // visible? The sidebar is lazy-loaded and comes and goes.
    let diag_js = r#"
(() => {
  const all = [...document.querySelectorAll('a[href*="/a/chat/s/"]')];
  const shown = all.filter((el) => el.getBoundingClientRect().width > 0);
  document.title = 'PROBE|' + JSON.stringify({
    total: all.length,
    shown: shown.length,
    first: all.length ? all[0].getAttribute('href') : '',
  });
})()
"#;
    let _ = win.eval(diag_js);
    if let Ok(rx) = rx.lock() {
        if let Ok(title) = rx.recv_timeout(Duration::from_secs(3)) {
            println!("  diagnose: {title}");
        }
    }

    let open_js = r#"
(() => {
  const links = [...document.querySelectorAll('a[href*="/a/chat/s/"]')]
    .filter((el) => el.getBoundingClientRect().width > 0);
  if (links.length) links[0].click();
})()
"#;
    let mut answers = 0i64;
    for attempt in 1..=15 {
        if win.eval(open_js).is_err() {
            println!("injection failed");
            return;
        }
        std::thread::sleep(Duration::from_millis(900));
        if win.eval(webchat::collect_js()).is_err() {
            println!("injection failed");
            return;
        }
        if let Some(value) = wait_for("answers", 2) {
            answers = value.get("answers").and_then(|v| v.as_i64()).unwrap_or(0);
            if answers > 0 {
                println!("  after attempt {attempt}, the page holds {answers} answers");
                break;
            }
        }
    }
    if answers == 0 {
        println!("  could not open an old conversation (none in the sidebar, or the site was redesigned)");
        return;
    }

    println!("== converting the last answer to Markdown ==");
    const SLICE: usize = 700;
    let mut offset = 0usize;
    let mut dump = String::new();
    let mut reported = false;
    loop {
        let js = format!(
            "{}\n(() => {{\n  const nodes = document.querySelectorAll('{}');\n  const node = nodes[nodes.length - 1];\n  const text = node ? window.__snapiMD(node) : '';\n  let diag = '';\n  try {{\n    const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');\n    const chain = [];\n    let root = node;\n    for (let i = 0; i < 6 && root && root.parentElement; i += 1) {{\n      root = root.parentElement;\n      chain.push(cut(String(root.className || ''), 24));\n    }}\n    const norm = (s) => (s || '').replace(/\\s+/g, ' ').trim();\n    const all = root ? norm(root.innerText) : '';\n    const mine = norm(node ? node.innerText : '');\n    const extra = mine ? all.split(mine).join('') : all;\n    diag = 'chain: ' + chain.join(' < ') + ' || outside the answer: ' + cut(extra, 400);\n  }} catch (e) {{ diag = 'diagnose failed: ' + e.name; }}\n  document.title = '{}' + JSON.stringify({{ off: {}, total: text.length, chunk: text.slice({}, {}), diag: diag }});\n}})()",
            webchat::markdown_js(),
            ".ds-assistant-message-main-content",
            webchat::TAG,
            offset,
            offset,
            offset + SLICE
        );
        if win.eval(js).is_err() {
            println!("injection failed");
            return;
        }
        let Some(value) = wait_for("chunk", 3) else {
            println!("  could not fetch content");
            return;
        };
        let total = value.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if !reported {
            println!("  converted total: {total} chars");
            if let Some(diag) = value.get("diag").and_then(|v| v.as_str()) {
                println!("  structure diagnose: {diag}");
            }
            reported = true;
        }
        let chunk = value.get("chunk").and_then(|v| v.as_str()).unwrap_or("");
        if chunk.is_empty() {
            break;
        }
        dump.push_str(chunk);
        offset += SLICE;
        if offset > 20000 || offset >= total {
            break;
        }
    }
    println!();
    println!("--- converted Markdown ---");
    println!("{dump}");
    println!("--- end ---");
}

/// Find the "deep thinking" container: print elements whose class names
/// contain think/reason together with their lengths, then open old
/// conversations one by one to see which have a thinking process.
/// **Read-only, nothing is sent.**
fn think_run(win: &tauri::WebviewWindow, rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>) {
    use ask_with_snip_lib::webchat;

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(title) => {
                    if let Some(value) = parse_any(&title) {
                        if value.get(field).is_some() {
                            return Some(value);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    };

    // Wait for the page to be ready first
    for _ in 0..20 {
        let _ = win.eval(webchat::collect_js());
        if wait_for("ready", 2)
            .and_then(|v| v.get("ready").and_then(|b| b.as_bool()))
            .unwrap_or(false)
        {
            break;
        }
    }

    // List the conversations in the sidebar (when collapsed the anchors are
    // still in the DOM, so don't filter by visibility)
    //
    // But in a narrow window the sidebar is **not rendered at all** (measured:
    // 0 items), so it must be expanded first. The expand button is in the top
    // left corner with hashed class names, so find it by position, click, then
    // see whether the links increased.
    let open_sidebar_js = r#"
(() => {
  const vis = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const links = () => document.querySelectorAll('a[href*="/a/chat/s/"]').length;
  const before = links();
  const corner = [...document.querySelectorAll('button, [role=button]')]
    .filter((el) => vis(el))
    .filter((el) => {
      const r = el.getBoundingClientRect();
      return r.left < 90 && r.top < 90;
    });
  for (const el of corner.slice(0, 4)) el.click();
  document.title = 'PROBE|' + JSON.stringify({ err: '', before: before, clicked: corner.length, after: links() });
})()
"#;
    let _ = win.eval(open_sidebar_js);
    if let Some(value) = wait_for("err", 3) {
        println!(
            "  sidebar expand: clicked {} top-left button(s), conversation items {} -> {}",
            value.get("clicked").and_then(|v| v.as_u64()).unwrap_or(0),
            value.get("before").and_then(|v| v.as_u64()).unwrap_or(0),
            value.get("after").and_then(|v| v.as_u64()).unwrap_or(0),
        );
    }
    std::thread::sleep(Duration::from_millis(1200));

    let list_js = r#"
(() => {
  const links = [...document.querySelectorAll('a[href*="/a/chat/s/"]')].map((a) => a.getAttribute('href'));
  document.title = 'PROBE|' + JSON.stringify({ links: links });
})()
"#;
    let _ = win.eval(list_js);
    let links: Vec<String> = wait_for("links", 3)
        .and_then(|v| v.get("links").and_then(|l| l.as_array()).cloned())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    println!("the sidebar holds {} conversation(s)", links.len());
    for link in &links {
        println!("  {link}");
    }

    // Open each one (read-only) and check for a thinking-process container
    let probe_js = r#"
(() => {
  try {
    // The thinking block may be collapsed with empty innerText — print it
    // anyway; a length of 0 is itself a clue. The payload must stay short:
    // window titles have a length limit, overflow gets truncated and the JSON
    // becomes unparseable.
    const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
    const hit = [...document.querySelectorAll('[class*=think], [class*=Think], [class*=reason], [class*=Reason]')]
      .slice(0, 5)
      .map((el) => ({
        cls: cut(typeof el.className === 'string' ? el.className : '', 46),
        len: (el.innerText || '').trim().length,
      }));
    // '深度思考' matches the real page's Chinese label — keep for matching.
    const label = [...document.querySelectorAll('div, span, button')]
      .filter((el) => el.children.length === 0 && (el.innerText || '').trim() === '深度思考')
      .slice(0, 2)
      .map((el) => cut(typeof el.className === 'string' ? el.className : '', 46));
    const answers = document.querySelectorAll('.ds-assistant-message-main-content').length;

    // Known: what really holds the thinking process is .ds-think-content
    // (hash-free class name, measured stable in real sessions). What needs
    // answering here is its position relative to the "answer node" — the
    // driver finds the message root via node.closest('.ds-message'), so the
    // thinking block must be inside the same root to be readable.
    const think = document.querySelector('.ds-think-content');
    let place = '';
    if (think) {
      const node = document.querySelector('.ds-assistant-message-main-content');
      const root = node ? node.closest('.ds-message') : null;
      const chain = [];
      let up = think;
      for (let i = 0; i < 5 && up && up.parentElement; i += 1) {
        up = up.parentElement;
        chain.push(cut(String(up.className || ''), 22));
      }
      place =
        'same root=' + (root && root.contains(think) ? 'yes' : 'no') +
        ' root class=' + cut(root ? String(root.className || '') : '', 30) +
        ' chain=' + chain.join('<') +
        ' visible=' + (think.getBoundingClientRect().height > 0 ? 'yes' : 'no') +
        ' previous sibling=' + cut(think.previousElementSibling ? String(think.previousElementSibling.innerText || '').trim() : '', 20);
    }
    document.title = 'PROBE|' + JSON.stringify({ err: '', answers: answers, hit: hit, label: label, place: place });
  } catch (e) {
    document.title = 'PROBE|' + JSON.stringify({ err: e.name + ': ' + e.message });
  }
})()
"#;
    let targets: Vec<String> = if links.is_empty() {
        // The sidebar didn't render; use the ready-made URL (the --url one)
        println!("no anchors in the sidebar; probing the current page instead");
        vec![String::new()]
    } else {
        links.clone()
    };

    for (index, link) in targets.iter().take(6).enumerate() {
        let url = if link.is_empty() {
            String::new()
        } else if link.starts_with("http") {
            link.clone()
        } else {
            format!("https://chat.deepseek.com{link}")
        };
        println!("\n== item {}: {}", index + 1, if url.is_empty() { "current page" } else { &url });
        if !url.is_empty() {
            let _ = win.eval(format!("location.href = '{}'", url));
            std::thread::sleep(Duration::from_secs(5));
        }
        let _ = win.eval(probe_js);
        match wait_for("err", 4) {
            None => println!("  no report"),
            Some(value) => {
                let err = value.get("err").and_then(|e| e.as_str()).unwrap_or("");
                if !err.is_empty() {
                    println!("  the injected script errored: {err}");
                    continue;
                }
                let answers = value.get("answers").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("  answer count: {answers}");
                if let Some(place) = value.get("place").and_then(|p| p.as_str()) {
                    if !place.is_empty() {
                        println!("  thinking-block position: {place}");
                    }
                }
                if let Some(labels) = value.get("label").and_then(|l| l.as_array()) {
                    if !labels.is_empty() {
                        println!("  class names of the \"deep thinking\" text: {labels:?}");
                    }
                }
                match value.get("hit").and_then(|h| h.as_array()) {
                    Some(hits) if !hits.is_empty() => {
                        for hit in hits {
                            let cls = hit.get("cls").and_then(|c| c.as_str()).unwrap_or("");
                            let len = hit.get("len").and_then(|l| l.as_u64()).unwrap_or(0);
                            println!("  candidate: len={len:5}  {cls}");
                        }
                    }
                    _ => println!("  no elements with think/reason in their class names"),
                }
            }
        }
    }
}

/// End-to-end trial (--asktest): use the current site registry to **actually
/// send one test question**, streaming the read, and finally dump the real
/// class names of the answer container — for calibrating the three new sites.
/// **Sends one message into the account** (the text is "Please reply only: test123").
fn asktest_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
    with_image: bool,
) {
    use ask_with_snip_lib::webchat;
    let site = webchat::probe_site();
    println!("== end-to-end trial: {} (with image: {with_image}) ==", site.id);
    // Pure red 64x64 PNG (tools/test-red.b64 holds its base64): a real image,
    // so asking "what color" self-verifies whether the image got through
    let tiny_png = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../tools/test-red.b64"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let images = if with_image && !tiny_png.is_empty() { vec![tiny_png] } else { Vec::new() };

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(title) => {
                    if let Some(v) = parse_any(&title) {
                        if v.get(field).is_some() {
                            return Some(v);
                        }
                    }
                }
                Err(_) => {}
            }
        }
        None
    };

    // 1. Wait for readiness (also printing each payload's key fields, so a
    //    ready failure shows where it got stuck)
    let mut ready = false;
    for round in 0..15 {
        if round == 0 {
            // Wrap the first round in a probe: syntax-level death, runtime
            // errors, the title being eaten by the site — all three visible
            let wrapped = format!(
                "window.addEventListener('error', function(e) {{ document.title = 'PROBE|JSERR:' + e.message; }});
try {{ {} }} catch (e) {{ document.title = 'PROBE|CAUGHT:' + String(e && e.message); }}
setTimeout(function() {{ document.title = 'PROBE|title-now=' + String(document.title).slice(0, 24) + '|composer=' + !!document.querySelector('{}'); }}, 400);",
                webchat::collect_js_for(site),
                site.composer.replace(char::from(92), "\\")
            );
            let _ = win.eval(&wrapped);
            std::thread::sleep(Duration::from_millis(900));
        }
        if let Err(err) = win.eval(webchat::collect_js_for(site)) {
            println!("    eval failed: {err}");
        }
        let Ok(rx) = rx.lock() else { break };
        let mut got_ready = false;
        for _ in 0..2 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(title) => {
                    if let Some(v) = parse_any(&title) {
                        println!(
                            "    payload: ready={:?} answers={:?} chars={:?} err={:?} input={:?}",
                            v.get("ready").and_then(|x| x.as_bool()),
                            v.get("answers").and_then(|x| x.as_u64()),
                            v.get("chars").and_then(|x| x.as_u64()),
                            v.get("err").and_then(|x| x.as_str()),
                            v.get("input").and_then(|x| x.as_i64()),
                        );
                        if v.get("ready").and_then(|x| x.as_bool()).unwrap_or(false) {
                            got_ready = true;
                        }
                    } else {
                        println!("    (not our payload: {title:.60})");
                    }
                }
                Err(_) => {}
            }
        }
        drop(rx);
        if got_ready {
            ready = true;
            break;
        }
    }
    if !ready {
        println!("  ✗ composer never appeared ({}) — the composer selector is wrong, or the page isn't ready", site.composer);
        return;
    }
    println!("  ✓ composer ready ({})", site.composer);

    // 2. Type + send (contenteditable goes through CDP real input; the probe
    //    window always uses port 9400)
    let port: u16 = 9400;
    if site.composer_is_textarea {
        let turn = webchat::Turn {
            prompt: if with_image { String::from("Did you receive the image? If yes, reply with the image's dominant color; if not, reply NOIMG") } else { String::from("Please reply with exactly two words: test done") },
            images: images.clone(),
            new_chat: false,
            resume: None,
        };
        if win.eval(webchat::compose_js(site, &turn)).is_err() {
            println!("  x compose injection failed");
            return;
        }
        if with_image {
            if let Some(v) = wait_for("composed", 4) {
                println!(
                    "  compose report: img path={:?}",
                    v.get("img").and_then(|x| x.as_str()).unwrap_or("(none)")
                );
            }
        }
        std::thread::sleep(Duration::from_millis(if with_image { 3000 } else { 1200 }));
        if win.eval(webchat::send_js(site)).is_err() {
            println!("  x send injection failed");
            return;
        }
    } else {
        let turn = webchat::Turn {
            prompt: if with_image { String::from("Did you receive the image? If yes, reply with the image's dominant color; if not, reply NOIMG") } else { String::from("Please reply with exactly two words: test done") },
            images: images.clone(),
            new_chat: false,
            resume: None,
        };
        if win.eval(webchat::compose_js(site, &turn)).is_err() {
            println!("  x compose injection failed");
            return;
        }
        std::thread::sleep(Duration::from_millis(if with_image { 3000 } else { 300 }));
        if with_image {
            if let Some(v) = wait_for("composed", 4) {
                println!(
                    "  compose report: img path={:?}",
                    v.get("img").and_then(|x| x.as_str()).unwrap_or("(none)")
                );
            }
            // Paste diagnose: number of imgs near the composer (attachment
            // previews push this number up)
            // (附件/上传 match the Chinese aria-labels — keep for matching.)
            let attach_diag = format!(
                r#"
(() => {{
  const c = document.querySelector('{}');
  const root = c && (c.closest('form') || c.parentElement);
  let n = -1;
  if (root) n = root.querySelectorAll('img').length;
  document.title = 'PROBE|' + JSON.stringify({{
    imgs: n,
    allImg: document.querySelectorAll('img').length,
    inputs: [...document.querySelectorAll('input[type=file]')].map((el) => ((el.id || 'no-id') + '|' + (el.accept || 'no-accept') + '|' + (el.className || '').slice(0, 30)).slice(0, 90)),
    att: [...document.querySelectorAll('[data-testid],[class*=attach],[class*=preview],[class*=upload],[aria-label*=附件],[aria-label*=Attach],[aria-label*=上传]')]
      .slice(0, 10).map((el) => (el.tagName + '|' + (typeof el.className === 'string' ? el.className.slice(0, 40) : '') + '|' + (el.getAttribute('aria-label') || '')).slice(0, 90)),
  }});
}})()
"#,
                site.composer
            );
            let _ = win.eval(&attach_diag);
            for _ in 0..4 {
                let Ok(rx) = rx.lock() else { break };
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(title) => {
                        if let Some(v) = parse_any(&title) {
                            println!(
                                "  paste diagnose: imgs near composer={:?} imgs on page={:?} file inputs={:?} attachment-related={:?}",
                                v.get("imgs").cloned().unwrap_or(serde_json::json!(-9)),
                                v.get("allImg").cloned().unwrap_or(serde_json::json!(-9)),
                                v.get("inputs").cloned().unwrap_or(serde_json::json!([])),
                                v.get("att").cloned().unwrap_or(serde_json::json!([]))
                            );
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        if let Err(err) = webchat::cdp_deliver(site, win, port, &turn, &Default::default()) {
            println!("  x CDP delivery (paste/type/send) failed: {err}");
            return;
        }
    }
    std::thread::sleep(Duration::from_millis(800));
    // Post-send diagnose: is there text in the composer, was the send button
    // found, is it disabled
    let diag = format!(
        r#"
(() => {{
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const composer = document.querySelector('{}');
  const text = composer ? ((composer.textContent || '') + '|' + (composer.value || '')) : '(none)';
  const sends = [...document.querySelectorAll('{}')].map((b) => ({{
    disabled: b.disabled === true || b.getAttribute('aria-disabled') === 'true',
    vis: b.getBoundingClientRect && b.getBoundingClientRect().width > 0,
    aria: cut(b.getAttribute('aria-label') || '', 24),
    cls: cut(typeof b.className === 'string' ? b.className : '', 50),
  }}));
  document.title = 'PROBE|' + JSON.stringify({{ err: '', text: cut(text, 60), sends: sends }});
}})()
"#,
        site.composer, site.send
    );
    let _ = win.eval(&diag);
    for _ in 0..4 {
        let Ok(rx) = rx.lock() else { break };
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(title) => {
                if let Some(v) = parse_any(&title) {
                    println!(
                        "  post-send diagnose: composer={:?} send buttons={:?}",
                        v.get("text").and_then(|x| x.as_str()).unwrap_or(""),
                        v.get("sends").cloned().unwrap_or(serde_json::json!([]))
                    );
                    break;
                }
            }
            Err(_) => break,
        }
    }
    // Wait 2 seconds after typing, then dump the button panorama again: the
    // send button usually only renders after input
    std::thread::sleep(Duration::from_secs(2));
    let buttons_dump = r#"
(() => {
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const vis = (el) => el.getBoundingClientRect && el.getBoundingClientRect().width > 0;
  const composer2 = document.querySelector('div.ql-editor.textarea, #prompt-textarea, div[contenteditable="true"].ProseMirror');
  const near = composer2 ? [...composer2.closest('form, rich-textarea, div')?.querySelectorAll('button') || []]
    .filter(vis).map((b) => cut(b.getAttribute('aria-label') || '', 30) + '|' + cut(typeof b.className === 'string' ? b.className : '', 30)) : [];
  document.title = 'PROBE|' + JSON.stringify({ err: '', ctext: cut(composer2 ? (composer2.textContent || '') : '', 50), near: near.slice(0, 12) });
})()
"#;
    let _ = win.eval(buttons_dump);
    for _ in 0..4 {
        let Ok(rx) = rx.lock() else { break };
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(title) => {
                if let Some(v) = parse_any(&title) {
                    println!(
                        "  after 2s: composer text={:?} buttons near the composer={:?}",
                        v.get("ctext").and_then(|x| x.as_str()).unwrap_or(""),
                        v.get("near").cloned().unwrap_or(serde_json::json!([]))
                    );
                    break;
                }
            }
            Err(_) => break,
        }
    }
    println!("  send clicked ({}), watching for 60 seconds...", site.send);

    // 3. Read for 60 seconds
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut chars = 0u64;
    let mut answers = 0u64;
    while std::time::Instant::now() < deadline {
        let _ = win.eval(webchat::collect_js_for(site));
        if let Some(v) = wait_for("chars", 2) {
            answers = v.get("answers").and_then(|x| x.as_u64()).unwrap_or(answers);
            let c = v.get("chars").and_then(|x| x.as_u64()).unwrap_or(chars);
            if c != chars {
                chars = c;
                println!("    body {chars} chars / {answers} answers");
            }
        }
        if chars > 0 && answers > 0 {
            // Settled; leave early
            std::thread::sleep(Duration::from_millis(2500));
            break;
        }
    }
    if chars == 0 {
        println!("  ✗ not a single char in 60 seconds — the answer-container selector ({}) is probably wrong; dumping real nodes below", site.answer);
    } else {
        println!("  ✓ read {chars} chars of body");
    }

    // 4. Dump the real class names of answer-node candidates
    let dump = r#"
(() => {
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const all = [...document.querySelectorAll('model-response, message-content, .markdown, [data-message-author-role], [class*=claude-message], [class*=message-content], [class*=assistant], [class*=response]')]
    .map((el) => ({
      tag: el.tagName,
      cls: cut(typeof el.className === 'string' ? el.className : '', 90),
      len: (el.textContent || '').length,
      head: cut((el.textContent || '').trim(), 40),
    }));
  const body = cut((document.body.innerText || '').replace(/\s+/g, ' ').trim(), 300);
  document.title = 'PROBE|' + JSON.stringify({ err: '', all: all, body: body });
})()
"#;
    let _ = win.eval(dump);
    for _ in 0..8 {
        let Ok(rx) = rx.lock() else { return };
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(title) => {
                if let Some(v) = parse_any(&title) {
                    if let Some(items) = v.get("all").and_then(|x| x.as_array()) {
                        println!("  answer-node candidates:");
                        for it in items {
                            println!("    <{}> cls={} len={} head={:?}",
                                it.get("tag").and_then(|x| x.as_str()).unwrap_or("?"),
                                it.get("cls").and_then(|x| x.as_str()).unwrap_or(""),
                                it.get("len").and_then(|x| x.as_u64()).unwrap_or(0),
                                it.get("head").and_then(|x| x.as_str()).unwrap_or(""));
                        }
                    }
                    println!("  tail of the page text: {}", v.get("body").and_then(|x| x.as_str()).unwrap_or(""));
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Step-by-step probe (--steps): run the driver's three injected pieces
/// (converter / collect / ready check) separately, each writing one PROBE|
/// payload. When a site "returns zero payloads", this shows which step died.
fn steps_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    use ask_with_snip_lib::webchat;
    let site = webchat::probe_site();
    let js = format!(
        "document.title = 'PROBE|step1';
{}
document.title = 'PROBE|step2-md-ok';
try {{ {} }} catch (e) {{ document.title = 'PROBE|collect-threw:' + String(e && e.message); }}
document.title = 'PROBE|step3-composer=' + !!document.querySelector('{}');",
        webchat::markdown_js(),
        webchat::collect_js_for(site),
        site.composer.replace(char::from(92), "\\").replace(char::from(39), "'")
    );
    println!("== step-by-step probe ({}) ==", site.id);
    if let Err(err) = win.eval(&js) {
        println!("  eval itself failed: {err}");
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let got = rx.lock().ok().and_then(|rx| rx.recv_timeout(Duration::from_millis(500)).ok());
        let Some(title) = got else { continue };
        let Some(value) = parse_any(&title) else { continue };
        println!("  received: {}", serde_json::to_string(&value).unwrap_or_default());
        if title.contains("step3") || title.contains("threw") {
            return;
        }
    }
    println!("  not one step payload in 10 seconds (not even step1 = eval never executed at all)");
}

/// Generic node reconnaissance (--dom): dump every candidate
/// composer/send-button/answer container on the page. For calibrating
/// selectors on the three new sites — it knows nothing of deepseek's specific
/// structure and runs on any site.
fn dom_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    const DOM_JS: &str = r#"
(() => {
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const vis = (el) => el.getBoundingClientRect && el.getBoundingClientRect().width > 0;
  const editables = [...document.querySelectorAll('textarea, [contenteditable="true"]')]
    .map((el) => ({
      tag: el.tagName,
      cls: cut(typeof el.className === 'string' ? el.className : '', 60),
      id: el.id || '',
      vis: vis(el),
    }));
  const buttons = [...document.querySelectorAll('button, [role=button]')]
    .filter((el) => vis(el))
    .map((el) => ({
      aria: cut(el.getAttribute('aria-label') || '', 30),
      label: cut(el.getAttribute('data-testid') || '', 30),
      text: cut((el.innerText || '').trim(), 16),
      cls: cut(typeof el.className === 'string' ? el.className : '', 40),
    }))
    .filter((b) => b.aria || b.label || b.text);
  const markdowns = [...document.querySelectorAll('.markdown, [class*=markdown], model-response, message-content, [data-message-author-role], [class*=message-content], [class*=claude-message]')]
    .map((el) => ({ tag: el.tagName, cls: cut(typeof el.className === 'string' ? el.className : '', 60), len: (el.textContent || '').length }));
  document.title = 'PROBE|' + JSON.stringify({
    err: '',
    url: cut(location.href, 80),
    title: cut(document.title, 40),
    editables: editables.slice(0, 8),
    buttons: buttons.slice(0, 25),
    markdowns: markdowns.slice(0, 10),
  });
})()
"#;
    println!("== generic node reconnaissance (--dom) ==");
    if let Err(err) = win.eval(DOM_JS) {
        println!("  injection failed: {err}");
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    while std::time::Instant::now() < deadline {
        let got = rx.lock().ok().and_then(|rx| rx.recv_timeout(Duration::from_millis(500)).ok());
        let Some(title) = got else { continue };
        let Some(value) = parse_any(&title) else { continue };
        println!("  url: {}", value.get("url").and_then(|v| v.as_str()).unwrap_or("?"));
        println!("  site title: {}", value.get("title").and_then(|v| v.as_str()).unwrap_or("?"));
        if let Some(items) = value.get("editables").and_then(|v| v.as_array()) {
            println!("  composer candidates:");
            for it in items {
                println!("    <{}> id={} cls={} visible={}",
                    it.get("tag").and_then(|v| v.as_str()).unwrap_or("?"),
                    it.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    it.get("cls").and_then(|v| v.as_str()).unwrap_or(""),
                    it.get("vis").and_then(|v| v.as_bool()).unwrap_or(false));
            }
        }
        if let Some(items) = value.get("buttons").and_then(|v| v.as_array()) {
            println!("  button candidates (first 25 identified):");
            for it in items {
                println!("    aria={} testid={} text={} cls={}",
                    it.get("aria").and_then(|v| v.as_str()).unwrap_or(""),
                    it.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                    it.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                    it.get("cls").and_then(|v| v.as_str()).unwrap_or(""));
            }
        }
        if let Some(items) = value.get("markdowns").and_then(|v| v.as_array()) {
            println!("  answer-container candidates:");
            for it in items {
                println!("    <{}> cls={} len={}",
                    it.get("tag").and_then(|v| v.as_str()).unwrap_or("?"),
                    it.get("cls").and_then(|v| v.as_str()).unwrap_or(""),
                    it.get("len").and_then(|v| v.as_u64()).unwrap_or(0));
            }
        }
        return;
    }
    println!("  no report arrived");
}

/// Read several rounds on a real conversation using the **driver's own**
/// `collect_js`, to see whether the body and the chain of thought come out.
///
/// Why it exists: the chain of thought used to be guessed from "whatever text
/// remained outside the answer"; now it is identified via the
/// `.ds-think-content` container (found by the `--think` probe). Whether the
/// guess is right, only the real page can answer.
///
/// **Read-only**: no pasting, no typing, no sending — it just opens an
/// existing conversation (from `--url`, or the first in the sidebar) and keeps
/// reading. Note there is no "record a baseline before sending" step here, so
/// the first round reports the existing answer and thinking in full — exactly
/// what we want to see (in a real round the baseline is recorded before
/// asking).
fn collect_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    use ask_with_snip_lib::webchat;

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(title) => {
                    if let Some(value) = parse_any(&title) {
                        if value.get(field).is_some() {
                            return Some(value);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    };

    println!("== waiting for the page to be ready ==");
    let mut ready = false;
    for _ in 0..12 {
        let _ = win.eval(webchat::collect_js());
        if wait_for("ready", 2)
            .and_then(|v| v.get("ready").and_then(|b| b.as_bool()))
            .unwrap_or(false)
        {
            ready = true;
            break;
        }
    }
    if !ready {
        println!("  page not ready (maybe not logged in)");
        return;
    }
    println!("  ready");

    // The sidebar-expand step lives in --think; borrow its conclusion here:
    // expand the top-left corner first, otherwise the conversations aren't visible
    let open_sidebar = r#"
(() => {
  const vis = (el) => el.getBoundingClientRect().width > 0 && el.getBoundingClientRect().height > 0;
  const corner = [...document.querySelectorAll('button, [role=button]')]
    .filter(vis)
    .filter((el) => { const r = el.getBoundingClientRect(); return r.left < 90 && r.top < 90; });
  for (const el of corner.slice(0, 4)) el.click();
  document.title = 'PROBE|' + JSON.stringify({ err: '', n: document.querySelectorAll('a[href*="/a/chat/s/"]').length });
})()
"#;
    let _ = win.eval(open_sidebar);
    let links = wait_for("n", 3).and_then(|v| v.get("n").and_then(|x| x.as_u64())).unwrap_or(0);
    println!("  the sidebar holds {links} conversation(s)");

    if std::env::args().any(|a| a == "--here") {
        println!("  using the current page (--here)");
    } else {
        let open_first = r#"
(() => {
  const vis = (el) => el.getBoundingClientRect().width > 0;
  const links = [...document.querySelectorAll('a[href*="/a/chat/s/"]')].filter(vis);
  if (links.length) links[0].click();
})()
"#;
        let _ = win.eval(open_first);
        std::thread::sleep(Duration::from_secs(6));
    }

    println!("== reading 10 rounds: body (d) and chain of thought (dr) ==");
    println!("(note: the probe's own waits also pull payloads from the title channel; unlike the driver, it");
    println!("  doesn't collect from a single place end-to-end, so the char counts here may run a few chars short");
    println!("  of what the page reported. The missing bit is exactly \"one payload lost\" — the final re-read");
    println!("  step detects it and re-sends the whole thing; see below.)");
    let mut text = String::new();
    let mut reasoning = String::new();
    for round in 1..=10 {
        if win.eval(webchat::collect_js()).is_err() {
            println!("  injection failed");
            return;
        }
        if let Some(value) = wait_for("d", 2) {
            let d = value.get("d").and_then(|v| v.as_str()).unwrap_or("");
            let dr = value.get("dr").and_then(|v| v.as_str()).unwrap_or("");
            let answers = value.get("answers").and_then(|v| v.as_u64()).unwrap_or(0);
            if !d.is_empty() {
                text.push_str(d);
            }
            if !dr.is_empty() {
                reasoning.push_str(dr);
            }
            if !d.is_empty() || !dr.is_empty() {
                println!(
                    "  round {round:2}: answers={answers} page body={} chars, body +{} chars / chain of thought +{} chars / rst={}",
                    value.get("chars").and_then(|v| v.as_u64()).unwrap_or(0),
                    d.chars().count(),
                    dr.chars().count(),
                    value.get("rst").and_then(|v| v.as_bool()).unwrap_or(false),
                );
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }

    let head = |s: &str, n: usize| -> String { s.chars().take(n).collect::<String>() };
    println!();
    println!("--- body (first 200 chars) ---");
    println!("{}", head(&text, 200));
    println!("--- chain of thought (first 200 chars, {} total) ---", reasoning.chars().count());
    println!("{}", head(&reasoning, 200));
    if reasoning.is_empty() {
        println!("(no chain of thought read: this conversation may simply not have had deep thinking on)");
    }

    // Final re-read: first deliberately report a wrong length — the page
    // should re-send the whole body; then the right one — it should push
    // nothing. This step guarantees "the answer stored in the panel matches
    // the page exactly" and deserves one verification on a real page.
    println!();
    println!("== final re-read (read-only) ==");
    // Align with the page's String.length: that side counts UTF-16 code units
    let have = text.encode_utf16().count();
    let have_reasoning = reasoning.encode_utf16().count();
    // Mirror the driver's way: one re-read first, then keep nudging while
    // collecting (the page only pushes the next chunk when evaluated)
    let drain_after_flush = |report: usize| -> (String, bool) {
        if win.eval(webchat::collect_js_flush(webchat::probe_site(), report, have_reasoning, webchat::Baseline::default())).is_err() {
            return (String::new(), false);
        }
        let mut got = String::new();
        let mut rst_seen = false;
        for _ in 0..60 {
            let Some(value) = wait_for("d", 2) else { break };
            let Ok(sig) = webchat::read_signal(&value) else { break };
            if sig.rst {
                rst_seen = true;
            }
            got.push_str(&sig.text);
            if sig.more || sig.rmore {
                let _ = win.eval(webchat::collect_js());
            } else {
                break;
            }
        }
        (got, rst_seen)
    };
    // Write the whole body to a file: only a character-by-character comparison
    // with the stored record shows where "on the page" and "stored" differ
    let dump_path = std::env::temp_dir().join("snapi_page_answer.txt");
    for (label, report) in [("length mismatch", have + 3), ("length match", have)] {
        let (resent, rst_seen) = drain_after_flush(report);
        if label == "length mismatch" {
            let _ = std::fs::write(&dump_path, &resent);
            println!("  (the full body was written to {})", dump_path.display());
        }
        println!(
            "  {label} (reported {report}): re-sent {} chars, {}",
            resent.encode_utf16().count(),
            if rst_seen {
                "went through full replacement"
            } else if resent.is_empty() {
                "pushed nothing (this is correct)"
            } else {
                "pushed increments (wrong; should have been a full replacement)"
            }
        );
    }
}

/// What the "deep thinking" toggle looks like: read the current state -> click
/// -> read again -> click back to restore.
///
/// The driver must turn deep thinking on for the user (otherwise the model
/// doesn't think at all in new conversations and there is no chain of thought
/// to read — confirmed with the conversation the user had just tested: driver-
/// opened chats have no thinking blocks, manually-chatted ones do). But it
/// **must not click blindly**: if the toggle was already on, clicking turns it
/// off. So first find which DOM attribute differs between on and off; the
/// driver judges by that and only clicks when it should be on.
fn toggle_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    // Do it all in one eval: snapshot -> click -> wait -> snapshot -> click to
    // restore. Each step carries a nonce (writing the identical string into
    // the title twice doesn't fire the event the second time — bitten on this
    // probe's very first run), and the whole thing is wrapped in try/catch to
    // guarantee a report.
    const TOGGLE_JS: &str = r#"
(async () => {
  const report = (o) => {
    try { document.title = 'PROBE|' + JSON.stringify(Object.assign({ n: Date.now() }, o)); } catch (e) {}
  };
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const vis = (el) => el.getBoundingClientRect().width > 0;
  // '深度思考' matches the real page's Chinese label — keep for matching.
  const find = () => [...document.querySelectorAll('div, span')]
    .filter((el) => vis(el) && (el.innerText || '').trim() === '深度思考');
  const chain = (leaf) => {
    const out = [];
    let el = leaf;
    for (let i = 0; i < 7 && el; i += 1) {
      out.push(el.tagName.toLowerCase() + '|' + cut(typeof el.className === 'string' ? el.className : '', 120));
      el = el.parentElement;
    }
    return out;
  };
  try {
    const leaves = find();
    if (!leaves.length) { report({ err: '', step: 'notfound' }); return; }
    const leaf = leaves[0];
    report({ err: '', step: 'before', chain: chain(leaf) });
    leaf.click();
    await new Promise((r) => setTimeout(r, 700));
    const after = find();
    report({ err: '', step: 'clicked', count: after.length, chain: after.length ? chain(after[0]) : [] });
    if (after.length) {
      after[0].click();
      await new Promise((r) => setTimeout(r, 700));
      const restored = find();
      report({ err: '', step: 'restored', chain: restored.length ? chain(restored[0]) : [] });
    }
  } catch (e) {
    report({ err: (e && e.message) || String(e) });
  }
})()
"#;
    let _ = win.eval(TOGGLE_JS);
    for _ in 0..6 {
        let Ok(rx) = rx.lock() else { return };
        match rx.recv_timeout(Duration::from_secs(4)) {
            Ok(title) => {
                if let Some(value) = parse_any(&title) {
                    println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
                    if value.get("step").and_then(|s| s.as_str()) == Some("restored") {
                        return;
                    }
                    if value.get("err").and_then(|e| e.as_str()).map(|s| !s.is_empty()).unwrap_or(false) {
                        return;
                    }
                    if value.get("step").and_then(|s| s.as_str()) == Some("notfound") {
                        return;
                    }
                }
            }
            Err(_) => {
                println!("  timed out waiting for the next step's report");
                return;
            }
        }
    }
}

/// Check whether the "edit" and "regenerate" buttons exist and can be clicked,
/// and whether clicking "edit" really puts that text into the composer.
///
/// Why this needs separate verification: the panel's two buttons (edit
/// question / regenerate) work over the web UI by **clicking the page's own
/// buttons** for you, and those buttons' class names are all hashed — they can
/// only be recognized by text. Recognizing by text requires confirming the
/// text is really there first.
///
/// **Clicking "edit" is safe**: it only enters the page's own edit state (the
/// original text goes back into the composer), sends nothing, and vanishes
/// when the window closes. **"Regenerate" is NOT clicked** — it really makes
/// the site generate again and burns quota; only the user can press that from
/// the panel.
fn actions_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    use ask_with_snip_lib::webchat;

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(title) => {
                    if let Some(value) = parse_any(&title) {
                        if value.get(field).is_some() {
                            return Some(value);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    };

    let mut ready = false;
    for _ in 0..12 {
        let _ = win.eval(webchat::collect_js());
        if wait_for("ready", 2)
            .and_then(|v| v.get("ready").and_then(|b| b.as_bool()))
            .unwrap_or(false)
        {
            ready = true;
            break;
        }
    }
    if !ready {
        println!("  page not ready (maybe not logged in)");
        return;
    }

    // Expand the sidebar first, then enter a conversation (in a narrow window
    // the sidebar isn't rendered at all)
    let open_sidebar = r#"
(() => {
  const vis = (el) => el.getBoundingClientRect().width > 0 && el.getBoundingClientRect().height > 0;
  const corner = [...document.querySelectorAll('button, [role=button]')]
    .filter(vis)
    .filter((el) => { const r = el.getBoundingClientRect(); return r.left < 90 && r.top < 90; });
  for (const el of corner.slice(0, 4)) el.click();
  document.title = 'PROBE|' + JSON.stringify({ err: '', n: document.querySelectorAll('a[href*="/a/chat/s/"]').length });
})()
"#;
    let _ = win.eval(open_sidebar);
    let links = wait_for("n", 3).and_then(|v| v.get("n").and_then(|x| x.as_u64())).unwrap_or(0);
    println!("  the sidebar holds {links} conversation(s)");

    if !std::env::args().any(|a| a == "--here") {
        let open_first = r#"
(() => {
  const vis = (el) => el.getBoundingClientRect().width > 0;
  const links = [...document.querySelectorAll('a[href*="/a/chat/s/"]')].filter(vis);
  if (links.length) links[0].click();
})()
"#;
        let _ = win.eval(open_first);
        std::thread::sleep(Duration::from_secs(6));
    }

    // Step one: do these buttons exist at all, what do they look like
    //
    // Measured: **they cannot be found by text** (neither "edit" nor
    // "regenerate" matched anything) — the site's buttons are icon buttons
    // with no text and not even an aria-label. So the icons themselves must be
    // examined: print each .ds-button's <use href> or first bytes of its path,
    // plus which message row it sits on.
    let find_js = r#"
(() => {
  const vis = (n) => { const r = n.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  // Hover the last message: the toolbar usually only shows on hover (CSS
  // :hover can't be synthesized, but React's mouseover delegation receives it)
  const all = [...document.querySelectorAll('.ds-message')];
  const last = all[all.length - 1];
  if (last) {
    for (const type of ['mouseover', 'mouseenter', 'mousemove']) {
      last.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, view: window }));
    }
  }
  // The payload must stay short: window titles have a length limit, overflow
  // kills the whole payload (bitten once here)
  const icon = (el) => {
    const svg = el.querySelector('svg');
    if (!svg) return '(no svg)';
    const use = svg.querySelector('use');
    if (use) return 'use=' + cut(use.getAttribute('href') || use.getAttribute('xlink:href') || '', 24);
    const path = svg.querySelector('path');
    return 'path=' + cut(path ? path.getAttribute('d') || '' : '', 24);
  };
  const msgs = all.map((m) => {
    const r = m.getBoundingClientRect();
    return {
      top: Math.round(r.top),
      bottom: Math.round(r.bottom),
      user: !m.querySelector('.ds-assistant-message-main-content'),
    };
  });
  const buttons = [...document.querySelectorAll('.ds-button')]
    .filter((el) => !el.closest('form'))
    .slice(0, 10)
    .map((el) => {
      const r = el.getBoundingClientRect();
      return {
        top: Math.round(r.top + r.height / 2),
        vis: vis(el),
        svg: icon(el),
      };
    });
  // Any recognizable markers: print all attributes of each button and its two
  // ancestor levels (many design systems hang data-* attributes)
  const attrs = (el) => [...el.attributes].map((a) => a.name + '=' + cut(a.value, 14)).join(' ');
  const lastMsg = all[all.length - 1];
  const around = lastMsg && lastMsg.parentElement && lastMsg.parentElement.parentElement
    ? [...lastMsg.parentElement.parentElement.querySelectorAll('.ds-button')].slice(0, 8).map((el) => ({
        vis: vis(el),
        at: attrs(el),
        up: cut(String(el.parentElement ? el.parentElement.className || '' : ''), 30),
        up2: cut(String(el.parentElement && el.parentElement.parentElement ? el.parentElement.parentElement.className || '' : ''), 30),
      }))
    : [];
  document.title = 'PROBE|' + JSON.stringify({
    err: '',
    msgs: msgs.length,
    buttons: buttons.map((b) => b.svg + '|' + b.top),
    around: around,
  });
})()
"#;
    let _ = win.eval(find_js);
    println!("== after hovering: which buttons/icons are in the messages ==");
    match wait_for("err", 3) {
        None => println!("  no report"),
        Some(value) => {
            println!("  message count: {}", value.get("msgs").and_then(|v| v.as_array()).map(|m| m.len()).unwrap_or(0));
            if let Some(items) = value.get("msgs").and_then(|v| v.as_array()) {
                for (i, m) in items.iter().enumerate() {
                    println!(
                        "  message {i}: {}({}-{})",
                        if m.get("user").and_then(|v| v.as_bool()).unwrap_or(false) { "from me" } else { "answer" },
                        m.get("top").and_then(|v| v.as_i64()).unwrap_or(0),
                        m.get("bottom").and_then(|v| v.as_i64()).unwrap_or(0),
                    );
                }
            }
            match value.get("buttons").and_then(|v| v.as_array()) {
                Some(items) if !items.is_empty() => {
                    for item in items {
                        println!(
                            "  y={} vis={} {}",
                            item.get("top").and_then(|v| v.as_i64()).unwrap_or(0),
                            item.get("vis").and_then(|v| v.as_bool()).unwrap_or(false),
                            item.get("svg").and_then(|v| v.as_str()).unwrap_or(""),
                        );
                    }
                }
                _ => println!("  (no .ds-button found)"),
            }
            match value.get("around").and_then(|v| v.as_array()) {
                Some(items) if !items.is_empty() => {
                    println!("  buttons around the last message (including hidden ones):");
                    for item in items {
                        println!(
                            "    vis={} parent={} / {} :: {}",
                            item.get("vis").and_then(|v| v.as_bool()).unwrap_or(false),
                            item.get("up").and_then(|v| v.as_str()).unwrap_or(""),
                            item.get("up2").and_then(|v| v.as_str()).unwrap_or(""),
                            item.get("at").and_then(|v| v.as_str()).unwrap_or(""),
                        );
                    }
                }
                _ => println!("  no .ds-button around the last message"),
            }
        }
    }

    // Step two: click "edit" and see whether that text really entered the
    // composer (not sent; gone when the window closes)
    // '编辑' matches the real page's Chinese edit button — keep for matching.
    println!("== clicking \"edit\", then inspecting the composer (no send) ==");
    let click_edit = r#"
(() => {
  const vis = (n) => { const r = n.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const hits = [...document.querySelectorAll('div, button, span, a')]
    .filter((n) => (n.innerText || '').trim() === '编辑' && vis(n));
  if (!hits.length) {
    document.title = 'PROBE|' + JSON.stringify({ err: '', note: 'no "edit" button on the page' });
    return;
  }
  const hit = hits[hits.length - 1];
  (hit.closest('button, [role=button], a') || hit).click();
  document.title = 'PROBE|' + JSON.stringify({ err: '', note: 'clicked the last "edit"' });
})()
"#;
    let _ = win.eval(click_edit);
    match wait_for("note", 3) {
        None => println!("  no report"),
        Some(value) => println!("  {}", value.get("note").and_then(|v| v.as_str()).unwrap_or("")),
    }
    std::thread::sleep(Duration::from_secs(2));

    let read_box = r#"
(() => {
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const areas = [...document.querySelectorAll('textarea')];
  // 发送消息/给 DeepSeek/取消/保存/发送 match the real page's Chinese text — keep for matching.
  const box = areas.find((el) => /发送消息|给 DeepSeek/.test(el.placeholder || '')) || areas[0] || null;
  const texts = [...document.querySelectorAll('div, button, span')]
    .filter((n) => ['取消', '保存', '发送'].includes((n.innerText || '').trim()))
    .map((n) => (n.innerText || '').trim());
  document.title = 'PROBE|' + JSON.stringify({
    err: '',
    areas: areas.length,
    placeholder: box ? cut(box.placeholder || '', 24) : '(no textarea)',
    len: box ? (box.value || '').length : -1,
    head: box ? cut(box.value || '', 40) : '',
    buttons: texts.slice(0, 6),
  });
})()
"#;
    let _ = win.eval(read_box);
    match wait_for("err", 3) {
        None => println!("  no report"),
        Some(value) => {
            println!("  textarea count: {}", value.get("areas").and_then(|v| v.as_u64()).unwrap_or(0));
            println!(
                "  composer placeholder=\"{}\" currently holds {} chars",
                value.get("placeholder").and_then(|v| v.as_str()).unwrap_or(""),
                value.get("len").and_then(|v| v.as_i64()).unwrap_or(-1)
            );
            println!("  starts with: {}", value.get("head").and_then(|v| v.as_str()).unwrap_or(""));
            if let Some(btns) = value.get("buttons").and_then(|b| b.as_array()) {
                let list: Vec<&str> = btns.iter().filter_map(|v| v.as_str()).collect();
                println!("  nearby buttons: {}", if list.is_empty() { "(no cancel/save/send seen)".to_string() } else { list.join(", ") });
            }
        }
    }
}

/// Reproduce "regenerate": take the last round of a history record (the
/// question + its image) and, exactly like the panel does, **really start a
/// new conversation and ask again**, printing every step and every increment
/// read.
///
/// Why a separate mode: the panel's button runs "new chat -> paste -> type ->
/// send -> read back", and users reported "no answer ever came" while the site
/// had already finished generating. This mode exists to see, in exactly that
/// situation, which step failed: it wasn't sent, the page didn't render, or
/// the reading side broke.
///
/// **It really sends messages into the account** (identical to pressing
/// "regenerate"), so it only runs when explicitly requested.
fn regen_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
    record: &str,
) {
    use ask_with_snip_lib::webchat;

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(title) => {
                    if let Some(value) = parse_any(&title) {
                        if value.get(field).is_some() {
                            return Some(value);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    };

    // If --prompt is given, use that plain-text question directly (better not
    // to disturb the account's history); otherwise read the last turn of the
    // record: the question + its image (the screenshot originally selected)
    let override_prompt = std::env::args()
        .position(|a| a == "--prompt")
        .and_then(|i| std::env::args().nth(i + 1));
    let (prompt, image) = if let Some(text) = override_prompt {
        println!("(--prompt override: sending only one plain-text question, no image)");
        (text, String::new())
    } else {
        let raw = match std::fs::read_to_string(record) {
            Ok(raw) => raw,
            Err(err) => {
                println!("cannot read the record {record}: {err}");
                return;
            }
        };
        let conv: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(err) => {
                println!("the record is not valid JSON: {err}");
                return;
            }
        };
        let last = conv
            .get("turns")
            .and_then(|t| t.as_array())
            .and_then(|t| t.last())
            .cloned();
        let Some(last) = last else {
            println!("the record has no turns");
            return;
        };
        (
            last.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            last.get("image_b64").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        )
    };
    println!("record: {record}");
    println!(
        "question to re-ask: {} ({} chars), image {} KB",
        prompt,
        prompt.chars().count(),
        image.len() / 1024
    );

    let turn = webchat::Turn {
        prompt: prompt.clone(),
        images: if image.is_empty() { Vec::new() } else { vec![image.to_string()] },
        // Same as the panel's "regenerate": start a new conversation (the web
        // UI cannot edit history)
        new_chat: true,
        resume: None,
    };

    let clock = std::time::Instant::now();
    let at = |label: &str| println!("[{:>6.1}s] {label}", clock.elapsed().as_secs_f32());

    println!("== 1. waiting for the page to be ready ==");
    let mut ready = false;
    for _ in 0..20 {
        let _ = win.eval(webchat::collect_js());
        if wait_for("ready", 2)
            .and_then(|v| v.get("ready").and_then(|b| b.as_bool()))
            .unwrap_or(false)
        {
            ready = true;
            break;
        }
    }
    if !ready {
        println!("  page not ready (maybe not logged in)");
        return;
    }
    at("ready");

    println!("== 2. starting a new conversation ==");
    let _ = win.eval(webchat::new_chat_js(webchat::probe_site()));
    std::thread::sleep(Duration::from_millis(600));
    let mut reloaded = false;
    while let Ok(title) = rx.lock().map(|rx| rx.try_recv()) {
        match title {
            Ok(title) => {
                if let Some(value) = parse_any(&title) {
                    if value.get("newchat").and_then(|v| v.as_str()) == Some("fallback") {
                        reloaded = true;
                    }
                }
            }
            Err(_) => break,
        }
    }
    at(if reloaded {
        "button not clickable, fell back to the home page (page reloads)"
    } else {
        "clicked \"new chat\""
    });
    if reloaded {
        for _ in 0..20 {
            let _ = win.eval(webchat::collect_js());
            if wait_for("ready", 2)
                .and_then(|v| v.get("ready").and_then(|b| b.as_bool()))
                .unwrap_or(false)
            {
                break;
            }
        }
        at("ready again");
    }

    println!("== 3. turning \"deep thinking\" on (same approach as a new chat) ==");
    let _ = win.eval(webchat::think_on_js(webchat::probe_site()));
    std::thread::sleep(Duration::from_millis(1200));
    at("toggle clicked");

    println!("== 4. paste + type + send ==");
    if let Err(err) = win.eval(webchat::compose_js(webchat::probe_site(), &turn)) {
        println!("  injection failed: {err}");
        return;
    }
    std::thread::sleep(Duration::from_millis(1600));
    if let Some(value) = wait_for("input", 2) {
        println!(
            "  {} chars in the composer, {} image(s) in the container",
            value.get("input").and_then(|v| v.as_i64()).unwrap_or(-1),
            value.get("imgs").and_then(|v| v.as_i64()).unwrap_or(-1)
        );
    }
    if let Err(err) = win.eval(webchat::send_js(webchat::probe_site())) {
        println!("  clicking send failed: {err}");
        return;
    }
    at("send clicked");

    println!("== 5. reading every 250ms, watching for up to 150 seconds ==");
    let deadline = std::time::Instant::now() + Duration::from_secs(150);
    let mut answer = String::new();
    let mut reasoning = String::new();
    let mut stable = 0u32;
    let mut last_report = std::time::Instant::now();
    while std::time::Instant::now() < deadline {
        if win.eval(webchat::collect_js()).is_err() {
            println!("  injection failed");
            return;
        }
        let mut got = false;
        while let Some(value) = wait_for("d", 1) {
            let Ok(sig) = webchat::read_signal(&value) else { break };
            if !sig.text.is_empty() || !sig.reasoning.is_empty() {
                got = true;
            }
            answer.push_str(&sig.text);
            reasoning.push_str(&sig.reasoning);
            if sig.more || sig.rmore {
                let _ = win.eval(webchat::collect_js());
            } else {
                break;
            }
        }
        if got {
            stable = 0;
            println!(
                "[{:>6.1}s] increment received: body {} chars / chain of thought {} chars (last report {:.1}s ago)",
                clock.elapsed().as_secs_f32(),
                answer.chars().count(),
                reasoning.chars().count(),
                last_report.elapsed().as_secs_f32()
            );
            last_report = std::time::Instant::now();
        } else {
            stable += 1;
            // Say "still waiting" every 10 seconds and pull up what the page
            // looked like at that moment — to tell "the page rendered nothing"
            // from "it rendered but we didn't read it"
            if stable % 4 == 1 {
                let diag = r#"
(() => {
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const count = (sel) => document.querySelectorAll(sel).length;
  const md = window.__snapiMD;
  let first = '';
  try {
    const node = document.querySelector('.ds-assistant-message-main-content');
    first = node ? cut(md ? String(md(node)) : '(converter missing)', 60) : '(no answer node)';
  } catch (e) { first = 'converter threw: ' + e.name; }
  document.title = 'PROBE|' + JSON.stringify({
    err: '',
    url: cut(location.href, 60),
    t: cut(document.title, 24),
    msgs: count('.ds-message'),
    answers: count('.ds-assistant-message-main-content'),
    think: count('.ds-think-content'),
    mdNodes: count('.ds-markdown'),
    snapi: window.__snapi ? cut(JSON.stringify({read: (window.__snapi.a||{}).read?.length ?? -1, sent: (window.__snapi.a||{}).sent?.length ?? -1}), 60) : '(state gone)',
    first: first,
  });
})()
"#;
                let _ = win.eval(diag);
                if let Some(value) = wait_for("url", 2) {
                    println!(
                        "    page: url={} site title={} msgs={} answers={} thinking blocks={} markdown={} state={}",
                        value.get("url").and_then(|v| v.as_str()).unwrap_or("?"),
                        value.get("t").and_then(|v| v.as_str()).unwrap_or("?"),
                        value.get("msgs").and_then(|v| v.as_u64()).unwrap_or(0),
                        value.get("answers").and_then(|v| v.as_u64()).unwrap_or(0),
                        value.get("think").and_then(|v| v.as_u64()).unwrap_or(0),
                        value.get("mdNodes").and_then(|v| v.as_u64()).unwrap_or(0),
                        value.get("snapi").and_then(|v| v.as_str()).unwrap_or("?"),
                    );
                    println!("    converter on the first answer: {}", value.get("first").and_then(|v| v.as_str()).unwrap_or("?"));
                }
            }
            if last_report.elapsed() >= Duration::from_secs(10) {
                let probe = wait_for("answers", 1);
                println!(
                    "[{:>6.1}s] {:.0}s without any increment; the page now reports: answers={} body length={} ({} chars collected)",
                    clock.elapsed().as_secs_f32(),
                    last_report.elapsed().as_secs_f32(),
                    probe.as_ref().and_then(|v| v.get("answers")).and_then(|v| v.as_u64()).unwrap_or(0),
                    probe.as_ref().and_then(|v| v.get("chars")).and_then(|v| v.as_u64()).unwrap_or(0),
                    answer.chars().count(),
                );
                last_report = std::time::Instant::now();
            }
            if stable >= 12 && !answer.is_empty() {
                at("3 seconds without new content; treating as answered");
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    println!();
    println!("--- conversation list (newest on top) ---");
    let list_js = r#"
(() => {
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const items = [...document.querySelectorAll('a[href*="/a/chat/s/"]')]
    .slice(0, 6)
    .map((a) => cut(String(a.innerText || '').trim().replace(/\s+/g, ' '), 24) || '(no text)');
  document.title = 'PROBE|' + JSON.stringify({ err: '', items: items });
})()
"#;
    let _ = win.eval(list_js);
    if let Some(value) = wait_for("err", 3) {
        if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
            for (i, item) in items.iter().enumerate() {
                println!("  {}. {}", i + 1, item.as_str().unwrap_or(""));
            }
        }
    }

    println!("--- result ---");
    let head = |s: &str, n: usize| -> String { s.chars().take(n).collect::<String>() };
    println!("body {} chars: {}", answer.chars().count(), head(&answer, 160));
    println!("chain of thought {} chars: {}", reasoning.chars().count(), head(&reasoning, 160));
    if answer.is_empty() {
        println!("(not a single char read — the site has probably finished answering already; the problem is on the reading side)");
    }
}

/// Look only, touch nothing: list the sidebar's conversations (title + first
/// few dozen chars) and report whether each contains answers.
///
/// Answers "did my click actually go out, and into which conversation" —
/// **sends nothing**.
fn list_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    use ask_with_snip_lib::webchat;

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(title) => {
                    if let Some(value) = parse_any(&title) {
                        if value.get(field).is_some() {
                            return Some(value);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    };

    // The sidebar isn't rendered in a narrow window; expand it first
    let open_sidebar = r#"
(() => {
  const vis = (el) => el.getBoundingClientRect().width > 0 && el.getBoundingClientRect().height > 0;
  const corner = [...document.querySelectorAll('button, [role=button]')]
    .filter(vis)
    .filter((el) => { const r = el.getBoundingClientRect(); return r.left < 90 && r.top < 90; });
  for (const el of corner.slice(0, 4)) el.click();
  document.title = 'PROBE|' + JSON.stringify({ err: '', n: document.querySelectorAll('a[href*="/a/chat/s/"]').length });
})()
"#;
    let _ = win.eval(open_sidebar);
    let _ = wait_for("n", 3);
    std::thread::sleep(Duration::from_millis(1200));

    let list_js = r#"
(() => {
  const cut = (s, n) => (typeof s === 'string' ? s.slice(0, n) : '');
  const norm = (s) => String(s || '').replace(/\s+/g, ' ').trim();
  const items = [...document.querySelectorAll('a[href*="/a/chat/s/"]')].slice(0, 8).map((a) => ({
    title: cut(norm(a.innerText), 26) || '(no text)',
    href: String(a.getAttribute('href') || ''),
    active: a.className.indexOf('active') >= 0 || a.getAttribute('aria-current') === 'page',
  }));
  document.title = 'PROBE|' + JSON.stringify({ err: '', items: items });
})()
"#;
    let _ = win.eval(list_js);
    println!("== conversations in the sidebar (DOM order, usually newest on top) ==");
    match wait_for("err", 3) {
        None => println!("  no report"),
        Some(value) => match value.get("items").and_then(|v| v.as_array()) {
            Some(items) if !items.is_empty() => {
                for (i, item) in items.iter().enumerate() {
                    println!(
                        "  {}. {}  [{}]{}",
                        i + 1,
                        item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                        item.get("href").and_then(|v| v.as_str()).unwrap_or(""),
                        if item.get("active").and_then(|v| v.as_bool()).unwrap_or(false) { " <- currently open" } else { "" },
                    );
                }
            }
            _ => println!("  none at all (sidebar not rendered?)"),
        },
    }

    // Opening conversations one by one would move the current page, so only
    // read the first: what is on it now
    println!();
    println!("== what the current page holds ==");
    let _ = win.eval(webchat::collect_js());
    if let Some(value) = wait_for("answers", 2) {
        let answers = value.get("answers").and_then(|v| v.as_u64()).unwrap_or(0);
        let chars = value.get("chars").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("  answers={answers}, last answer's body={chars} chars");
    }
}

/// Read a body, then click "new chat", and see whether the read state was
/// cleared.
///
/// This verifies a hypothesis: **the page's "where we read up to" persists
/// across conversations**. If it persists, and a new conversation's answer is
/// identical to the previous one (same image, same question — measured: it
/// really is identical), then `feed` sees "no change" and pushes no increment
/// at all — the panel stays stuck on "analyzing" forever.
///
/// **Read-only**: clicking "new chat" sends nothing into the account (empty
/// conversations are not stored by the site).
fn state_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    use ask_with_snip_lib::webchat;

    let wait_for = |field: &str, tries: usize| -> Option<serde_json::Value> {
        for _ in 0..tries {
            let Ok(rx) = rx.lock() else { return None };
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(title) => {
                    if let Some(value) = parse_any(&title) {
                        if value.get(field).is_some() {
                            return Some(value);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    };
    let collect_once = |win: &tauri::WebviewWindow| -> Option<serde_json::Value> {
        let _ = win.eval(webchat::collect_js());
        wait_for("d", 2)
    };

    println!("== 1. read the current page once ==");
    match collect_once(win) {
        None => {
            println!("  no report");
            return;
        }
        Some(value) => {
            let chars = value.get("chars").and_then(|v| v.as_u64()).unwrap_or(0);
            let d = value.get("d").and_then(|v| v.as_str()).unwrap_or("");
            println!("  page body {} chars; this round pushed {} chars (first read should be the whole thing)", chars, d.chars().count());
        }
    }

    println!("== 2. click \"new chat\" ==");
    let _ = win.eval(webchat::new_chat_js(webchat::probe_site()));
    std::thread::sleep(Duration::from_millis(2500));
    let mut reloaded = false;
    while let Ok(title) = rx.lock().map(|rx| rx.try_recv()) {
        match title {
            Ok(title) => {
                if let Some(value) = parse_any(&title) {
                    if value.get("newchat").and_then(|v| v.as_str()) == Some("fallback") {
                        reloaded = true;
                    }
                }
            }
            Err(_) => break,
        }
    }
    println!("  route taken: {}", if reloaded { "fell back to the home page (page reload)" } else { "clicked the button" });
    std::thread::sleep(Duration::from_millis(2000));

    println!("== 3. inspect the read state in the new conversation ==");
    match collect_once(win) {
        None => println!("  no report"),
        Some(value) => {
            let chars = value.get("chars").and_then(|v| v.as_u64()).unwrap_or(0);
            let answers = value.get("answers").and_then(|v| v.as_u64()).unwrap_or(0);
            let d = value.get("d").and_then(|v| v.as_str()).unwrap_or("");
            println!(
                "  answers={answers}, body in the read state={} chars; this round pushed {} chars",
                chars,
                d.chars().count()
            );
            if chars > 0 && !reloaded {
                println!("  => the state was **not** cleared: as long as the new conversation's answer matches the previous one, it is treated as \"no change\"");
            } else if reloaded {
                println!("  => the page reloaded, so the state is fresh (this path has no such problem)");
            } else {
                println!("  => the state was cleared (this path is fine)");
            }
        }
    }
}

/// Measure whether the page can still run animation frames and timers while
/// "hidden / offscreen / shown".
///
/// Why measure: Chromium pauses requestAnimationFrame and throttles timers for
/// **invisible** pages (hidden long enough, down to once per minute). The
/// driver's web UI window is always hidden; if the site's streaming rendering
/// relies on exactly these, not a single char ever gets painted — we read
/// empty nodes forever, which shows up as "stuck on analyzing, no answer",
/// until timeout. Rounds where the window had been brought up by "Show page"
/// all worked fine, which matches exactly.
///
/// **Read-only**: only runs a timing JS snippet in the window; touches no
/// site, sends nothing.
fn throttle_run(
    win: &tauri::WebviewWindow,
    rx: &std::sync::Arc<std::sync::Mutex<mpsc::Receiver<String>>>,
) {
    let js = r#"
(() => {
  window.__probe = { raf: 0, timer: 0 };
  const tick = () => { window.__probe.raf += 1; requestAnimationFrame(tick); };
  requestAnimationFrame(tick);
  const id = setInterval(() => { window.__probe.timer += 1; }, 100);
  setTimeout(() => {
    clearInterval(id);
    document.title = 'PROBE|' + JSON.stringify({
      err: '',
      vis: document.visibilityState,
      raf: window.__probe.raf,
      timer: window.__probe.timer,
    });
  }, 3000);
})()
"#;
    println!("== is the page still alive for 3 seconds (rAF and a 100ms timer, how many ticks each) ==");
    if let Err(err) = win.eval(js) {
        println!("  injection failed: {err}");
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        let got = rx.lock().ok().and_then(|rx| rx.recv_timeout(Duration::from_millis(500)).ok());
        let Some(title) = got else { continue };
        let Some(value) = parse_any(&title) else { continue };
        if value.get("raf").is_some() {
            let raf = value.get("raf").and_then(|v| v.as_u64()).unwrap_or(0);
            let timer = value.get("timer").and_then(|v| v.as_u64()).unwrap_or(0);
            println!(
                "  visibilityState={} rAF={raf} ticks (ideal ~180)  100ms timer={timer} ticks (ideal ~30)",
                value.get("vis").and_then(|v| v.as_str()).unwrap_or("?")
            );
            if raf == 0 {
                println!("  => animation frames fully stalled: anything painting text via rAF never updates in a hidden window");
            } else if raf < 30 {
                println!("  => animation frames heavily throttled");
            }
            if timer < 10 {
                println!("  => timers throttled too");
            }
            return;
        }
    }
    println!("  no report");
}

fn base64_of(bytes: &[u8]) -> String {    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn main() {
    // Declare WebView2's debug port via environment variable (the argument
    // form is ignored on some versions). The probe has one window, fixed 9400.
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--remote-debugging-port=9400 --remote-allow-origins=*",
    );
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry = args.iter().any(|a| a == "--dry");
    let convert = args.iter().any(|a| a == "--convert");
    let think = args.iter().any(|a| a == "--think");
    let collect = args.iter().any(|a| a == "--collect");
    let actions = args.iter().any(|a| a == "--actions");
    let list = args.iter().any(|a| a == "--list");
    let state = args.iter().any(|a| a == "--state");
    let throttle = args.iter().any(|a| a == "--throttle");
    // Besides --throttle, pass --visible=hidden|offscreen to measure other modes
    let window_mode = args
        .iter()
        .find_map(|a| a.strip_prefix("--visible="))
        .unwrap_or(if throttle { "hidden" } else { "shown" })
        .to_string();
    // --regen <record json>: reproduce "regenerate" (really sends into the account)
    let regen = args
        .iter()
        .position(|a| a == "--regen")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let toggle = args.iter().any(|a| a == "--toggle");
    let dom = args.iter().any(|a| a == "--dom");
    let asktest = args.iter().any(|a| a == "--asktest");
    let has_img = args.iter().any(|a| a == "--img");
    let steps = args.iter().any(|a| a == "--steps");
    // --url <conversation URL>: open directly on an old conversation (the
    // sidebar is collapsed in narrow windows and unclickable; deep-linking is
    // easiest). Without it, open the home page.
    let target = args
        .iter()
        .position(|a| a == "--url")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // --site <id>: run against another site (deepseek / chatgpt / gemini /
    // claude); selectors switch accordingly
    let site_id = args
        .iter()
        .position(|a| a == "--site")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "deepseek".into());
    ask_with_snip_lib::webchat::probe_select_site(&site_id);
    let url = if target.is_none() {
        ask_with_snip_lib::webchat::probe_site().home.to_string()
    } else {
        target.clone().unwrap()
    };
    // --account <name>: borrow that account's login state (same data
    // directory). **The app must have exited first** — otherwise that
    // account's window holds the directory and WebView2 won't start.
    let account = args
        .iter()
        .position(|a| a == "--account")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let url: tauri::Url = url.parse().expect("invalid URL");
    // One load brings several events (Started/Finished, another round after
    // redirects); let only the first Finished read the content
    let handled = AtomicBool::new(false);
    // document.title changes are only received via on_document_title_changed;
    // Tauri doesn't sync them to the window title by default.
    // The Arc<Mutex> exists because the on_page_load callback is Fn (may be
    // called several times), so values can't be moved out, and Receiver isn't Sync.
    let (title_tx, title_rx) = mpsc::channel::<String>();
    let title_rx = std::sync::Arc::new(std::sync::Mutex::new(title_rx));

    tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            // The callback moves the whole thing away, so keep a copy on each side
            let page_handle = handle.clone();
            let watchdog_handle = handle.clone();

            let builder =
                WebviewWindowBuilder::new(app, "webchat-probe", WebviewUrl::External(url))
                    .title("webchat probe (closes itself when done)")
                    .inner_size(900.0, 700.0);
            // --throttle measures page liveness in "hidden / offscreen / shown"
            // states, so all three arrangements must be creatable
            let mut builder = match window_mode.as_str() {
                "hidden" => builder.visible(false),
                "offscreen" => builder.position(-4000.0, -4000.0).skip_taskbar(true),
                _ => builder.center(),
            };
            if let Some(name) = account.clone() {
                let label = ask_with_snip_lib::webchat::window_label(&name);
                let dir = tauri::Manager::path(
                    app.handle())
                    .app_local_data_dir()
                    .expect("cannot get the data directory")
                    .join("EBWebView")
                    .join(&label);
                println!("(borrowing account {name}'s login state: {})", dir.display());
                builder = builder.data_directory(dir);
            }
            // Proxy follows the config: without it the other three sites are
            // unreachable in China and calibration is impossible.
            // The probe window always opens one CDP debug port (no conflict
            // with the main program's per-account allocation)
            builder = builder.additional_browser_args(
                "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port=9400",
            );
            if let Ok(cfg_path) = tauri::Manager::path(app.handle()).app_config_dir() {
                if let Ok(text) = std::fs::read_to_string(cfg_path.join("config.json")) {
                    if let Ok(cfg) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(proxy) = cfg.get("webchat_proxy").and_then(|v| v.as_str()) {
                            if !proxy.trim().is_empty() {
                                println!("(using the proxy from the config: {proxy})");
                                builder = builder.additional_browser_args(&format!(
                                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --proxy-server={proxy}"
                                ));
                            }
                        }
                    }
                }
            }
            builder
                .on_document_title_changed(move |_win, title| {
                    // Both payload kinds go into the channel: PROBE| is the
                    // reconnaissance, SNAPI| is the driver's JS
                    if title.starts_with("PROBE|") || title.starts_with(ask_with_snip_lib::webchat::TAG) {
                        let _ = title_tx.send(title);
                    }
                })
                .on_page_load(move |win, payload| {
                    println!("page load: {:?} {}", payload.event(), payload.url());
                    if payload.event() != PageLoadEvent::Finished {
                        return;
                    }
                    if handled.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let handle = page_handle.clone();
                    let title_rx = std::sync::Arc::clone(&title_rx);
                    // The outer layer is Fn (may be called several times), so
                    // clone the values before moving them into the thread
                    let regen_now = regen.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(SETTLE);
                        if throttle {
                            throttle_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if dry {
                            dry_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if convert {
                            convert_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if think {
                            think_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if collect {
                            collect_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if actions {
                            actions_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if state {
                            state_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if list {
                            list_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if dom {
                            dom_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if asktest {
                            asktest_run(&win, &title_rx, has_img);
                            handle.exit(0);
                            return;
                        }
                        if steps {
                            steps_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if let Some(record) = regen_now.clone() {
                            regen_run(&win, &title_rx, &record);
                            handle.exit(0);
                            return;
                        }
                        if toggle {
                            toggle_run(&win, &title_rx);
                            handle.exit(0);
                            return;
                        }
                        if let Err(err) = win.eval(PROBE_JS) {
                            println!("JS injection failed: {err}");
                        }
                        match title_rx.lock().map(|rx| rx.recv_timeout(TITLE_WAIT)) {
                            Ok(Ok(title)) => report(&title),
                            Ok(Err(RecvTimeoutError::Timeout)) => println!(
                                "\nno page content arrived: the page loaded but its content can't be read,\n\
                                 probably blocked on a verification page, or this site doesn't accept script injection."
                            ),
                            Ok(Err(RecvTimeoutError::Disconnected)) => println!("\nthe read channel disconnected"),
                            Err(_) => println!("\nthe read channel lock is broken"),
                        }
                        handle.exit(0);
                    });
                })
                .build()?;

            // Watchdog: if loading never ends (network stuck, blocked with no
            // response), still wrap up
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(150));
                println!("\n150 seconds passed with no result: this path doesn't work (blocked, timed out, or load events never fired).");
                watchdog_handle.exit(1);
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the probe failed to start");
}

fn report(title: &str) {
    let body = title.strip_prefix("PROBE|").unwrap_or(title);    println!("\n== page reconnaissance (read back {} chars) ==", body.len());
    for part in body.split(SEP) {
        println!("  {part}");
    }

    // The result is one joined line; search by fragment
    let find = |key: &str| -> Option<&str> {
        body.split(SEP).find(|part| part.starts_with(key))
    };
    let imgs = |part: Option<&str>| {
        part.and_then(|p| p.split("img=").nth(1))
            .and_then(|rest| rest.split([',', ' ']).next())
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0)
    };

    println!();
    println!("Conclusion:");
    println!(
        "  - composer {}, login token {}",
        if body.contains("composer: yes") { "present" } else { "absent (probably stuck on the login page)" },
        if body.contains("login token: yes") { "present (this webview is logged in and can be driven)" } else { "absent" }
    );

    let before = imgs(find("before image"));
    let after = imgs(find("after image"));
    if after > before {
        println!("  - image stuffing succeeded: the page accepted the file and drew a thumbnail (img {before} -> {after})");
    } else {
        println!("  - no change seen from image stuffing (img {before} -> {after})");
    }

    match find("message candidates") {
        None => println!("  - never reached the \"inspect message structure\" step (probably no clickable old conversations in the sidebar)"),
        Some(_) => {
            if let Some(line) = body.split(SEP).find(|p| p.starts_with("longest text block")) {
                println!("  - {line}");
            }
            println!("  - message structure: see the \"message candidates\" line above");
        }
    }
}
