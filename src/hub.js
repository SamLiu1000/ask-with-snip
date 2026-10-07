/* Hub window: gives users who "can't find the entry point" something visible to click. */
(function () {
  const { call, listen, installErrorReporting, toast, setLanguage, applyTheme, t } = window.AskWithSnip;
  installErrorReporting("hub");

  const hotkeyLine = document.getElementById("hotkey-line");

  listen("theme-changed", (e) => applyTheme(e.payload)).catch(() => {});

  async function boot() {
    try {
      const cfg = await call("get_config");
      setLanguage(cfg && cfg.language);
      applyTheme(cfg && cfg.theme);
      window.AskWithSnip.applyI18n();
      // The hotkey is configurable; show what the user actually set, don't hardcode
      hotkeyLine.textContent = t("也可以随时按 {hk}").replace("{hk}", (cfg && cfg.hotkey) || "Ctrl+Shift+A");
    } catch (_) {
      /* If config can't be read, keep the default copy; the buttons are unaffected */
    }
  }

  document.getElementById("btn-snip").addEventListener("click", () => {
    call("trigger_snip").catch((err) => toast(String(err), "err"));
  });

  document.getElementById("btn-history").addEventListener("click", () => {
    call("open_panel").catch((err) => toast(String(err), "err"));
  });

  document.getElementById("btn-settings").addEventListener("click", () => {
    call("open_settings_window").catch((err) => toast(String(err), "err"));
  });

  document.getElementById("btn-quit").addEventListener("click", () => {
    call("quit_app").catch((err) => toast(String(err), "err"));
  });

  boot();
})();
