/*
 * Startup toast: pinned to the bottom-right corner, dismisses itself after a few seconds.
 *
 * It exists for one reason only: tell the user "the app is up and waiting in the
 * background". So no modal, no focus stealing, no confirmation — the real entry
 * points live in the tray menu and the hub window.
 *
 * Note: this window is focusable(false) (no focus stealing), and in practice it
 * **receives no hover or keyboard events**: a click does land (dismissing relies on
 * that), but mouseenter and Esc never arrive. So a hover-paused countdown and an
 * Esc handler are deliberately omitted — they would be dead code.
 */
(function () {
  const { call, listen, installErrorReporting, setLanguage, applyTheme, applyI18n } = window.AskWithSnip;
  installErrorReporting("toast");

  const card = document.getElementById("card");
  const hotkeyEl = document.getElementById("hotkey");

  /** How long to stay visible (ms). Long enough to read two lines, short enough not to block the corner. */
  const DWELL = 6000;

  let timer = null;
  let closed = false;

  function dismiss() {
    if (closed) return;
    closed = true;
    clearTimeout(timer);
    // Hide instead of destroy: the window is created once, and destroying it from its own IPC leaves a zombie label
    call("hide_toast").catch(() => {});
  }

  listen("theme-changed", (e) => applyTheme(e.payload)).catch(() => {});

  async function boot() {
    try {
      const cfg = await call("get_config");
      // The hotkey is configurable; show what the user actually set, don't hardcode
      if (cfg && cfg.hotkey) hotkeyEl.textContent = cfg.hotkey;
      setLanguage(cfg && cfg.language);
      applyTheme(cfg && cfg.theme);
      applyI18n();
    } catch (_) {
      /* If config can't be read, fall back to the default copy in the HTML */
    }

    // Lay out the copy before showing, to avoid a flash of an empty box
    await call("toast_ready").catch(() => {});
    timer = setTimeout(dismiss, DWELL);
  }

  // Click to dismiss early (click events do arrive here; see the file header)
  card.addEventListener("click", dismiss);

  boot();
})();
