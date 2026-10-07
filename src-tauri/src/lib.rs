pub mod api;
pub mod cdp;
pub mod capture;
pub mod config;
pub mod history;
pub mod webchat;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use capture::Frame;
use config::AppConfig;
use history::{Conversation, StagedImage};

const OVERLAY: &str = "overlay";
const PANEL: &str = "panel";
const SETTINGS: &str = "settings";
/// Hub window: gives users who "can't find the entry point" a visible button.
const HUB: &str = "hub";
/// Startup toast: after launch this tool quietly resides in the tray; if nothing is
/// displayed, the user can't confirm it started. This small window sticks to the
/// bottom-right corner, disappears on its own after a few seconds, and never steals focus.
const TOAST: &str = "toast";
/// Embedded web-chat window. The fallback path when API quota runs out: the screenshot
/// goes to the clipboard and the user pastes and asks on the web page themselves, using
/// the web account without consuming API quota.
const WEBCHAT: &str = "webchat";
/// Web-chat URL. To switch to another provider's web version, just change this line
/// (recompile afterwards).
const WEBCHAT_URL: &str = "https://chat.deepseek.com/";

/// Event sent to the frontend when hotkey registration fails
const EVT_HOTKEY_ERROR: &str = "hotkey-error";
/// Saved config's theme: every open window re-skins itself on receipt.
const EVT_THEME_CHANGED: &str = "theme-changed";
/// Emitted when the conversation changed (new snip, or an entry opened from history);
/// tells the panel to refresh
const EVT_CONVERSATION_CHANGED: &str = "conversation-changed";
/// Emitted when a new frame is ready; tells the overlay to pick up the image again
/// (the overlay window is reused too)
const EVT_FRAME_READY: &str = "frame-ready";

// Default window sizes. The remembered sizes are used in practice; these are only
// the first-open values.
const PANEL_W: f64 = 900.0;
const PANEL_H: f64 = 620.0;
const SETTINGS_W: f64 = 780.0;
const SETTINGS_H: f64 = 780.0;
const HUB_W: f64 = 470.0;
const HUB_H: f64 = 400.0;
/// Size of the web-chat window on first open. It's a page for reading long answers and
/// logging in, so give it more room.
const WEBCHAT_W: f64 = 1000.0;
const WEBCHAT_H: f64 = 760.0;
const PANEL_GAP: f64 = 12.0;

// Size and edge margins of the startup toast. Extra room is left at the bottom-right
// for the taskbar.
const TOAST_W: f64 = 366.0;
const TOAST_H: f64 = 104.0;
const TOAST_MARGIN: f64 = 22.0;
const TOAST_BOTTOM: f64 = 86.0;

/// The current conversation.
///
/// Keeping "this turn's screenshots" and "all Q&A turns" together because they are
/// inherently bound: follow-ups must carry the screenshots, and history must store
/// them too; storing them separately would require pairing everywhere.
#[derive(Default)]
struct AppState {
    /// Frozen frame: a full-screen shot taken when the hotkey fires; all crops are
    /// based on it afterwards
    frame: Mutex<Option<Frame>>,
    /// Why the screen capture failed. It's what lets the overlay window show the
    /// failure reason to the user instead of the hotkey seemingly doing nothing.
    snip_error: Mutex<Option<String>>,
    conversation: Mutex<Option<Conversation>>,
    /// Whether a snip is in progress.
    ///
    /// An explicit flag rather than "is the overlay window present" — the window is
    /// reused and always exists, so existence checks would give wrong answers.
    snip_active: AtomicBool,
    /// Whether this snip round was triggered by the panel's "+" button (append mode):
    /// the shot goes into the current conversation's staging area (film strip),
    /// skipping the "continue context / start new" logic. See snip_for_staging.
    staging: AtomicBool,
    /// Which snip round this is. Incremented per trigger, used to invalidate the
    /// previous round's watchdog thread (see arm_overlay_watchdog).
    snip_gen: AtomicU64,
    /// When this snip round was triggered (unix ms). Only used to measure the
    /// "hotkey -> overlay visible" latency: if it gets too large the overlay appears
    /// later than the user starts moving, the prime suspect when snipping misbehaves.
    snip_started_at: AtomicU64,
    /// Read-back channel for the web-chat driver. The page writes deltas into
    /// document.title and the window's title events feed them in here; the running
    /// turn receives them from this end. None when no driver is running and the
    /// signal is dropped.
    ///
    /// A tokio channel rather than std's: the driver is an async task that must hold
    /// the receiver across awaits, and std's Receiver is not Send (the compiler
    /// rejects it; the sender side is synchronous, works fine either way).
    webchat_signal: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    /// Cancel flag for the current turn. Set when the user presses "Cancel"; the
    /// running path sees it on its next loop iteration.
    cancel: Mutex<api::Cancel>,
    /// When the current turn started (unix ms). The panel shows "Analyzing… 12s"
    /// so the user knows it's alive, not hung.
    ask_started_at: AtomicU64,
    busy: AtomicBool,
}

/// Lock without panicking: in a resident tool, one poisoned lock isn't worth
/// crashing the whole process over.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The last time window geometry was written to disk (unix ms), used by
/// remember_window_rect_throttled.
///
/// Window events are dispatched on the main thread, so there's no race between
/// read-modify-write; Relaxed is enough.
static LAST_LAYOUT_SAVE: AtomicU64 = AtomicU64::new(0);

/// Flow tracing log, off by default; for diagnosing "window should appear but
/// doesn't" and "stuck" situations.
///
/// ```text
/// set SNAPI_TRACE=1 && ask-with-snip.exe
/// ```
///
/// Logs go to stderr, so the app must be started from a command line to see them.
/// These problems only happen at runtime and are hard to guess by reading code,
/// so keeping this switch beats adding prints after the fact.
/// Log file: `<config dir>/trace.log`, rewritten on each launch.
///
/// Why it writes to disk by default: when users report "the answer finished on the
/// web page but the app never received it", the scene only exists on their side
/// (no screen sharing, no access to their windows), and guessing from code afterwards
/// is painful. The log stays in a file; next reproduction just reads that one file.
static TRACE_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static TRACE_LOG: std::sync::Mutex<Option<(std::fs::File, u64)>> = std::sync::Mutex::new(None);
/// Per-file cap; stop writing once full: a long-lived window shouldn't quietly fill the disk
const TRACE_LIMIT: u64 = 4 * 1024 * 1024;

/// Record the config directory (called once in `setup`) so `trace` knows where to write.
fn set_trace_dir(dir: std::path::PathBuf) {
    let _ = TRACE_DIR.set(dir.clone());
    // Create the file right away (create truncates): otherwise leftovers from the
    // previous run would be mistaken for this run's log
    if let Ok(mut guard) = TRACE_LOG.lock() {
        let _ = std::fs::create_dir_all(&dir);
        *guard = std::fs::File::create(dir.join("trace.log"))
            .ok()
            .map(|file| (file, 0u64));
    }
}

fn trace(message: &str) {
    if std::env::var_os("SNAPI_TRACE").is_some() {
        eprintln!("[trace] {message}");
    }
    let Ok(mut guard) = TRACE_LOG.lock() else {
        return;
    };
    if guard.is_none() {
        let dir = TRACE_DIR.get().cloned().unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&dir);
        *guard = std::fs::File::create(dir.join("trace.log"))
            .ok()
            .map(|file| (file, 0u64));
    }
    let Some((file, written)) = guard.as_mut() else {
        return;
    };
    if *written > TRACE_LIMIT {
        return;
    }
    // Prefix is "seconds since the first log line": when diagnosing these problems what
    // matters is ordering and intervals; absolute time needs timezone math and is less direct
    let line = format!("[+{:7.3}s] {message}\n", log_clock().elapsed().as_secs_f64());
    if std::io::Write::write_all(file, line.as_bytes()).is_ok() {
        *written += line.len() as u64;
    }
}

/// Reference point for relative time: the moment the first log line was written.
fn log_clock() -> &'static std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now)
}

/// Give a window "close means hide" behavior and remember its size/position.
///
/// Every window in this program is reused and never destroyed. Besides slow rebuilds,
/// the key reason is a pitfall we hit: **destroying a window from within its own IPC
/// call** (e.g. the overlay's JS calling dismiss_overlay to destroy itself) leaves the
/// webview in a zombie state where "the native window is gone but the label is still
/// taken" — `get_webview_window` returns Some forever, waiting never helps, and
/// building a window with the same label afterwards fails with
/// "a webview with label `xxx` already exists". What the user sees is
/// "the first snip works, the next one says already exists".
///
/// So the unified approach: intercept the close request, store the geometry, and only
/// hide. Real exit goes through app.exit(0).
fn hide_on_close(app: &AppHandle, label: &str) {
    let Some(win) = app.get_webview_window(label) else {
        return;
    };
    // The overlay's size/position is computed by the program every time (it must cover
    // the whole screen), not worth remembering. Web-account window labels are dynamic
    // (webchat-<account>), so match by prefix.
    let tracked = label == PANEL || label == SETTINGS || label == HUB || label.starts_with("webchat-");
    let label = label.to_string();
    let handle = app.clone();
    win.on_window_event(move |event| match event {
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            if let Some(win) = handle.get_webview_window(&label) {
                remember_window_rect(&handle, &label, &win);
                let _ = win.hide();
            }
        }
        // Record while moving/resizing, not only on hide — users often move the window
        // then go straight into another snip or quit the app entirely without clicking
        // ✕, and that move would be lost (the window would reopen at the default spot).
        // This event is dense while dragging, so throttle it.
        tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_) if tracked => {
            remember_window_rect_throttled(&handle, &label);
        }
        _ => {}
    });
}

/// While dragging/resizing, persist geometry at most every 300ms.
///
/// WM_MOVE is per-pixel: one drag can produce hundreds of events and writing the file
/// for each is pointless. The final value after release is covered by the save on
/// window hide and on exit.
fn remember_window_rect_throttled(app: &AppHandle, label: &str) {
    let now = history::now_millis().max(0) as u64;
    let last = LAST_LAYOUT_SAVE.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 300 {
        return;
    }
    LAST_LAYOUT_SAVE.store(now, Ordering::Relaxed);
    if let Some(win) = app.get_webview_window(label) {
        if win.is_minimized().unwrap_or(false) {
            return; // size is meaningless when minimized; don't overwrite the remembered value
        }
        remember_window_rect(app, label, &win);
    }
}

/// Re-record every window's geometry before exit.
///
/// Users often quit with the panel open (or pick "Quit" from the tray); at that
/// instant the window's size/position is the final result they want — not saving it
/// would put the window back at the default position next time.
fn save_all_window_layouts(app: &AppHandle) {
    for (label, win) in app.webview_windows() {
        let tracked = label == PANEL || label == SETTINGS || label == HUB || label.starts_with("webchat-");
        if !tracked || win.is_minimized().unwrap_or(false) || !win.is_visible().unwrap_or(false) {
            continue;
        }
        remember_window_rect(app, &label, &win);
    }
}

/// Remember a window's position and size (logical coordinates).
///
/// Stores the outer-frame position + client-area size: restore uses position() and
/// inner_size() respectively, so windows with title bars still line up despite
/// differing frame thickness.
fn remember_window_rect(app: &AppHandle, label: &str, win: &tauri::WebviewWindow) {
    let (Ok(position), Ok(size)) = (win.outer_position(), win.inner_size()) else {
        return;
    };
    let scale = win.scale_factor().unwrap_or(1.0).max(0.01);
    config::save_layout(
        app,
        label,
        config::WindowRect {
            x: (position.x as f64 / scale).round() as i32,
            y: (position.y as f64 / scale).round() as i32,
            width: (size.width as f64 / scale).round() as u32,
            height: (size.height as f64 / scale).round() as u32,
        },
    );
}

/// Load the last remembered window geometry. If the position is clearly off-screen
/// (monitor changed, resolution changed), pretend there is none; otherwise the user
/// gets "the window opened but can't be seen at all".
fn restored_rect(app: &AppHandle, label: &str) -> Option<config::WindowRect> {
    let rect = config::load_layouts(app).get(label)?;
    if !rect.is_valid() {
        return None;
    }
    // Measure the screen with Tauri's own monitor info — its logical coordinates are
    // exactly what position()/inner_size() are interpreted against. The xcap-based
    // capture::primary_geometry() can report a different (wrong) DPI scale, and then
    // a saved position passes the bounds check yet lands half off-screen once Tauri
    // applies the real scale (the settings window ended up as a ~100px sliver past
    // the screen's right edge — the user reads that as "the page won't open").
    let (screen_w, screen_h) = match app.primary_monitor().ok().flatten() {
        Some(monitor) => {
            let scale = (monitor.scale_factor() as f64).max(0.01);
            (
                monitor.size().width as f64 / scale,
                monitor.size().height as f64 / scale,
            )
        }
        None => {
            let geometry = capture::primary_geometry().ok()?;
            let scale = geometry.scale_factor.max(0.01) as f64;
            (
                geometry.width as f64 / scale,
                geometry.height as f64 / scale,
            )
        }
    };
    let width = rect.width as f64;
    let height = rect.height as f64;

    // Window bigger than the screen (monitor changed): the saved geometry is
    // meaningless, fall back to the default position.
    if width > screen_w || height > screen_h {
        trace(&format!("{label}'s saved size {width}x{height} no longer fits the screen; using the default position"));
        return None;
    }

    // Clamp the position so the whole window stays on screen. A saved position is
    // only valid for the monitor layout / DPI scale it was saved under; after the
    // layout changes, restoring it verbatim can push the window almost entirely
    // off-screen (e.g. x=1605 logical on a 1706-logical-wide screen leaves a ~100px
    // sliver at the right edge — the user reads that as "the window won't open").
    let x = (rect.x as f64).clamp(0.0, screen_w - width);
    let y = (rect.y as f64).clamp(0.0, screen_h - height);
    if x != rect.x as f64 || y != rect.y as f64 {
        trace(&format!(
            "{label}'s saved position ({},{}) is not fully on screen; clamped to ({x},{y})",
            rect.x, rect.y
        ));
    }
    Some(config::WindowRect {
        x: x.round() as i32,
        y: y.round() as i32,
        width: rect.width,
        height: rect.height,
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FramePayload {
    data_url: String,
    width: u32,
    height: u32,
    scale_factor: f32,
}

/// Does this snip get appended to the current conversation, or start a new one?
///
/// Only two cases append: the panel's `+` (`staging`, an explicit "continue this"),
/// and **the "continue context" toggle being on while the current conversation has
/// already been asked in** (`should_continue`).
///
/// Note: leftover unsent images on the film strip **no longer** auto-append — that
/// rule clashed with "toggle the continue setting off": the user turns the toggle
/// off, snips a new image, and it still gets attached to the old conversation
/// (complained about in real use). Now the toggle decides; when starting a new
/// conversation, the old strip's images are **moved into the new conversation's film
/// strip**, not lost.
fn should_append(staging: bool, continue_setting: bool, current: Option<&Conversation>) -> bool {
    staging || should_continue(continue_setting, current)
}

/// Should this snip continue the current conversation or start a new one?
///
/// The toggle being on isn't enough; the current conversation must **have been asked
/// in already**: with only a freshly snipped, not-yet-asked image around, holding it
/// is pointless. (This tier can no longer reach "replace that image" — when unsent
/// images exist, `appends_to_current` blocks first and appends directly.)
/// Extracted as its own function because these conditions are easy to get wrong and
/// extraction makes them directly testable.
fn should_continue(continue_setting: bool, current: Option<&Conversation>) -> bool {
    continue_setting && current.map(|c| !c.turns.is_empty()).unwrap_or(false)
}

/// Everything the panel needs: the staging area (film strip) + all Q&A turns.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationPayload {
    id: String,
    created_at: i64,
    /// The image shown in the UI: prefer the last staged one, otherwise the last turn's
    data_url: String,
    crop_w: u32,
    crop_h: u32,
    sent_w: u32,
    sent_h: u32,
    /// Staging area (film strip): snipped-but-unsent images, with their selection state
    staged: Vec<StagedPayload>,
    /// The images from the most recent turn that carried images (data url + their
    /// position within that turn). The film strip shouldn't go empty after sending:
    /// those images now belong to that turn, and the panel keeps displaying them
    /// alongside the staging area so the layout matches pre-send. Without this, after
    /// sending only the "single static preview" remains and several images look like one.
    /// Positions are included because each card has an ✕: deleting a sent image edits
    /// that turn's record.
    sent: Vec<SentPayload>,
    turns: Vec<TurnPayload>,
}

/// One card in the staging area. Only the data url is sent for the image itself
/// (the panel displays it directly).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StagedPayload {
    data_url: String,
    selected: bool,
    crop_w: u32,
    crop_h: u32,
    sent_w: u32,
    sent_h: u32,
}

/// A film-strip card that has "already been sent": the image + its position in the
/// record (needed to point at the right one when deleting).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SentPayload {
    data_url: String,
    /// Which turn (0-based)
    turn_index: usize,
    /// Which image within that turn (0-based)
    image_index: usize,
}

/// One Q&A turn. The images aren't sent wholesale (a conversation can hold several,
/// each hundreds of KB of base64); the frontend is only told "this turn carried N
/// freshly snipped images" so it can badge the UI.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnPayload {
    question: String,
    answer: String,
    /// How many images this turn carried (0 = text-only follow-up)
    image_count: usize,
    /// Screenshots sent with this turn (data urls): the conversation renders the images
    /// back onto their turn, no need to dig through the film strip when reviewing
    #[serde(rename = "images")]
    images: Vec<String>,
}

impl From<&Conversation> for ConversationPayload {
    fn from(conv: &Conversation) -> Self {
        Self {
            id: conv.id.clone(),
            created_at: conv.created_at,
            // A record with no images at all (shouldn't happen normally) gets an empty
            // string so the frontend collapses the preview entirely, instead of sending
            // "data:image/jpeg;base64," which the browser renders as a broken image
            data_url: match conv.display_image() {
                "" => String::new(),
                image => format!("data:image/jpeg;base64,{image}"),
            },
            crop_w: conv.crop_w,
            crop_h: conv.crop_h,
            sent_w: conv.sent_w,
            sent_h: conv.sent_h,
            staged: conv
                .staged
                .iter()
                .map(|item| StagedPayload {
                    data_url: format!("data:image/jpeg;base64,{}", item.image_b64),
                    selected: item.selected,
                    crop_w: item.crop_w,
                    crop_h: item.crop_h,
                    sent_w: item.sent_w,
                    sent_h: item.sent_h,
                })
                .collect(),
            // Prefer the **most recent turn with images**: after a text-only follow-up,
            // the film strip must not wipe out the previous batch of images
            sent: conv
                .turns
                .iter()
                .enumerate()
                .rev()
                .map(|(index, turn)| (index, turn.all_images()))
                .find(|(_, images)| !images.is_empty())
                .map(|(turn_index, images)| {
                    images
                        .into_iter()
                        .enumerate()
                        .map(|(image_index, image)| SentPayload {
                            data_url: format!("data:image/jpeg;base64,{image}"),
                            turn_index,
                            image_index,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            turns: conv
                .turns
                .iter()
                .map(|turn| TurnPayload {
                    question: turn.question.clone(),
                    answer: turn.answer.clone(),
                    image_count: turn.all_images().len(),
                    images: turn
                        .all_images()
                        .iter()
                        .filter(|image| !image.is_empty())
                        .map(|image| format!("data:image/jpeg;base64,{image}"))
                        .collect(),
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------- Hotkey and snipping

/// Trigger a snip: freeze the full screen first, then prepare the overlay window.
///
/// Screen capture is a blocking system call, so it goes on its own thread; window
/// operations must return to the main thread.
fn spawn_snip(app: &AppHandle) {
    // Ignore duplicate triggers while a snip is in progress. This flag must be reset
    // on every early-return path, otherwise the hotkey looks broken and never
    // responds again.
    let state = app.state::<AppState>();
    if state.snip_active.swap(true, Ordering::SeqCst) {
        return;
    }
    // Start a new round; the previous round's watchdog expires now (otherwise it
    // would wrongly clear this round's state)
    let generation = state.snip_gen.fetch_add(1, Ordering::SeqCst) + 1;
    state
        .snip_started_at
        .store(history::now_millis().max(0) as u64, Ordering::SeqCst);
    drop(state);
    trace(&format!("spawn_snip: round {generation} starts"));

    let app = app.clone();
    std::thread::spawn(move || {
        // Get the geometry first: even if capture fails, having the size lets the
        // overlay window be placed correctly so the failure reason can be shown.
        let geometry = match capture::primary_geometry() {
            Ok(geometry) => geometry,
            Err(err) => {
                eprintln!("{err}");
                reset_snip_active(&app);
                return;
            }
        };

        let captured = capture::capture_primary();
        let capture_error = captured.as_ref().err().cloned();
        trace(&format!(
            "spawn_snip: capture {}",
            match &capture_error {
                Some(err) => format!("failed: {err}"),
                None => "done".to_string(),
            }
        ));

        let scale = geometry.scale_factor.max(0.01) as f64;
        let size = (
            (geometry.width.max(320)) as f64 / scale,
            (geometry.height.max(240)) as f64 / scale,
        );
        let pos = (geometry.origin_x as f64 / scale, geometry.origin_y as f64 / scale);

        {
            let state = app.state::<AppState>();
            *lock(&state.snip_error) = capture_error;
            *lock(&state.frame) = captured.ok();
        }

        let app_for_ui = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Err(err) = present_overlay(&app_for_ui, generation, pos, size) {
                eprintln!("{err}");
                reset_snip_active(&app_for_ui);
            }
        });
    });
}

fn reset_snip_active(app: &AppHandle) {
    app.state::<AppState>()
        .snip_active
        .store(false, Ordering::SeqCst);
}

/// Prepare the overlay. The window is reused: created only the first time; afterwards
/// it's just hidden, repositioned/resized, and the frontend is notified of the new frame.
fn present_overlay(
    app: &AppHandle,
    generation: u64,
    pos: (f64, f64),
    size: (f64, f64),
) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(OVERLAY) {
        let _ = win.hide();
        let _ = win.set_size(tauri::LogicalSize::new(size.0, size.1));
        let _ = win.set_position(tauri::LogicalPosition::new(pos.0, pos.1));
        // On receipt the frontend re-fetches the frame, lays the background, resets the
        // selection box, then calls back overlay_ready to show it
        let _ = app.emit_to(OVERLAY, EVT_FRAME_READY, ());
        trace("present_overlay: reused existing window, frame-ready broadcast");
    } else {
        create_overlay(app, pos, size)?;
        trace("present_overlay: created overlay window (cold start, slowest round)");
    }

    arm_overlay_watchdog(app, generation);
    Ok(())
}

/// Safety net: the frontend is supposed to finish painting the frozen frame and call
/// overlay_ready to show it.
///
/// If the frontend never gets there (error, hang), snip_active stays held and the
/// hotkey looks broken from then on. So after a timeout the flag is released and the
/// user can at least press the hotkey again.
///
/// One very subtle pitfall here, worth writing down: **release the flag only, never
/// touch the frozen frame**. The frame used to be cleared too, reasoning "not needed
/// anymore, release those dozens of MB early". But the cold-start overlay round shows
/// slowly (WebView2 cold start + full-screen JPEG encoding + handing to the webview to
/// decode; can exceed 3 seconds), and at 3 seconds the frame got destroyed while the
/// user was looking at a **perfectly normal** frozen picture (the image is in the
/// webview, unaffected) — so they snipped normally and only at submit time got "the
/// frame is already stale". Worse, they could keep snipping, so every snip failed,
/// looking like an endless loop. Frame release belongs to teardown_overlay and the
/// next snip (which overwrites directly); a few MB isn't worth trading correctness for.
///
/// Also, don't conclude after a single check: the window may just be slow, not broken.
/// If it truly is broken, no fear — the user pressing Esc goes through cancel_snip
/// and releases the state all the same.
fn arm_overlay_watchdog(app: &AppHandle, generation: u64) {
    let handle = app.clone();
    std::thread::spawn(move || {
        for delay in [3u64, 4, 5] {
            std::thread::sleep(std::time::Duration::from_secs(delay));
            if !overlay_still_missing(&handle, generation) {
                return;
            }
        }
        eprintln!("the overlay never became visible; state has been reset (the frontend may have errored; see the [webview] log above)");
        reset_snip_active(&handle);
    });
    arm_overlay_hard_cap(app, generation);
}

/// Hard deadline for a snip round. The soft watchdog above only covers "the overlay
/// never showed"; if the overlay **did** show but the webview later died (or hung),
/// `is_visible()` stays true, the soft watchdog never fires, snip_active is held
/// forever, and every hotkey/tray/button press is silently swallowed — the app looks
/// dead until rebooted.
///
/// So this second timer just gives up after a generous cap: a live selection takes
/// seconds, not minutes; even a user who parks the overlay to read the screen for a
/// while will resume long before the cap. On expiry the round is torn down via
/// teardown_overlay (hide overlay, drop the frame, release the flag — the same code
/// path as Esc, so no new state transitions are invented). At this timescale the
/// frame cannot be mid-use: if the frontend were alive enough to submit,
/// finish_selection would have run and the flag would already be released (or about
/// to be, by dismiss_overlay — tearing down again is harmless, teardown is idempotent
/// on the same window).
///
/// The generation check first: if a new round started, this timer is stale and must
/// not touch anything.
const SNIP_HARD_CAP: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn arm_overlay_hard_cap(app: &AppHandle, generation: u64) {
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SNIP_HARD_CAP);
        let state = handle.state::<AppState>();
        if state.snip_gen.load(Ordering::SeqCst) != generation {
            return; // a newer round started; this timer is stale
        }
        if !state.snip_active.load(Ordering::SeqCst) {
            return; // finished normally (selection done or cancelled)
        }
        drop(state);
        eprintln!("snip round {generation} never finished within {} s; the overlay webview is probably dead — collapsing the overlay and releasing the state", SNIP_HARD_CAP.as_secs());
        trace(&format!("watchdog: hard cap hit on round {generation}; tearing down the stuck overlay"));
        teardown_overlay(&handle);
    });
}

/// Is the overlay still not showing this round, worth waiting another?
fn overlay_still_missing(app: &AppHandle, generation: u64) -> bool {
    let state = app.state::<AppState>();
    if state.snip_gen.load(Ordering::SeqCst) != generation {
        return false; // already a new round; this watchdog is stale
    }
    if !state.snip_active.load(Ordering::SeqCst) {
        return false; // already finished (selection done or cancelled), not "not showing"
    }
    let Some(win) = app.get_webview_window(OVERLAY) else {
        return false;
    };
    !win.is_visible().unwrap_or(false)
}

fn create_overlay(app: &AppHandle, pos: (f64, f64), size: (f64, f64)) -> Result<(), String> {
    // Build but don't show: wait for the frontend to paint the frozen frame before
    // show, so the user sees an "instant freeze" instead of a window that's first
    // white then bright.
    WebviewWindowBuilder::new(app, OVERLAY, WebviewUrl::App("overlay.html".into()))
        .title("ask-with-snip")
        .inner_size(size.0, size.1)
        .position(pos.0, pos.1)
        .decorations(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .visible(false)
        .build()
        .map_err(|e| format!("Failed to create the snip window: {e}"))?;

    hide_on_close(app, OVERLAY);
    Ok(())
}

/// Panel position: prefer the lower-right of the selection; flip to the other side at
/// screen edges; failing that, hug the screen edge.
///
/// Only used while the user hasn't moved the panel themselves (once moved, the
/// remembered position wins).
fn panel_position(frame: &Frame, crop_x: u32, crop_y: u32, crop_w: u32, crop_h: u32) -> (f64, f64) {
    let scale = frame.scale_factor.max(0.01) as f64;
    let screen_w = frame.width() as f64 / scale;
    let screen_h = frame.height() as f64 / scale;

    let sel_x = (frame.origin_x as f64 + crop_x as f64) / scale;
    let sel_y = (frame.origin_y as f64 + crop_y as f64) / scale;

    let mut x = sel_x + crop_w as f64 / scale + PANEL_GAP;
    if x + PANEL_W > screen_w {
        x = sel_x - PANEL_W - PANEL_GAP;
    }
    let mut y = sel_y + crop_h as f64 / scale + PANEL_GAP;
    if y + PANEL_H > screen_h {
        y = (screen_h - PANEL_H - PANEL_GAP).max(0.0);
    }

    (x.max(0.0), y.max(0.0))
}

/// Build the panel window.
///
/// Size/position prefer the last remembered values; only without them (first open)
/// use the suggested position or center.
fn create_panel(app: &AppHandle, suggested: Option<(f64, f64)>) -> Result<(), String> {
    let saved = restored_rect(app, PANEL);
    let size = saved
        .map(|r| (r.width as f64, r.height as f64))
        .unwrap_or((PANEL_W, PANEL_H));

    let mut builder = WebviewWindowBuilder::new(app, PANEL, WebviewUrl::App("panel.html".into()))
        .title("ask-with-snip")
        .inner_size(size.0, size.1)
        .min_inner_size(620.0, 380.0)
        .decorations(false)
        // Windows gives undecorated windows no native resize border; the resize handle
        // is drawn in the page (see panel.js)
        .resizable(true)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(true)
        .visible(false)
        // By default the Tauri drag-drop handler swallows file drops at the window
        // level (only the Rust onDragDrop event sees them). Disable it so the page
        // gets ordinary HTML5 dragover/drop events — that's what panel.js's
        // drop-onto-the-panel upload is built on. The title bar keeps dragging via
        // data-tauri-drag-region, which doesn't depend on this handler.
        .disable_drag_drop_handler();

    builder = match saved.or_else(|| {
        suggested.map(|(x, y)| config::WindowRect {
            x: x.round() as i32,
            y: y.round() as i32,
            ..Default::default()
        })
    }) {
        Some(rect) => builder.position(rect.x as f64, rect.y as f64),
        None => builder.center(),
    };

    builder
        .build()
        .map_err(|e| format!("Failed to create the result window: {e}"))?;

    // Safety net: the panel is supposed to be shown by the frontend calling panel_ready.
    // Any frontend exception leaves the panel invisible with no visible cause. So show
    // it on schedule (this show is a no-op on the normal path).
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if let Some(win) = handle.get_webview_window(PANEL) {
            if !win.is_visible().unwrap_or(false) {
                eprintln!("the panel did not become visible as expected; shown by fallback (the frontend may have errored; see the [webview] log above)");
                let _ = win.show();
            }
        }
    });

    hide_on_close(app, PANEL);
    Ok(())
}

/// Show the result panel (runs after a snip completes).
///
/// The panel window is **reused** across snips. The previous approach — "close the
/// old one, build a new one" — guaranteed that the second snip failed: close() is
/// async, the old window wasn't destroyed yet, and a new window with the same label
/// couldn't be created.
fn show_panel(app: &AppHandle, suggested: (f64, f64)) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(PANEL) {
        let _ = win.show();
        let _ = win.set_focus();
        // Tell the frontend to switch to the new conversation
        let _ = app.emit_to(PANEL, EVT_CONVERSATION_CHANGED, ());
        return Ok(());
    }
    create_panel(app, Some(suggested))
}

/// Open the panel to browse history (without snipping). Used by the tray menu and the
/// hub's "Conversation history".
fn open_history_panel(app: &AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(PANEL) {
        let _ = win.show();
        let _ = win.set_focus();
        let _ = app.emit_to(PANEL, EVT_CONVERSATION_CHANGED, ());
        return Ok(());
    }
    create_panel(app, None)
}

/// Raise a window to the front and give it focus, even when this process is not
/// the foreground process (opening from the tray menu, a hotkey, or a second
/// launch). In those cases Windows silently denies SetForegroundWindow, so the
/// window is created fine but stays buried under the active app — which users
/// read as "the page won't open". Flashing always-on-top around set_focus makes
/// the window manager accept the focus change; it is dropped again right after.
fn raise_window(app: &AppHandle, label: &'static str) {
    if let Some(win) = app.get_webview_window(label) {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_always_on_top(true);
        let _ = win.set_focus();
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            if let Some(win) = app.get_webview_window(label) {
                let _ = win.set_always_on_top(false);
            }
        });
    }
}

fn open_settings(app: &AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(SETTINGS) {
        // A window can outlive its webview (WebView2 died around creation time): the
        // window manager still lists it, but every operation on it silently no-ops, so
        // "raise the existing window" shows nothing and the user reads that as
        // "settings won't open" with no error anywhere. Probe the webview; a dead one
        // answers even is_visible() with an Err. Destroy the corpse and rebuild.
        if win.is_visible().is_ok() {
            raise_window(app, SETTINGS);
            return Ok(());
        }
        trace("settings: existing window's webview is dead; destroying it and rebuilding");
        let _ = win.destroy();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    let saved = restored_rect(app, SETTINGS);
    let size = saved
        .map(|r| (r.width as f64, r.height as f64))
        .unwrap_or((SETTINGS_W, SETTINGS_H));

    trace("settings: starting to create the settings window");
    // Same pitfall as ensure_webchat: after the old window is destroyed the registry
    // frees the slot asynchronously, so the first attempt at the same name hits
    // "already exists" — absorbed with short retries.
    let make_builder = || {
        let b = WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
            .title("ask-with-snip Settings")
            .inner_size(size.0, size.1)
            .min_inner_size(620.0, 520.0)
            .resizable(true);
        if let Some(rect) = saved {
            b.position(rect.x as f64, rect.y as f64)
        } else {
            b.center()
        }
    };
    let mut built = None;
    for _ in 0..10 {
        match make_builder().build() {
            Ok(_) => {
                built = Some(());
                            break;
            }
            Err(e) => {
                let text = e.to_string();
                if !text.contains("already exists") {
                    trace(&format!("settings: creation failed: {text}"));
                    return Err(format!("Failed to create the settings window: {e}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    if built.is_none() {
        trace("settings: still failing after 3 seconds of already-exists retries");
        return Err("Failed to create the settings window: the old window's registry slot never freed up".to_string());
    }

    hide_on_close(app, SETTINGS);
    // A newly built window may also land beneath the always-on-top panel (when created
    // from a panel click, Windows doesn't always bring the new window to the front);
    // raise it explicitly so the user can actually see it.
    raise_window(app, SETTINGS);
    Ok(())
}

/// Hub window. This tool normally has no main window, so new users don't know where
/// to start after launching; give them a visible, clickable entry point.
fn open_hub(app: &AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(HUB) {
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }

    let saved = restored_rect(app, HUB);
    let size = saved
        .map(|r| (r.width as f64, r.height as f64))
        .unwrap_or((HUB_W, HUB_H));

    let mut builder = WebviewWindowBuilder::new(app, HUB, WebviewUrl::App("hub.html".into()))
        .title("ask-with-snip")
        .inner_size(size.0, size.1)
        .min_inner_size(380.0, 320.0)
        .resizable(true);

    builder = match saved {
        Some(rect) => builder.position(rect.x as f64, rect.y as f64),
        None => builder.center(),
    };

    builder
        .build()
        .map_err(|e| format!("Failed to create the hub window: {e}"))?;

    hide_on_close(app, HUB);
    Ok(())
}

/// Build (or raise) the embedded web-chat window.
///
/// This path deliberately touches no API: the login state is kept by the webview
/// itself, so it needs no API key and is unaffected by "quota exhausted". Fully
/// decoupled from the automatic-ask pipeline.
///
/// The window is only hidden, never destroyed (see hide_on_close): the login state and
/// the on-page conversation survive, so the next open resumes the same page without
/// re-login.
///
/// `visible` distinguishes two uses: when the user clicks "Web chat" it must be
/// visible (they need to log in, watch, and inspect errors themselves); when the
/// driver runs in the background it must not pop up (the experience should match the
/// API path), so it's built hidden.
/// Build (or raise) the chat window for a given web account.
///
/// **One window per account, with its own WebView2 data directory** (EBWebView/<account>):
/// login states are independent, so DeepSeek, ChatGPT... can all be logged in
/// simultaneously and switching accounts needs no re-login. Window labels are also
/// per-account (webchat-<account>); the reuse rule is unchanged — hide, never destroy.
/// Extract the host from a URL (https://claude.ai/new → claude.ai). Deep links are
/// gated on the host: which site the record belongs to vs. which site the current
/// account is on; on mismatch, don't continue the context.
fn webchat_url_host(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn ensure_webchat(
    app: &AppHandle,
    visible: bool,
    site: &webchat::Site,
    account: &config::WebchatAccount,
) -> Result<tauri::WebviewWindow, String> {
    let label = webchat::window_label(&account.name);
    if let Some(win) = app.get_webview_window(&label) {
        // Windows are reused per account name, but the site is a mutable property of
        // the account (the dropdown can change any time). A host mismatch means this
        // window was opened on the old site (in-page navigation after login doesn't
        // change the host), so it must be destroyed and rebuilt — reusing it would
        // keep typing questions into the old site's page.
        let want_host = site
            .home
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        let cur_host = win
            .url()
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        if !cur_host.is_empty()
            && !cur_host.eq_ignore_ascii_case(&want_host)
        {
            trace(&format!(
                "web chat: account {}'s window is still on {}, switching to {}; rebuilding the window",
                account.name, cur_host, want_host
            ));
            let _ = win.destroy();
        } else {
            if visible {
                let _ = win.show();
                let _ = win.set_focus();
            }
            return Ok(win);
        }
    }
    // Keep only the **current account's** web window at a time: the CDP debug port is
    // process-level fixed (9400); with two accounts' browser processes coexisting only
    // one port works and input would land on the wrong page. Login states live on disk
    // per account and survive window close; just rebuild the window when switching back.
    for (other_label, other) in app.webview_windows() {
        if other_label.starts_with("webchat-") && other_label != label {
            let _ = other.destroy();
        }
    }

    let url: tauri::Url = site
        .home
        .parse()
        .map_err(|e| format!("Invalid web-chat URL ({}): {e}", site.home))?;

    let saved = restored_rect(app, WEBCHAT);
    let size = saved
        .map(|r| (r.width as f64, r.height as f64))
        .unwrap_or((WEBCHAT_W, WEBCHAT_H));

    // document.title changes are only observable here — Tauri doesn't sync them to
    // the window title by default, so this doesn't affect the title; it just leaves a
    // read-back channel for the driver (see webchat::on_title).
    let title_handle = app.clone();

    // Login state isolated per account: an independent data directory per account
    let data_dir = crate::data_root(app).join("EBWebView").join(&label);

    // Browser args: the defaults + optional proxy + **one fixed CDP debug port per account**.
    // The debug port is for the driver: Gemini/Claude's editors intercept synthetic
    // events; only real input (CDP Input.insertText) gets through (see cdp.rs top note).
    // Note additional_browser_args **replaces** the defaults, so include them.
    let cfg = config::load(app);
    let port = cdp::cdp_port(&account.name);
    let mut args = format!("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}");
    if account.proxy_for(&site.id) {
        if cfg.webchat_proxy.is_empty() {
            eprintln!("account {} is set to use a proxy, but no proxy address is filled in on the settings page; connecting directly", account.name);
        } else {
            args.push_str(&format!(" --proxy-server={}", cfg.webchat_proxy));
            trace(&format!("web chat: {} uses proxy {} on {}", account.name, site.id, cfg.webchat_proxy));
        }
    }
    // The environment variable is the path WebView2 actually reads (additional_browser_args
    // has no effect on --remote-debugging-port; verified ignored). One data directory per
    // account = a fresh browser process per window build, and setting the port in the env
    // right now is exactly when it gets read.
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        &args,
    );

    // destroy() takes effect **asynchronously** in Tauri: building a new window with the
    // same name right after the old one is destroyed, before the registry frees, hits
    // "already exists" on the first try (verified: takes two clicks when logging in or
    // switching back to an account). Short retries absorb the async gap.
    let make_builder = || {
        let handle = title_handle.clone();
        let b = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url.clone()))
            .title(format!("Web chat ({} · {})", site.label, account.name))
            .inner_size(size.0, size.1)
            .min_inner_size(420.0, 320.0)
            .resizable(true)
            .visible(visible)
            .data_directory(data_dir.clone())
            .on_document_title_changed(move |_win, title: String| {
                webchat::on_title(&handle, &title);
            })
            // Float above everything like the panel: the panel is always-on-top, and
            // without this the new window would be covered by its own panel
            .always_on_top(true)
            .additional_browser_args(&args);
        if let Some(rect) = saved {
            b.position(rect.x as f64, rect.y as f64)
        } else {
            b.center()
        }
    };
    let mut win = None;
    for _ in 0..10 {
        match make_builder().build() {
            Ok(w) => {
                win = Some(w);
                break;
            }
            Err(e) => {
                if !e.to_string().contains("already exists") {
                    return Err(format!("Failed to create the web-chat window: {e}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    let win = win.ok_or_else(|| {
        "Failed to create the web-chat window: the old window's registry slot never freed up (already exists for 3 seconds straight)".to_string()
    })?;
    if visible {
        let _ = win.set_focus();
    }

    hide_on_close(app, &label);
    Ok(win)
}

/// The startup toast sits in the bottom-right corner.
///
/// Extracted so it can be tested: the easiest thing to get wrong is boundaries like
/// "the screen is smaller than the window", where a negative coordinate sends the
/// window off-screen.
fn toast_position(screen_w: f64, screen_h: f64, origin: (f64, f64)) -> (f64, f64) {
    (
        origin.0 + (screen_w - TOAST_W - TOAST_MARGIN).max(0.0),
        origin.1 + (screen_h - TOAST_H - TOAST_BOTTOM).max(0.0),
    )
}

/// Build the toast window (hidden first; shown after the frontend paints the text, so
/// there's no white flash).
///
/// It doesn't take focus (focusable(false)), so it won't push away the window the user
/// is typing in. The cost: such windows **receive no mouse-hover or keyboard events**
/// (clicks do land, verified), which is why the toast page only implements
/// "click to dismiss", no hover pause and no Esc.
fn open_toast(app: &AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(TOAST) {
        let _ = win.show();
        return Ok(());
    }

    let geometry = capture::primary_geometry()?;
    let scale = geometry.scale_factor.max(0.01) as f64;
    let (x, y) = toast_position(
        geometry.width as f64 / scale,
        geometry.height as f64 / scale,
        (geometry.origin_x as f64 / scale, geometry.origin_y as f64 / scale),
    );

    WebviewWindowBuilder::new(app, TOAST, WebviewUrl::App("toast.html".into()))
        .title("ask-with-snip")
        .inner_size(TOAST_W, TOAST_H)
        .position(x, y)
        .decorations(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(true)
        .focusable(false)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|e| format!("Failed to create the startup toast: {e}"))?;

    hide_on_close(app, TOAST);
    Ok(())
}

/// Open a window from places "inside main-thread message handling", like tray menus.
///
/// Can't build the window in place — that hits the finish_selection pitfall (creating
/// a WebView synchronously inside a message handler deadlocks); it must be built on
/// another thread.
fn open_off_main_thread<F>(app: &AppHandle, what: &str, open: F)
where
    F: FnOnce(&AppHandle) -> Result<(), String> + Send + 'static,
{
    let handle = app.clone();
    let what = what.to_string();
    std::thread::spawn(move || {
        if let Err(err) = open(&handle) {
            eprintln!("Failed to open {what}: {err}");
        }
    });
}

// ---------------------------------------------------------------- IPC commands

/// The overlay needs the full-screen background image after loading, plus the pixel
/// size for coordinate conversion.
#[tauri::command]
async fn get_frame(state: tauri::State<'_, AppState>) -> Result<FramePayload, String> {
    // On capture failure the reason must be carried back here for the frontend to show
    // (careful not to nest the two locks)
    let capture_error = lock(&state.snip_error).clone();
    let guard = lock(&state.frame);
    let frame = match guard.as_ref() {
        Some(frame) => frame,
        None => {
            return Err(capture_error
                .unwrap_or_else(|| "No frame available. Press the hotkey again to take a screenshot.".to_string()))
        }
    };

    // The background image may be lossy (cosmetic only); cropping uses the lossless
    // in-memory frame, so what's sent to the model is unaffected.
    let started = history::now_millis();
    let data_url = capture::to_data_url_jpeg(&frame.image, 0, 92)?;
    trace(&format!(
        "get_frame: full-screen background is {} chars, encoded in {} ms",
        data_url.len(),
        (history::now_millis() - started).max(0)
    ));

    Ok(FramePayload {
        data_url,
        width: frame.width(),
        height: frame.height(),
        scale_factor: frame.scale_factor,
    })
}

/// The overlay has finished painting the frozen frame; show it.
#[tauri::command]
async fn overlay_ready(app: AppHandle) -> Result<(), String> {
    // This latency is worth recording: once it exceeds the watchdog's patience, the
    // frozen frame gets wrongly treated as "stale"
    let started = app
        .state::<AppState>()
        .snip_started_at
        .load(Ordering::SeqCst);
    let now = history::now_millis().max(0) as u64;
    trace(&format!(
        "overlay_ready: hotkey to overlay visible took {} ms",
        now.saturating_sub(started)
    ));

    if let Some(win) = app.get_webview_window(OVERLAY) {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    }
    // On Windows the foreground window is "whoever just received input wins": calling
    // set_focus the instant the window shows is sometimes refused (verified: same
    // process, sometimes it works, sometimes not). When it fails, the user's first
    // click is spent activating the window, looking like "clicked once, nothing
    // happened, kicked back to re-snip". So check again after a moment and request
    // focus once more if not foreground — the real foreground usually arrives after that.
    let handle = app.clone();
    std::thread::spawn(move || {
        for delay in [120u64, 260, 500] {
            std::thread::sleep(std::time::Duration::from_millis(delay));
            let Some(win) = handle.get_webview_window(OVERLAY) else {
                return;
            };
            match win.is_focused() {
                Ok(true) => return,
                _ => {
                    let _ = win.set_focus();
                }
            }
        }
        trace("overlay_ready: the overlay never got foreground focus (the first click will activate the window)");
    });
    Ok(())
}

/// Selection finished. Coordinates are physical pixels within the frozen frame.
///
/// Must be async (i.e. run off the main thread). Tauri's synchronous commands run
/// inline in the main thread's message handler, and calling
/// `WebviewWindowBuilder::build()` from there deadlocks: creating the WebView2 child
/// window needs the message pump running, but the message handler is holding the main
/// thread. Symptom: the window handle exists yet never shows, the whole main thread
/// hangs, even the tray stops responding. All similar commands must follow this rule.
#[tauri::command]
async fn finish_selection(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let result = finish_selection_inner(&app, &state, x, y, width, height).await;
    if result.is_err() {
        // On submit failure the snip state must be released. Otherwise snip_active
        // stays held and the hotkey looks broken: the user keeps snipping at a stale
        // overlay and nothing responds (they can't see the stderr reason). The
        // frontend also shows the failure reason and suggests Esc — but the hotkey at
        // least must let them retry.
        reset_snip_active(&app);
    }
    result
}

async fn finish_selection_inner(
    app: &AppHandle,
    state: &tauri::State<'_, AppState>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    trace(&format!(
        "finish_selection: entered x={x} y={y} w={width} h={height}"
    ));
    let cfg = config::load(app);

    let (image_b64, crop_w, crop_h, sent_w, sent_h, panel_pos) = {
        let guard = lock(&state.frame);
        // Reaching here with an empty frame means this round's frozen frame vanished
        // early (watchdog misfire, or the user re-pressed the hotkey starting a new
        // round). The message must say what to do next, not just "it's stale".
        let frame = guard
            .as_ref()
            .ok_or("This frame is no longer valid. Press Esc to close this window, then press the hotkey again to snip.")?;

        let cropped = capture::crop(frame, x, y, width, height)?;
        let (crop_w, crop_h) = (cropped.width(), cropped.height());
        trace(&format!("finish_selection: cropped {crop_w}x{crop_h}"));

        let image_b64 =
            capture::encode_jpeg_base64(&cropped, cfg.max_image_edge, cfg.jpeg_quality)?;
        trace(&format!(
            "finish_selection: encoded {} chars",
            image_b64.len()
        ));

        // Size actually sent: proportionally shrunk when the long edge exceeds the cap
        let long_edge = crop_w.max(crop_h);
        let (sent_w, sent_h) = if cfg.max_image_edge > 0 && long_edge > cfg.max_image_edge {
            let ratio = cfg.max_image_edge as f32 / long_edge as f32;
            (
                ((crop_w as f32 * ratio).round() as u32).max(1),
                ((crop_h as f32 * ratio).round() as u32).max(1),
            )
        } else {
            (crop_w, crop_h)
        };

        let panel_pos = panel_position(frame, x.max(0) as u32, y.max(0) as u32, crop_w, crop_h);

        (image_b64, crop_w, crop_h, sent_w, sent_h, panel_pos)
    };
    trace("finish_selection: frozen-frame lock released");

    // Where this shot goes: see should_append's comment. When starting a new
    // conversation, unsent images on the old strip are **moved into the new one** (no
    // image loss); the conversation boundary follows the "continue context" toggle.
    let continued = {
        let staging = state.staging.swap(false, Ordering::SeqCst);
        let mut guard = lock(&state.conversation);
        let new_image = StagedImage {
            image_b64,
            selected: true,
            crop_w,
            crop_h,
            sent_w,
            sent_h,
        };
        if should_append(staging, cfg.continue_conversation, guard.as_ref()) {
            match guard.as_mut() {
                Some(conv) => {
                    conv.stage(new_image);
                    conv.crop_w = crop_w;
                    conv.crop_h = crop_h;
                    conv.sent_w = sent_w;
                    conv.sent_h = sent_h;
                    true
                }
                None => {
                    // No current conversation (shouldn't happen: "+" only appears when
                    // the panel has screenshots); treat as a new conversation
                    *guard = Some(Conversation {
                        id: history::new_id(),
                        created_at: history::now_millis(),
                        staged: vec![new_image],
                        crop_w,
                        crop_h,
                        sent_w,
                        sent_h,
                        ..Default::default()
                    });
                    false
                }
            }
        } else {
            // New conversation: move unsent images from the old strip over (keeping
            // their selection states), nothing lost
            let mut staged = guard
                .as_mut()
                .map(|conv| std::mem::take(&mut conv.staged))
                .unwrap_or_default();
            staged.push(new_image);
            *guard = Some(Conversation {
                id: history::new_id(),
                created_at: history::now_millis(),
                staged,
                crop_w,
                crop_h,
                sent_w,
                sent_h,
                turns: Vec::new(),
                ..Default::default()
            });
            false
        }
    };
    trace(&format!(
        "finish_selection: {}",
        if continued {
            "appended to the current conversation"
        } else {
            "started a new conversation"
        }
    ));

    let result = show_panel(app, panel_pos);
    trace(&format!("finish_selection: show_panel returned {result:?}"));

    // Record "has been used successfully". Afterwards the hub window no longer pops
    // up on launch to intrude.
    if result.is_ok() && !cfg.has_used {
        let updated = AppConfig {
            has_used: true,
            ..cfg
        };
        if let Err(err) = config::save(app, &updated) {
            eprintln!("Failed to record the has-used state: {err}");
        }
    }

    result
}

/// The panel has rendered its preview image; show it.
#[tauri::command]
async fn panel_ready(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(PANEL) {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The full content of the current conversation. Returns None when there's no screenshot yet.
#[tauri::command]
async fn get_conversation(
    state: tauri::State<'_, AppState>,
) -> Result<Option<ConversationPayload>, String> {
    Ok(lock(&state.conversation)
        .as_ref()
        .map(ConversationPayload::from))
}

/// Start a new conversation (the sidebar's "New").
///
/// Turns already asked were persisted long ago; nothing to do here. Unsent screenshots
/// (staged) move into the new conversation, matching the "start a new conversation"
/// gesture. Old conversations with turns stay in history untouched.
#[tauri::command]
async fn new_conversation(
    state: tauri::State<'_, AppState>,
) -> Result<ConversationPayload, String> {
    let staged = lock(&state.conversation)
        .as_mut()
        .map(|conv| std::mem::take(&mut conv.staged))
        .unwrap_or_default();
    let conv = Conversation {
        id: history::new_id(),
        created_at: history::now_millis(),
        staged,
        ..Default::default()
    };
    let payload = ConversationPayload::from(&conv);
    *lock(&state.conversation) = Some(conv);
    Ok(payload)
}

/// History list (summaries only, no images, hence fast).
#[tauri::command]
async fn list_history(app: AppHandle) -> Result<Vec<history::Summary>, String> {
    Ok(history::list(&app))
}

/// Open a conversation from history and make it the current one — so follow-ups can
/// continue directly.
#[tauri::command]
async fn load_conversation(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ConversationPayload, String> {
    let conv = history::load(&app, &id)?;
    let payload = ConversationPayload::from(&conv);
    *lock(&state.conversation) = Some(conv);
    Ok(payload)
}

#[tauri::command]
async fn delete_conversation(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    history::delete(&app, &id)?;
    // If the deleted one was the current conversation, switch the panel back to the
    // "no screenshot yet" state
    let mut guard = lock(&state.conversation);
    if guard.as_ref().map(|c| c.id == id).unwrap_or(false) {
        *guard = None;
    }
    Ok(())
}

#[tauri::command]
async fn clear_history(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    history::clear(&app)?;
    *lock(&state.conversation) = None;
    Ok(())
}

/// Open the panel to browse history (tray menu and the hub's "Conversation history").
#[tauri::command]
async fn open_panel(app: AppHandle) -> Result<(), String> {
    open_history_panel(&app)
}

/// Abandon this snip (Esc or clicking cancel).
#[tauri::command]
async fn cancel_snip(app: AppHandle) -> Result<(), String> {
    teardown_overlay(&app);
    Ok(())
}

#[tauri::command]
async fn dismiss_overlay(app: AppHandle) -> Result<(), String> {
    teardown_overlay(&app);
    Ok(())
}

/// Right-click on the overlay choosing "Ask directly": no screenshot, no crop; open
/// the ask panel directly for a text-only question. The conversation boundary still
/// obeys the "continue context" toggle (continue = stay in the current conversation;
/// new = unsent images from the old strip carry over).
#[tauri::command]
async fn open_panel_direct(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let cfg = config::load(&app);
    let staging = state.staging.swap(false, Ordering::SeqCst);
    {
        let mut guard = lock(&state.conversation);
        if should_append(staging, cfg.continue_conversation, guard.as_ref()) {
            // Appending to the current conversation: no new images to collect, nothing to change
            if guard.as_mut().is_none() {
                *guard = Some(Conversation {
                    id: history::new_id(),
                    created_at: history::now_millis(),
                    ..Default::default()
                });
            }
        } else {
            // New conversation: move unsent images from the old strip over (keeping
            // their selection states), nothing lost
            let mut staged = guard
                .as_mut()
                .map(|conv| std::mem::take(&mut conv.staged))
                .unwrap_or_default();
            *guard = Some(Conversation {
                id: history::new_id(),
                created_at: history::now_millis(),
                staged,
                turns: Vec::new(),
                ..Default::default()
            });
        }
    }
    trace("open_panel_direct: entering the conversation without a screenshot");

    // The panel window may still be alive (minimized to tray, or the previous snip's
    // panel never closed): reuse — show + focus + broadcast one conversation-changed.
    // Calling create_panel directly would hit "already exists" (a build with the same
    // label always fails), and the error goes unnoticed as the overlay collapses —
    // the symptom is "right-click Ask directly does nothing" (verified). show_panel /
    // open_history_panel already have this reuse branch; add the same here.
    let result = if let Some(win) = app.get_webview_window(PANEL) {
        let _ = win.show();
        let _ = win.set_focus();
        let _ = app.emit_to(PANEL, EVT_CONVERSATION_CHANGED, ());
        Ok(())
    } else {
        // No crop box to position by; let create_panel fall back on its own (last
        // remembered rect / default position)
        create_panel(&app, None)
    };
    teardown_overlay(&app);

    // Record "has been used successfully" (same as finish_selection)
    if result.is_ok() && !cfg.has_used {
        let updated = AppConfig {
            has_used: true,
            ..cfg
        };
        if let Err(err) = config::save(&app, &updated) {
            eprintln!("Failed to record the has-used state: {err}");
        }
    }
    result
}

/// Collapse the overlay and release the frozen frame. The frozen frame plus the
/// full-screen background add up to dozens of MB; not releasing early is wasted memory.
///
/// Hide only, never destroy: the window is reused, and destroying it from the
/// overlay's own IPC call turns the window into a zombie — see hide_on_close.
fn teardown_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(OVERLAY) {
        let _ = win.hide();
    }
    let state = app.state::<AppState>();
    *lock(&state.frame) = None;
    state.snip_active.store(false, Ordering::SeqCst);
    // This round didn't complete (Esc / watchdog). The "+"-triggered append is voided,
    // otherwise the flag stays set and the next **normal** snip gets misread as an append.
    state.staging.store(false, Ordering::SeqCst);
}

/// Collapse the panel (user clicks ✕ or presses Esc). Hide only, never destroy — the
/// panel window is reused across snips.
#[tauri::command]
async fn hide_panel(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(PANEL) {
        remember_window_rect(&app, PANEL, &win);
        let _ = win.hide();
    }
    Ok(())
}

#[tauri::command]
async fn open_settings_window(app: AppHandle) -> Result<(), String> {
    open_settings(&app)
}

/// The startup toast has laid out its text; show it.
///
/// Show without activating: it's just a notification and must not push away the window
/// the user is working in.
#[tauri::command]
async fn toast_ready(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(TOAST) {
        win.show().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Collapse the startup toast (auto-dismiss, click, and Esc all come here).
#[tauri::command]
async fn hide_toast(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(TOAST) {
        let _ = win.hide();
    }
    Ok(())
}

/// Manually trigger a snip (the hub's "Start snip" and the panel's "Re-snip").
#[tauri::command]
async fn trigger_snip(app: AppHandle) -> Result<(), String> {
    spawn_snip(&app);
    Ok(())
}

/// The panel's "+" append mode: **stand by only, don't snip**.
///
/// Clicking marks the "next snip" as an append (see finish_selection's staging branch),
/// the panel hides itself and the user goes on with their business; when they want to
/// snip they press the hotkey (or click the tray), and the shot automatically enters
/// the current conversation's film strip before the panel pops back. Pressing Esc to
/// cancel the snip voids the mark too (see teardown_overlay), so it won't pollute the
/// next normal snip.
#[tauri::command]
async fn snip_for_staging(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.staging.store(true, Ordering::SeqCst);
    trace("snip_for_staging: standing by; the next snip will append to the film strip");
    Ok(())
}

/// Toggle a staged image's selection state, returning the new value (so the panel can
/// update the highlight directly).
#[tauri::command]
async fn toggle_staged(
    state: tauri::State<'_, AppState>,
    index: usize,
) -> Result<bool, String> {
    let mut guard = lock(&state.conversation);
    let conv = guard.as_mut().ok_or("No screenshot yet")?;
    conv.toggle_staged(index)
}

/// Remove a **not-yet-sent** image from the film strip (the ✕ on that panel card).
#[tauri::command]
async fn remove_staged(
    state: tauri::State<'_, AppState>,
    index: usize,
) -> Result<(), String> {
    let mut guard = lock(&state.conversation);
    let conv = guard.as_mut().ok_or("No screenshot yet")?;
    conv.remove_staged(index)?;
    trace(&format!("remove_staged: removed staged image #{}", index + 1));
    Ok(())
}

/// A file the user dropped onto the panel (or pasted) arrives here. The frontend has
/// already decoded the image, downscaled the long edge and re-encoded it to JPEG base64
/// (using the same limits as a snip, read via get_config), so this just stages it like
/// any freshly snipped shot — it lands on the film strip, selected, and goes out with
/// the next question.
///
/// No history save here, same as the snip path: staged-but-unsent images are persisted
/// only when the question carrying them is sent (see ask_inner).
#[tauri::command]
async fn stage_uploaded_image(
    state: tauri::State<'_, AppState>,
    image_b64: String,
    crop_w: u32,
    crop_h: u32,
    sent_w: u32,
    sent_h: u32,
) -> Result<(), String> {
    let mut guard = lock(&state.conversation);
    let new_image = StagedImage {
        image_b64,
        selected: true,
        crop_w,
        crop_h,
        sent_w,
        sent_h,
    };
    match guard.as_mut() {
        Some(conv) => conv.stage(new_image),
        None => {
            // No current conversation (e.g. fresh install, question asked as plain
            // text): the drop starts one, just like a first snip would
            *guard = Some(Conversation {
                id: history::new_id(),
                created_at: history::now_millis(),
                staged: vec![new_image],
                ..Default::default()
            });
        }
    }
    trace("stage_uploaded_image: dropped image added to the film strip");
    Ok(())
}

/// Remove an **already-sent** image (the ✕ on the dimmed film-strip cards).
///
/// This edits the record: that turn has one fewer image, so later "continue context"
/// follow-ups and regenerations won't include it. So persist right away — otherwise
/// it comes back after a restart.
#[tauri::command]
async fn remove_turn_image(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    turn: usize,
    image: usize,
) -> Result<(), String> {
    let mut guard = lock(&state.conversation);
    let conv = guard.as_mut().ok_or("No screenshot yet")?;
    conv.remove_turn_image(turn, image)?;
    trace(&format!(
        "remove_turn_image: removed image #{} of turn #{}",
        image + 1,
        turn + 1
    ));
    let snapshot = conv.clone();
    drop(guard);
    history::save(&app, &snapshot)
}

/// Close the window that invoked this command. Again hidden rather than destroyed;
/// see hide_on_close.
#[tauri::command]
async fn close_self(window: tauri::Window) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

/// The frontend reports uncaught exceptions here, which land on stderr.
///
/// The webview has no console; a frontend exception is a silent failure — the symptom
/// is "the window should appear but doesn't", extremely hard to diagnose. With this
/// channel, launching the app from a command line shows where the frontend failed.
#[tauri::command]
fn report_js_error(message: String) {
    eprintln!("[webview] {message}");
}

/// Open a link in the system default browser.
///
/// Links come from model output, i.e. untrusted content, so only http/https is
/// allowed, preventing `file://` or other protocols from being abused.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err("Only http/https links may be opened".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/c", "start", "", trimmed])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("Failed to open the link: {e}"))?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(trimmed)
            .spawn()
            .map_err(|e| format!("Failed to open the link: {e}"))?;
    }
    Ok(())
}

/// Write text to the system clipboard.
///
/// Uses arboard to call the system clipboard API directly instead of `cmd /c clip`.
/// The latter has a pitfall: clip.exe only recognizes UTF-16LE by the BOM at the start
/// of input, and that BOM gets written into the clipboard as content, so the user
/// pastes an invisible U+FEFF.
#[tauri::command]
async fn copy_text(text: String) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("Failed to open the clipboard: {e}"))?;
    clipboard
        .set_text(text)
        .map_err(|e| format!("Failed to write to the clipboard: {e}"))
}

/// Add/remove/rename/switch web accounts and toggle their proxy, all through this one
/// **dedicated command**.
///
/// Previously the panel sent the whole config back through save_config, and the
/// changes never reached disk (very painful to diagnose). The dedicated channel only
/// passes the account list and the active account; every other field is taken from
/// disk as-is — the panel can't change what it shouldn't, and when something breaks we
/// know it's this command. `accounts` is received as serde_json::Value: any field-name
/// mismatch errors here instead of failing silently.
#[tauri::command]
async fn update_webchat_accounts(
    app: AppHandle,
    accounts: serde_json::Value,
    active: String,
) -> Result<(), String> {
    let mut cfg = config::load(&app);
    let parsed: Vec<config::WebchatAccount> = serde_json::from_value(accounts)
        .map_err(|e| format!("Invalid account data: {e}"))?;
    if parsed.is_empty() {
        return Err("At least one account must remain.".into());
    }
    if !parsed.iter().any(|a| a.name == active) {
        return Err("The account to activate does not exist.".into());
    }
    // After toggling a proxy / changing a site, the old window still carries the old
    // browser args (the proxy is a process-level arg; it can't be changed, only
    // rebuilt). Destroy the affected web windows so the next "Show web" or question
    // reopens with the new config — otherwise the user stares at a 404 page clicking
    // refresh to no effect.
    let old_proxy: Vec<(String, String)> = cfg
        .accounts
        .iter()
        .map(|a| (a.name.clone(), a.site.clone()))
        .collect();
    let old_map: std::collections::HashMap<String, (String, bool)> = cfg
        .accounts
        .iter()
        .map(|a| {
            (
                a.name.clone(),
                (a.site.clone(), a.proxy_for(&a.site)),
            )
        })
        .collect();
    cfg.accounts = parsed;
    cfg.active_account = active;
    let wrote = config::save(&app, &cfg);
    trace(&format!(
        "update_webchat_accounts: {} accounts, active {}, persisted {:?}",
        cfg.accounts.len(),
        cfg.active_account,
        wrote.as_ref().ok()
    ));
    // Accounts whose proxy selection (account×site) changed: destroy their windows
    for acc in &cfg.accounts {
        let changed = match old_map.get(&acc.name) {
            Some((site, old_proxy)) => *old_proxy != acc.proxy_for(&acc.site) || *site != acc.site,
            None => true, // new account: no window, nothing to do
        };
        if changed {
            let label = webchat::window_label(&acc.name);
            if let Some(win) = app.get_webview_window(&label) {
                trace(&format!("update_webchat_accounts: {}'s proxy/site changed; rebuilding the window", acc.name));
                let _ = win.hide();
                let _ = win.destroy();
            }
        }
    }
    let _ = old_proxy;
    wrote
}

/// Delete a web account's login state (its own WebView2 data directory).
///
/// If the window is still open the directory is locked — so hide the window first and
/// best-effort delete; whatever can't be deleted stays on disk (the account is already
/// gone from the config, so it doesn't affect use) and gets cleaned on next launch.
#[tauri::command]
async fn delete_webchat_account(app: AppHandle, name: String) -> Result<(), String> {
    let label = webchat::window_label(&name);
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.hide();
        let _ = win.destroy();
    }
    let dir = crate::data_root(&app).join("EBWebView").join(&label);
    if dir.exists() {
        if let Err(err) = std::fs::remove_dir_all(&dir) {
            trace(&format!("Failed to delete account {name}'s login-state directory (if a window holds it, it will be cleaned on next launch): {err}"));
        }
    }
    Ok(())
}

/// Bring the web-chat window into view: the user needs to see what it's doing
/// (logging in, passing a check, a site redesign). While the driver runs in the
/// background the window is hidden; when something goes wrong this button is the only
/// way into the scene.
#[tauri::command]
async fn reveal_webchat(app: AppHandle) -> Result<(), String> {
    let cfg = config::load(&app);
    webchat::reveal(&app, &cfg.active_webchat())
}

/// Reload the web-chat window's current page.
///
/// When to use: white-screened window, stuck on a verification page, proxy just
/// changed — when the page won't load, "Show web" can only look, not fix, and the user
/// would have to delete the account and start over; a refresh that can always be
/// pressed is enough. Uses `eval("location.reload()")`: the remote page has no Tauri
/// privileges, and reload is an action it can execute by itself without touching the
/// privilege boundary. When the window was never built, say so explicitly instead of
/// failing silently.
#[tauri::command]
async fn reload_webchat(app: AppHandle) -> Result<(), String> {
    let cfg = config::load(&app);
    let label = webchat::window_label(&cfg.active_webchat().name);
    let win = app
        .get_webview_window(&label)
        .ok_or_else(|| "The web-chat window has never been opened; click \"Show web\" first to bring it up".to_string())?;
    win.eval("location.reload()")
        .map_err(|e| format!("Reload failed: {e}"))
}

/// Ask a question. The first turn carries images; follow-ups are text-only — the
/// images are already in context, resending is both expensive and slow.
#[tauri::command]
async fn ask(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    prompt: String,
) -> Result<String, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("Please enter your question first.".into());
    }
    start_ask(app, state, AskWhat::New(prompt)).await
}

/// For retrying failures: put a turn's images back into the staging area.
///
/// When a new question fails, Rust removes that turn together with its images (the
/// images taken by take_selected_staged would be gone); before the panel's
/// "Regenerate / Edit question" resends, it returns the images first, otherwise the
/// retry becomes a text-only question with no image. Images come in as data urls;
/// the prefix is stripped and base64 stored.
#[tauri::command]
async fn restage_images(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    images: Vec<String>,
) -> Result<(), String> {
    let mut guard = lock(&state.conversation);
    let conv = guard.as_mut().ok_or("Conversation state is invalid")?;
    conv.staged = images
        .iter()
        .map(|url| {
            let b64 = url
                .split_once("base64,")
                .map(|(_, rest)| rest)
                .unwrap_or(url);
            StagedImage {
                image_b64: b64.to_string(),
                selected: true,
                crop_w: 0,
                crop_h: 0,
                sent_w: 0,
                sent_h: 0,
            }
        })
        .collect();
    Ok(())
}

/// Switch the UI language (the panel's "中/EN" toggle). Persisted immediately; the
/// settings page follows on its next open.
#[tauri::command]
async fn set_language(app: AppHandle, lang: String) -> Result<(), String> {
    let mut cfg = config::load(&app);
    cfg.language = if lang == "en" { "en".into() } else { "zh".into() };
    let wrote = config::save(&app, &cfg);
    trace(&format!("set_language: {}, persisted {:?}", cfg.language, wrote.as_ref().ok()));
    wrote
}

/// Save the panel's quick-ask preset buttons (right-click to edit, ＋ to add; whole
/// table replaced). Each entry is { name, prompt }; an empty prompt = use the name as
/// the prompt when sending.
#[tauri::command]
async fn set_presets(app: AppHandle, presets: Vec<config::PresetEntry>) -> Result<(), String> {
    let mut cfg = config::load(&app);
    cfg.presets = presets;
    // The flat field mirrors the active list; keep them in sync so an older
    // panel (or another reader) never sees a stale copy.
    if let Some(list) = cfg
        .preset_lists
        .iter_mut()
        .find(|l| l.name == cfg.active_preset_list)
    {
        list.presets = cfg.presets.clone();
    }
    let wrote = config::save(&app, &cfg);
    trace(&format!("set_presets: {} entries, persisted {:?}", cfg.presets.len(), wrote.as_ref().ok()));
    wrote
}

/// Save the whole quick-prompt-list set (name + entries per list) plus which one
/// is active. Lists, their contents and the remembered selection are all
/// persisted so the panel reopens on the list the user left active.
#[tauri::command]
async fn set_preset_lists(
    app: AppHandle,
    lists: Vec<config::PresetList>,
    active: String,
) -> Result<(), String> {
    let mut cfg = config::load(&app);
    cfg.preset_lists = lists;
    cfg.active_preset_list = active;
    // Keep the legacy flat field in sync with the newly active list
    if let Some(list) = cfg
        .preset_lists
        .iter()
        .find(|l| l.name == cfg.active_preset_list)
    {
        cfg.presets = list.presets.clone();
    }
    let wrote = config::save(&app, &cfg);
    trace(&format!(
        "set_preset_lists: {} lists, active {:?}, persisted {:?}",
        cfg.preset_lists.len(),
        cfg.active_preset_list,
        wrote.as_ref().ok()
    ));
    wrote
}

/// Regenerate: keep the last turn's question, redo the answer.
#[tauri::command]
async fn regenerate(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<String, String> {
    start_ask(app, state, AskWhat::Regenerate).await
}

/// Edit question: replace the last turn's question with a new one and ask again
/// (answer redone).
#[tauri::command]
async fn edit_question(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    prompt: String,
    images: Option<Vec<String>>,
) -> Result<String, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("The question cannot be empty.".into());
    }
    // images: the surviving screenshots picked in the editor; None = untouched (keep
    // the original set), Some = replace wholesale (possibly empty = text-only re-ask)
    //
    // The panel echoes back the data urls it displays (`data:image/jpeg;base64,...`),
    // and images it newly picked from local files carry the same prefix. Records store
    // bare base64 (api.rs re-adds the prefix when sending), so normalize before the
    // list lands in the turn — otherwise a re-asked turn keeps full data urls and the
    // next request sends a doubly-prefixed, broken image.
    let images = images.map(|list| {
        list.into_iter()
            .map(|image| match image.rsplit_once(',') {
                // A data url: everything after the last comma is the payload
                Some((header, payload)) if header.starts_with("data:") => payload.to_string(),
                _ => image,
            })
            .collect()
    });
    start_ask(app, state, AskWhat::Edit { prompt, images }).await
}

/// What to do with the last turn this round.
enum AskWhat {
    /// A new question (regular ask after a snip)
    New(String),
    /// Re-ask the previous turn's question, redo the answer
    Regenerate,
    /// Change the previous turn's question and ask again
    Edit {
        prompt: String,
        /// Some = replace the turn's images with this list; None = keep them
        images: Option<Vec<String>>,
    },
}

impl AskWhat {
    fn is_new(&self) -> bool {
        matches!(self, AskWhat::New(_))
    }
}

/// Shared shell for the three actions (ask / regenerate / edit question): set busy,
/// record the time, release when done.
async fn start_ask(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    what: AskWhat,
) -> Result<String, String> {
    if state.busy.swap(true, Ordering::SeqCst) {
        return Err("The previous question is still in progress; please wait.".into());
    }
    state
        .ask_started_at
        .store(history::now_millis().max(0) as u64, Ordering::SeqCst);
    let result = ask_inner(&app, &state, what).await;
    state.busy.store(false, Ordering::SeqCst);
    result
}

/// Cancel the current question.
///
/// Two things at once: set the cancel flag (the stream-reading end sees it on its next
/// loop iteration), and stop the web-chat side too (otherwise it keeps generating in
/// the background, burning the account's quota). Already-received partial output stays
/// on the panel as usual; cancelling doesn't wipe it.
#[tauri::command]
async fn cancel_ask(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    if !state.busy.load(Ordering::SeqCst) {
        return Ok(()); // not running in the first place; treat as a no-op
    }
    lock(&state.cancel).cancel();
    trace("cancel_ask: cancel flag set");
    // Also stop the current account's web-chat window (nothing to stop when it's closed)
    let cfg = config::load(&app);
    let label = webchat::window_label(&cfg.active_webchat().name);
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.eval(webchat::stop_js());
    }
    Ok(())
}

async fn ask_inner(
    app: &AppHandle,
    state: &tauri::State<'_, AppState>,
    what: AskWhat,
) -> Result<String, String> {
    let cfg = config::load(app);

    // The web-chat path needs no API config — that's the whole point of the path (you
    // can still ask after quota runs out). So the "is the API configured" check only
    // applies to the API path, and as usual happens before touching the conversation.
    let use_webchat = cfg.uses_webchat();
    let profile = if use_webchat {
        None
    } else {
        Some(
            cfg.active()
                .cloned()
                .ok_or("No API configured yet. Open Settings, fill it in, and save first.")?,
        )
    };

    // Is this a fresh turn or a follow-up: the web-chat side uses it to decide between
    // "start a new chat" and "continue this chat". Images follow the turn too — the
    // freshly snipped ones belong to this turn; follow-ups carry none.
    //
    // Regenerate / edit question both operate on the **last turn**: clear its answer
    // (edit also replaces the question), then proceed as usual — the "rebuild the
    // request from turns" logic below naturally asks again. The turn's pre-edit state
    // is kept so it can be restored verbatim on failure (see the error branch at the
    // end of the file).
    //
    // From here on images are a Vec: however many are selected in the film strip is
    // how many this turn carries.
    let (conversation_id, open_new_chat, images, prompt, resume_url, restore) = {
        let mut guard = lock(&state.conversation);
        // Text-only questions work without a screenshot: with no conversation, open an
        // empty one on the spot (same as open_panel_direct)
        let conv = match guard.as_mut() {
            Some(conv) => conv,
            None => guard.insert(Conversation {
                id: history::new_id(),
                created_at: history::now_millis(),
                ..Default::default()
            }),
        };

        // Keep the turn's pre-edit state: restore it verbatim on failure; don't
        // corrupt the record
        let restore = conv.turns.last().cloned();

        let prompt = match &what {
            AskWhat::New(prompt) => {
                // The selected staged images belong to this turn; unselected ones stay staged
                let images = conv.take_selected_staged();
                conv.start_turn(prompt, images);
                prompt.clone()
            }
            AskWhat::Regenerate => {
                if conv.turns.is_empty() {
                    return Err("There is no question to regenerate yet.".into());
                }
                conv.clear_last_answer()
                    .ok_or("There is no question to regenerate yet.")?
            }
            AskWhat::Edit { prompt, images } => {
                if conv.turns.is_empty() {
                    return Err("There is no question to edit yet.".into());
                }
                conv.rewrite_last_turn(prompt, images.clone())
                    .ok_or("There is no question to edit yet.")?
            }
        };

        let images = conv
            .turns
            .last()
            .map(|turn| turn.all_images())
            .unwrap_or_default();
        // Web chat: if this conversation has a corresponding session on the site
        // (recorded in webchat_url right after each turn), deep-link straight back and
        // keep chatting there — the site has the context, so earlier turns are **not
        // replayed one by one** (opening an old record and asking, regenerating, and
        // "continue context" all take this path). Without a URL (old records, or the
        // previous question got no answer), start a new chat — better to lose context
        // than to mix into another session.
        // **The host must match the current account's site**: the record was a Gemini
        // chat but the account switched to Claude — deep-linking would navigate
        // Claude's window to a Gemini session, mixing everything up.
        let resume_url = if use_webchat {
            let cur_host = webchat_url_host(webchat::site_by_id(&cfg.active_webchat().site).home);
            let conv_host = webchat_url_host(&conv.webchat_url);
            if !conv.webchat_url.is_empty() && conv_host.eq_ignore_ascii_case(&cur_host) {
                conv.webchat_url.clone()
            } else {
                if !conv.webchat_url.is_empty() {
                    trace(&format!(
                        "web chat: the record's session is on {} but the current account is on {}; not deep-linking, starting a new chat",
                        conv_host, cur_host
                    ));
                }
                String::new()
            }
        } else {
            String::new()
        };
        let open_new_chat = resume_url.is_empty()
            && if what.is_new() { conv.turns.len() <= 1 } else { true };

        (
            conv.id.clone(),
            open_new_chat,
            images,
            prompt,
            resume_url,
            restore,
        )
    };

    // The cancel flag for this turn. A fresh one each turn: the previous turn's cancel
    // must not affect this one
    let cancel = {
        let mut guard = lock(&state.cancel);
        *guard = api::Cancel::new();
        guard.clone()
    };

    let answer = match &profile {
        // The API path: rebuild the request from the turns; each turn's images follow
        // its own question
        Some(profile) => {
            let messages = {
                let guard = lock(&state.conversation);
                let conv = guard.as_ref().ok_or("Conversation state is invalid")?;
                api::messages_from_turns(&cfg.system_prompt, &conv.turns)
            };
            api::stream_chat(app, profile, cfg.timeout_secs, messages, &cancel)
                .await
                .map(|answer| (answer, String::new()))
        }
        // The web-chat path: hand it to the driver to operate the page. Context is
        // maintained by the site itself (same conversation continued); this side only
        // passes this turn's images and question.
        None => {
            let account = cfg.active_webchat();
            let site = webchat::site_by_id(&account.site);
            let turn = webchat::Turn {
                prompt,
                images,
                new_chat: open_new_chat,
                resume: if resume_url.is_empty() {
                    None
                } else {
                    Some(resume_url)
                },
            };
            let emitter = app.clone();
            webchat::run_turn(app, cfg.timeout_secs, site, &account, turn, &cancel, move |out| match out {
                webchat::Out::Delta(text) => {
                    let _ = emitter.emit(api::EVT_DELTA, text);
                }
                webchat::Out::Reasoning(text) => {
                    let _ = emitter.emit(api::EVT_REASONING, text);
                }
                webchat::Out::Replace { answer, reasoning } => {
                    // Only touch the fields provided: the other stream's content is still valid
                    let payload = serde_json::json!({
                        "answer": answer,
                        "reasoning": reasoning,
                    });
                    let _ = emitter.emit(api::EVT_REPLACE, payload);
                }
            })
            .await
            .map(|outcome| {
                (outcome.answer, outcome.site_url)
            })
        }
    };

    match answer {
        Ok((answer, site_url)) => {
            // The user may have snipped again meanwhile (switching conversations), so
            // verify the id before writing the answer onto the new conversation
            let snapshot = {
                let mut guard = lock(&state.conversation);
                match guard.as_mut() {
                    Some(conv) if conv.id == conversation_id => {
                        if let Some(pending) = conv.turns.last_mut() {
                            if pending.answer.trim().is_empty() {
                                pending.answer = answer.clone();
                            }
                        }
                        // After a web-chat turn, record the site session URL: later
                        // follow-ups, regenerations, and "continue context" all
                        // deep-link back to it (don't overwrite a non-empty old one)
                        if !site_url.is_empty() {
                            conv.webchat_url = site_url;
                        }
                        Some(conv.clone())
                    }
                    _ => None,
                }
            };

            match snapshot {
                Some(conv) => {
                    if let Err(err) = history::save(app, &conv) {
                        eprintln!("Failed to save the conversation record: {err}");
                    }
                }
                None => trace("the current conversation was switched away while asking; the answer was only handed to the frontend to display"),
            }

            let _ = app.emit(api::EVT_DONE, answer.clone());
            Ok(answer)
        }
        Err(err) => {
            let mut guard = lock(&state.conversation);
            if let Some(conv) = guard.as_mut() {
                if conv.id == conversation_id {
                    if what.is_new() {
                        // New question failed: retract the turn just pushed, so history
                        // doesn't keep a question with no answer
                        if conv
                            .turns
                            .last()
                            .map(|t| t.answer.trim().is_empty())
                            .unwrap_or(false)
                        {
                            conv.turns.pop();
                        }
                    } else if let Some(before) = restore {
                        // Regenerate / edit question failed: put that turn back
                        // **verbatim** (answer included). Popping would lose the user's
                        // edited question; and since this turn was never persisted,
                        // restoring it in memory leaves it exactly consistent with the
                        // on-disk copy.
                        if let Some(last) = conv.turns.last_mut() {
                            *last = before;
                        }
                    }
                }
            }
            let _ = app.emit(api::EVT_ERROR, err.clone());
            Err(err)
        }
    }
}

#[tauri::command]
fn get_config(app: AppHandle) -> AppConfig {
    config::load(&app)
}

/// Location of the config file, shown to the user on the settings page.
#[tauri::command]
fn get_config_path(app: AppHandle) -> String {
    config::config_path_display(&app)
}

#[tauri::command]
async fn save_config(app: AppHandle, cfg: AppConfig) -> Result<(), String> {
    // Try registering the hotkey first (failures are usually a transient "registration
    // not released" conflict; a retry fixes it), but **never let it interrupt the
    // whole save** — switching accounts, toggling proxies, deleting accounts on the
    // panel all come through here; if registration fails and aborts, all those
    // changes are silently lost (verified: config.json went seven hours without
    // hitting disk, every checkbox in vain). If the hotkey doesn't register, emit an
    // event to the frontend and persist the config anyway.
    if let Err(err) = register_hotkey(&app, &cfg.hotkey) {
        let _ = app.emit(EVT_HOTKEY_ERROR, err.clone());
        trace(&format!("save_config: hotkey registration failed (config saved anyway): {err}"));
    }
    config::save(&app, &cfg)?;
    // Every open window (panel / hub / settings / toast / overlay) follows on its own;
    // windows opened later read the config fresh anyway.
    let _ = app.emit(EVT_THEME_CHANGED, cfg.theme.clone());
    trace(&format!(
        "save_config: persisted ({} accounts, active {})",
        cfg.accounts.len(),
        cfg.active_account
    ));
    apply_autostart(&app, cfg.autostart)?;
    Ok(())
}

/// The settings page's "Test connection": try one request with the currently selected
/// profile.
#[tauri::command]
async fn test_api(cfg: AppConfig) -> Result<String, String> {
    let profile = cfg
        .active()
        .ok_or("No usable profile yet. Fill in the API URL and API Key first.")?
        .clone();
    api::test_connection(&profile, cfg.timeout_secs).await
}

/// Fetch the provider's model list (GET /models) for the model-name dropdown on
/// the settings page. Runs after "Test connection" succeeded.
#[tauri::command]
async fn list_models_api(cfg: AppConfig) -> Result<Vec<String>, String> {
    let profile = cfg
        .active()
        .ok_or("No usable profile yet. Fill in the API URL and API Key first.")?
        .clone();
    api::list_models(&profile, cfg.timeout_secs).await
}

#[tauri::command]
async fn quit_app(app: AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

// ---------------------------------------------------------------- Hotkey / autostart

fn register_hotkey(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    let manager = app.global_shortcut();
    let _ = manager.unregister_all();

    let shortcut = hotkey
        .trim()
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|e| format!("Invalid hotkey format (example: Ctrl+Shift+A): {e}"))?;

    manager
        .register(shortcut)
        .map_err(|e| format!("Failed to register the hotkey; it may be taken by other software: {e}"))
}

const AUTOSTART_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_NAME: &str = "ask-with-snip";

/// Launch at startup. On Windows, write the registry Run key directly — far lighter
/// than pulling in a plugin.
#[cfg(windows)]
fn apply_autostart(_app: &AppHandle, enable: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut command = Command::new("reg");
    command.creation_flags(CREATE_NO_WINDOW);

    if enable {
        let exe = std::env::current_exe().map_err(|e| format!("Cannot get the program path: {e}"))?;
        command
            .args([
                "add",
                AUTOSTART_KEY,
                "/v",
                AUTOSTART_NAME,
                "/t",
                "REG_SZ",
                "/d",
            ])
            .arg(format!("\"{}\"", exe.display()))
            .arg("/f");
    } else {
        command.args(["delete", AUTOSTART_KEY, "/v", AUTOSTART_NAME, "/f"]);
    }

    let output = command.output().map_err(|e| format!("Failed to run reg: {e}"))?;
    if enable && !output.status.success() {
        return Err(format!(
            "Failed to write the launch-at-startup entry: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // When disabling, if the entry never existed reg returns non-zero, which is not an error
    Ok(())
}

#[cfg(not(windows))]
fn apply_autostart(_app: &AppHandle, _enable: bool) -> Result<(), String> {
    Ok(())
}

// ---------------------------------------------------------------- Startup

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// ---------------------------------------------------------------- Portable data (exe-side data)
///
/// When writable, all user data (config, history, window rects, trace log, WebView2
/// profiles) lives in `<exe dir>\data\` so the whole install can be carried on a USB
/// stick or a moved folder. When that directory cannot be created (exe on a read-only
/// location), the code falls back to the per-user AppData directories as before.
///
/// Existing data is migrated from Roaming once (see migrate_old_portable_data). The
/// webchat login state (EBWebView, possibly hundreds of MB) is NOT auto-migrated:
/// re-login once, or copy `AppData\Local\com.askwithsnip.desktop\EBWebView` into
/// `<exe dir>\data\EBWebView` while the app is not running.
static PORTABLE_DATA: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn init_portable_data() {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data")));
    if let Some(dir) = dir {
        if std::fs::create_dir_all(&dir).is_ok() {
            migrate_old_portable_data(&dir);
            let _ = PORTABLE_DATA.set(Some(dir));
        }
    }
}

fn portable_data() -> Option<&'static std::path::PathBuf> {
    PORTABLE_DATA.get().and_then(|o| o.as_ref())
}

/// The writable data root: `<exe dir>\data` in portable mode, the per-user config
/// directory otherwise. Every data path in the app is built from this.
fn data_root(app: &AppHandle) -> std::path::PathBuf {
    if let Some(root) = portable_data() {
        return root.clone();
    }
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("com.askwithsnip.desktop"))
}

/// One-time migration of the small files from the old Roaming directory. The
/// WebView2 login-state tree is intentionally left alone (size; see module comment).
fn migrate_old_portable_data(new_root: &std::path::Path) {
    if new_root.join("config.json").exists() {
        return; // this data directory is already in use
    }
    let Ok(appdata) = std::env::var("APPDATA") else { return };
    let old = std::path::PathBuf::from(appdata).join("com.askwithsnip.desktop");
    if !old.exists() {
        return;
    }
    for name in ["config.json", "windows.json", "history"] {
        let src = old.join(name);
        let dst = new_root.join(name);
        if src.exists() && !dst.exists() {
            if let Err(err) = copy_recursive(&src, &dst) {
                eprintln!("portable data: migrating {name} failed: {err}");
            }
        }
    }
}

fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::create_dir_all(dst.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent")
        })?)?;
        std::fs::copy(src, dst)?;
        Ok(())
    }
}

pub fn run() {
    init_portable_data();
    // With portable data, the main windows' WebView2 profile lives under
    // <exe dir>\data\EBWebView too (the webchat windows override data_directory
    // per account beneath the same root, see ensure_webchat). Must be set before
    // any WebView2 is created.
    if let Some(root) = portable_data() {
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", root.join("EBWebView"));
    }
    // WebView2's CDP debug port can only be declared via an environment variable (the
    // builder-arg form is verified ignored). Fixed at 9400: only one web account's
    // window uses it at a time (switching accounts closes the old window, see
    // ensure_webchat), so the port never conflicts. Other windows having a debug port
    // doesn't matter (it only listens on 127.0.0.1).
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--remote-debugging-port=9400 --remote-allow-origins=*",
    );
    tauri::Builder::default()
        .manage(AppState::default())
        // Fool-proofing: only one instance at a time. When the icon is clicked again,
        // the second process exits right here after the callback, and the running
        // instance brings the hub window to the front — the user sees the window pop
        // up and knows the app was already running, rather than "clicked, nothing
        // happened". Note it must be the first registered plugin; the official docs
        // require initializing it before other plugins.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            open_off_main_thread(app, "hub", open_hub);
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        spawn_snip(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_frame,
            overlay_ready,
            finish_selection,
            panel_ready,
            get_conversation,
            new_conversation,
            list_history,
            load_conversation,
            delete_conversation,
            clear_history,
            open_panel,
            cancel_snip,
            dismiss_overlay,
            hide_panel,
            open_settings_window,
            toast_ready,
            hide_toast,
            trigger_snip,
            close_self,
            report_js_error,
            open_url,
            copy_text,
            reveal_webchat,
            reload_webchat,
            delete_webchat_account,
            update_webchat_accounts,
            ask,
            regenerate,
            edit_question,
            restage_images,
            set_language,
            set_presets,
            set_preset_lists,
            list_models_api,
            open_panel_direct,
            snip_for_staging,
            toggle_staged,
            remove_staged,
            stage_uploaded_image,
            remove_turn_image,
            cancel_ask,
            get_config,
            get_config_path,
            save_config,
            test_api,
            quit_app,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // Logs go into the config directory (trace can't get the app handle; hand
            // it the directory here)
            set_trace_dir(crate::data_root(&handle));
            // Re-point the Run-key autostart at this exe: after the exe is moved or
            // rebuilt elsewhere, the registry would keep launching the old location
            let _ = apply_autostart(&handle, config::load(&handle).autostart);

            // For diagnosis/automation: with SNAPI_AUTOSNIP=1, automatically enter one
            // snip after launch so verifying the flow doesn't require a human to press
            // the hotkey. Unaffected otherwise.
            if std::env::var_os("SNAPI_AUTOSNIP").is_some() {
                let autosnip = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    trace("SNAPI_AUTOSNIP: automatically triggering one snip");
                    spawn_snip(&autosnip);
                });
            }

            let snip = MenuItem::with_id(&handle, "snip", "Start snip", true, None::<&str>)?;
            let history_item =
                MenuItem::with_id(&handle, "history", "Conversation history", true, None::<&str>)?;
            let hub = MenuItem::with_id(&handle, "hub", "Open hub", true, None::<&str>)?;
            let settings = MenuItem::with_id(&handle, "settings", "Settings…", true, None::<&str>)?;
            let quit = MenuItem::with_id(&handle, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(
                &handle,
                &[
                    &snip,
                    &history_item,
                    &hub,
                    &PredefinedMenuItem::separator(&handle)?,
                    &settings,
                    &quit,
                ],
            )?;

            // Tray icon: Tauri/tray-icon scales with nearest-neighbor internally;
            // feeding a large image directly produces jaggies. So query the system's
            // exact tray size (varies with DPI) and pre-scale with Lanczos3 before
            // handing it to the tray.
            let tray_icon = {
                // Safety: GetSystemMetrics has no special preconditions
                let size = unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                        windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
                    )
                } as u32;
                let src = image::load_from_memory(include_bytes!("../icons/icon.png"))?
                    .to_rgba8();
                let scaled = image::imageops::resize(
                    &src, size, size, image::imageops::FilterType::Lanczos3,
                );
                let (w, h) = scaled.dimensions();
                tauri::image::Image::new_owned(scaled.into_raw(), w, h)
            };

            let mut tray = TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .menu(&menu)
                .tooltip("ask-with-snip — snip a screenshot and ask")
                // Left click snips directly; right click opens the menu
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "snip" => spawn_snip(app),
                    "history" => open_off_main_thread(app, "conversation history", open_history_panel),
                    "hub" => open_off_main_thread(app, "hub", open_hub),
                    "settings" => open_off_main_thread(app, "settings", open_settings),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        spawn_snip(tray.app_handle());
                    }
                });

            tray.build(&handle)?;

            let cfg = config::load(&handle);

            let hotkey_error = register_hotkey(&handle, &cfg.hotkey).err();
            if let Some(err) = &hotkey_error {
                eprintln!("{err}");
                let _ = handle.emit(EVT_HOTKEY_ERROR, err.clone());
            }

            // Which window(s) to open at launch, by the user's familiarity — three cases:
            //   Not configured yet (or the hotkey didn't register) → settings page, with the reason explained
            //   Configured but never successfully snipped  → hub, a visible entry point
            //   Already used                    → no window, just flash the bottom-right startup toast
            //
            // That last case used to be completely silent; users complained "after
            // launch nothing happens, can't tell if it started". So it now always
            // flashes a small toast (self-dismissing after a few seconds) to confirm
            // it's standing by in the background.
            let needs_setup =
                hotkey_error.is_some() || (!cfg.uses_webchat() && !config::is_configured(&handle));
            if needs_setup {
                if let Err(err) = open_settings(&handle) {
                    eprintln!("Failed to open settings: {err}");
                }
            } else {
                if !cfg.has_used {
                    // The tool normally has no main window; a newcomer sees "nothing on
                    // the desktop" after launch and can hardly figure out the hotkey
                    // and tray icon alone.
                    if let Err(err) = open_hub(&handle) {
                        eprintln!("Failed to open the hub: {err}");
                    }
                }
                // If the settings page is already open, skip the toast — it would make
                // the user think the app started twice
                if let Err(err) = open_toast(&handle) {
                    eprintln!("Failed to open the startup toast: {err}");
                }
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // Closing all windows does not mean quitting the app.
            //
            // Tauri's default is "exit when the last window closes", but this is a
            // resident tray tool; after the user closes the windows the program should
            // stay in the tray — otherwise closing a window makes the program vanish
            // and the user has to double-click again.
            //
            // Real exit goes through the tray menu / hub's "Quit"; that path is
            // app.exit(0) with an exit code (Some), so it isn't blocked here.
            // Record geometry only in ExitRequested: the event loop is still alive and
            // the window APIs are usable. RunEvent::Exit is the last step — touching
            // windows then isn't safe, and all normal exits (including the tray and
            // "Quit" via app.exit(0)) pass through ExitRequested first.
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                } else {
                    save_all_window_layouts(&_app);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked_conversation() -> Conversation {
        let mut conv = Conversation::default();
        conv.start_turn("asked already", Vec::new());
        conv.turns[0].answer = "answered already".into();
        conv
    }

    #[test]
    fn continue_is_off_by_default() {
        let conv = asked_conversation();
        assert!(
            !should_continue(false, Some(&conv)),
            "with the toggle off, start a new conversation even if the current one has been asked in"
        );
    }

    #[test]
    fn continue_needs_an_existing_conversation() {
        assert!(!should_continue(true, None), "nothing snipped yet, nothing to continue");
    }

    #[test]
    fn continue_skips_a_conversation_without_any_question() {
        // Only a freshly snipped, not-yet-asked image: the new image should replace it,
        // not append (otherwise turns pile up "with images but no words")
        let conv = Conversation {
            image_b64: "IMGPENDING".into(),
            ..Default::default()
        };
        assert!(!should_continue(true, Some(&conv)));
    }

    #[test]
    fn continue_appends_once_the_conversation_has_turns() {
        assert!(should_continue(true, Some(&asked_conversation())));
    }

    #[test]
    fn payload_keeps_the_sent_images_on_the_film_strip() {
        // The film strip must not go empty after sending: the payload must include the
        // most recent turn's images, otherwise the panel is left with a single static
        // preview and it looks like "I sent two, how did they become one".
        let mut conv = Conversation::default();
        conv.start_turn("who is this", vec!["IMG1".into(), "IMG2".into()]);
        conv.turns[0].answer = "answer one".into();
        // Another snip, not yet sent
        conv.stage(history::StagedImage {
            image_b64: "IMG3".into(),
            selected: true,
            crop_w: 10,
            crop_h: 10,
            sent_w: 10,
            sent_h: 10,
        });

        let payload = ConversationPayload::from(&conv);
        assert_eq!(payload.staged.len(), 1, "the staged image must be reported");
        let sent: Vec<(&str, usize, usize)> = payload
            .sent
            .iter()
            .map(|item| (item.data_url.as_str(), item.turn_index, item.image_index))
            .collect();
        assert_eq!(
            sent,
            vec![
                ("data:image/jpeg;base64,IMG1", 0, 0),
                ("data:image/jpeg;base64,IMG2", 0, 1)
            ],
            "the two sent images must be laid back on the film strip with 'which turn, which image' (the ✕ delete needs it)"
        );

        // After a text-only follow-up, the previous batch of images must stay up
        // (otherwise it's "many become one" again)
        conv.take_selected_staged();
        conv.start_turn("follow-up question", Vec::new());
        conv.turns[1].answer = "answer two".into();
        let payload = ConversationPayload::from(&conv);
        assert_eq!(payload.sent.len(), 2, "the latest turn has no images, so look back for one that does");
        assert!(payload.staged.is_empty(), "once sent, nothing remains staged");
    }

    #[test]
    fn only_the_plus_button_and_the_toggle_append() {
        // Leftover unsent images on the film strip **no longer** auto-append either:
        // that rule clashed with "turning off continue context" — the user turns the
        // toggle off, snips a new image, and it gets attached back into the old
        // conversation (complained about in real use). Now only "+" (explicit append)
        // and the on toggle can append; when starting a new conversation the old
        // images move into the new one.
        let mut conv = asked_conversation();
        assert!(
            !should_append(false, false, Some(&conv)),
            "toggle off, + not clicked: start a new conversation"
        );
        conv.stage(history::StagedImage {
            image_b64: "IMG".into(),
            selected: true,
            crop_w: 1,
            crop_h: 1,
            sent_w: 1,
            sent_h: 1,
        });
        assert!(
            !should_append(false, false, Some(&conv)),
            "unsent images on the strip must not override the toggle"
        );
        assert!(should_append(true, false, Some(&conv)), "clicking + means append");
        assert!(should_append(true, false, None), "clicking + means append (even without a conversation yet)");
        assert!(should_append(false, true, Some(&conv)), "toggle on and asked already: append");
        assert!(
            !should_append(false, true, None),
            "with no current conversation, even the on toggle only starts a new one"
        );
    }

    #[test]
    fn toast_sits_in_the_bottom_right_corner() {
        // A 2560x1440 screen: 22 on the right, room for the taskbar at the bottom
        let (x, y) = toast_position(2560.0, 1440.0, (0.0, 0.0));
        assert_eq!(x, 2560.0 - TOAST_W - TOAST_MARGIN);
        assert_eq!(y, 1440.0 - TOAST_H - TOAST_BOTTOM);
        assert!(x + TOAST_W <= 2560.0 && y + TOAST_H <= 1440.0, "must not go off-screen");
    }

    #[test]
    fn toast_follows_the_primary_monitor_origin() {
        // When the primary screen doesn't start at (0,0) (multi-monitor with the
        // primary on the right), the toast follows the offset
        let (x, y) = toast_position(1920.0, 1080.0, (1920.0, 0.0));
        assert_eq!(x, 1920.0 + 1920.0 - TOAST_W - TOAST_MARGIN);
        assert_eq!(y, 1080.0 - TOAST_H - TOAST_BOTTOM);
    }

    #[test]
    fn toast_never_goes_negative_on_a_tiny_screen() {
        // When the screen is smaller than the toast, coordinates clamp to 0 instead of
        // producing a negative position (a negative-coordinate window goes off-screen
        // and the user sees nothing)
        let (x, y) = toast_position(200.0, 120.0, (0.0, 0.0));
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn continue_only_looks_at_turns_not_at_the_pending_image() {
        // After a question was asked, one more snip is pending; snipping yet another
        // should still continue (the old pending one gets replaced)
        let mut conv = asked_conversation();
        conv.image_b64 = "IMGPENDING".into();
        assert!(should_continue(true, Some(&conv)));
    }
}
