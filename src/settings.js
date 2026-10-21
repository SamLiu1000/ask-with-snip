/* Settings page: multiple API profiles, provider presets, connection test. */
(function () {
  const { call, listen, installErrorReporting, toast, t, setLanguage, applyTheme, applyI18n, installHotkeyRecorder } =
    window.AskWithSnip;
  installErrorReporting("settings");

  /**
   * Provider presets.
   * They only fill in the base url and model name to get you started — model names change fast,
   * so check the provider's current docs before use; a wrong name makes the API return 400/404.
   */
  // "Custom" goes first (advanced users don't have to open the dropdown and scroll to the bottom), DeepSeek second,
  // the rest sorted alphabetically by name. The filled URL and model name are reference only.
  // GLM is split in two: Zhipu's open platform BigModel and the international Z.ai; their keys are not interchangeable.
  const PRESETS = [
    { label: "自定义（自己填）", base_url: "", model: "" },
    { label: "DeepSeek", base_url: "https://api.deepseek.com/v1", model: "deepseek-chat" },
    { label: "Claude（Anthropic）", base_url: "https://api.anthropic.com/v1", model: "claude-sonnet-4-5" },
    { label: "GLM（BigModel 智谱开放平台）", base_url: "https://open.bigmodel.cn/api/paas/v4", model: "glm-4v-flash" },
    { label: "GLM（Z.ai 海外版）", base_url: "https://api.z.ai/api/paas/v4", model: "glm-4v-flash" },
    { label: "Grok（xAI）", base_url: "https://api.x.ai/v1", model: "grok-2-vision-1212" },
    { label: "Kimi（月之暗面）", base_url: "https://api.moonshot.cn/v1", model: "moonshot-v1-8k-vision-preview" },
    { label: "OpenAI", base_url: "https://api.openai.com/v1", model: "gpt-4o-mini" },
    { label: "Qwen（通义千问）", base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1", model: "qwen-vl-plus" },
  ];

  /** The API-related fields belong to "the current profile"; the rest are global settings */
  const PROFILE_FIELDS = ["base_url", "api_key", "model", "extra_headers", "proxy"];
  const GLOBAL_FIELDS = [
    "hotkey", "timeout_secs", "system_prompt", "autostart", "continue_conversation",
    "max_image_edge", "jpeg_quality", "webchat_proxy",
    // theme deliberately absent: it's toggled from the panel's top bar now, and
    // save mutates the loaded config object, so the loaded value survives.
  ];

  const el = (id) => document.getElementById(id);
  const testResult = el("test-result");
  const profileList = el("profile-list");
  const profileHint = el("profile-hint");

  /**
   * The full config read back from the backend.
   * Saving mutates **on top of** this object and writes it back, rather than rebuilding one from the
   * form — the form lacks fields like has_used, and rebuilding would wipe them to defaults.
   */
  let config = null;

  // ---------------------------------------------------------- Form

  function readProfileFields() {
    const profile = {};
    for (const key of PROFILE_FIELDS) profile[key] = el(key).value.trim();
    return profile;
  }

  function writeProfileFields(profile) {
    for (const key of PROFILE_FIELDS) el(key).value = (profile && profile[key]) || "";
  }

  function readGlobalFields() {
    return {
      hotkey: el("hotkey").value.trim(),
      timeout_secs: Number(el("timeout_secs").value) || 120,
      system_prompt: el("system_prompt").value,
      autostart: el("autostart").checked,
      continue_conversation: el("continue_conversation").checked,
      max_image_edge: Number(el("max_image_edge").value) || 1568,
      jpeg_quality: Number(el("jpeg_quality").value) || 85,
      webchat_proxy: el("webchat_proxy").value.trim(),
    };
  }

  function writeGlobalFields(cfg) {
    for (const key of GLOBAL_FIELDS) {
      const node = el(key);
      if (!node) continue;
      const value = cfg[key];
      if (node.type === "checkbox") node.checked = Boolean(value);
      else node.value = value == null ? "" : value;
    }
  }

  /** Collect the form's in-edit content into config, so switching profiles / saving never loses changes */
  function syncFormIntoConfig() {
    Object.assign(config, readGlobalFields());
    const index = activeIndex();
    if (index >= 0) Object.assign(config.profiles[index], readProfileFields());
  }

  // ---------------------------------------------------------- Profile list

  function activeIndex() {
    if (!config || !config.profiles.length) return -1;
    const found = config.profiles.findIndex((p) => p.name === config.active_profile);
    return found >= 0 ? found : 0;
  }

  function activeProfile() {
    const index = activeIndex();
    return index >= 0 ? config.profiles[index] : null;
  }

  function renderProfiles() {
    profileList.textContent = "";
    const current = activeIndex();

    config.profiles.forEach((profile, index) => {
      const chip = document.createElement("div");
      chip.className = "profile-chip" + (index === current ? " active" : "");
      chip.dataset.index = String(index);
      chip.title = profile.base_url || t("（没填地址）");

      const name = document.createElement("span");
      name.className = "p-name";
      name.textContent = profile.name;

      const model = document.createElement("span");
      model.className = "p-model";
      model.textContent = profile.model || t("缺模型名");

      chip.append(name, model);

      // Don't allow deleting the last profile — with none left nothing works, it would only confuse
      if (config.profiles.length > 1) {
        const del = document.createElement("button");
        del.className = "p-del";
        del.textContent = "✕";
        del.title = t("删除「{n}」").replace("{n}", profile.name);
        del.addEventListener("click", (event) => {
          event.stopPropagation();
          removeProfile(index);
        });
        chip.appendChild(del);
      }

      chip.addEventListener("click", () => activateProfile(index));
      profileList.appendChild(chip);
    });

    const profile = activeProfile();
    const usable = profile && profile.api_key && profile.model;
    profileHint.textContent = profile
      ? usable
        ? t("当前用「{n}」提问").replace("{n}", profile.name)
        : t("「{n}」还缺 API Key 或模型名").replace("{n}", profile.name)
      : t("还没有配置");
    profileHint.style.color = usable ? "" : "var(--danger)";
  }

  /** Clicking a chip switches to that profile and persists immediately — otherwise closing the settings page would lose the click */
  async function activateProfile(index) {
    if (index === activeIndex()) return;
    syncFormIntoConfig();
    config.active_profile = config.profiles[index].name;
    writeProfileFields(config.profiles[index]);
    renderProfiles();
    await persist(t("已切换到「{n}」").replace("{n}", config.active_profile));
  }

  async function removeProfile(index) {
    syncFormIntoConfig();
    const [removed] = config.profiles.splice(index, 1);
    if (config.active_profile === removed.name) {
      config.active_profile = config.profiles[0] ? config.profiles[0].name : "";
    }
    writeProfileFields(activeProfile());
    renderProfiles();
    await persist(t("已删除「{n}」").replace("{n}", removed.name));
  }

  async function addProfile() {
    syncFormIntoConfig();
    // Name it "ProfileN"; find the first unused N
    let n = config.profiles.length + 1;
    const used = new Set(config.profiles.map((p) => p.name));
    while (used.has(t("配置{n}").replace("{n}", n))) n += 1;

    // The new profile inherits the current form's content; the user only edits the URL and Key
    const draft = Object.assign({ name: t("配置{n}").replace("{n}", n) }, readProfileFields());
    config.profiles.push(draft);
    config.active_profile = draft.name;
    renderProfiles();
    await persist(t("已存为「{n}」，改完地址和 Key 记得再点保存").replace("{n}", draft.name));
  }

  /** Rename the current profile. Uses an inline input, not prompt() — the webview has no native prompt. */
  function renameActive() {
    const chip = profileList.querySelector(".profile-chip.active");
    const profile = activeProfile();
    if (!chip || !profile) return;

    const input = document.createElement("input");
    input.type = "text";
    input.value = profile.name;
    input.style.cssText = "width:110px;font-size:12.5px;padding:1px 5px";
    chip.replaceChildren(input);
    input.focus();
    input.select();

    let done = false;
    const commit = async (save) => {
      if (done) return;
      done = true;
      const name = input.value.trim();
      if (save && name && name !== profile.name) {
        // If the new name collides with another profile, say so instead of silently renaming
        if (config.profiles.some((p) => p !== profile && p.name === name)) {
          renderProfiles();
          toast(t("已经有叫「{n}」的配置了").replace("{n}", name), "err");
          return;
        }
        const wasActive = config.active_profile === profile.name;
        profile.name = name;
        if (wasActive) config.active_profile = name;
        renderProfiles();
        await persist(t("已重命名为「{n}」").replace("{n}", name));
        return;
      }
      renderProfiles();
    };

    input.addEventListener("keydown", (event) => {
      if (event.isComposing || event.keyCode === 229) return;
      if (event.key === "Enter") {
        event.preventDefault();
        commit(true);
      } else if (event.key === "Escape") {
        event.preventDefault();
        commit(false);
      }
    });
    input.addEventListener("blur", () => commit(true));
  }

  // ---------------------------------------------------------- Save

  async function persist(message) {
    try {
      await call("save_config", { cfg: config });
      // The backend normalizes (dedupes names, fixes active, clamps ranges); treat its return as the truth
      config = await call("get_config");
      writeGlobalFields(config);
      writeProfileFields(activeProfile());
      renderProfiles();
      // Re-skin this page right away; every other window follows via theme-changed
      applyTheme(config.theme);
      if (message) toast(message, "ok");
      return true;
    } catch (err) {
      toast(String(err), "err");
      return false;
    }
  }

  // ---------------------------------------------------------- Presets

  /** Rebuildable: called again on language change so the option labels re-translate. */
  function buildPresets() {
    const select = el("preset");
    select.textContent = "";
    PRESETS.forEach((preset, index) => {
      const option = document.createElement("option");
      option.value = String(index);
      option.textContent = t(preset.label);
      select.appendChild(option);
    });
  }

  // ---------------------------------------------------------- Startup

  async function boot() {
    buildPresets();

    // Applying a preset only fills URL & model; bound once here, buildPresets may run again on language change
    el("preset").addEventListener("change", () => {
      const preset = PRESETS[Number(el("preset").value)];
      if (!preset || (!preset.base_url && !preset.model)) return;
      el("base_url").value = preset.base_url;
      el("model").value = preset.model;
      testResult.textContent = "";
    });

    // UI language follows the config (it changes here when toggled on the panel)
    try {
      const cfg0 = await call("get_config");
      setLanguage(cfg0.language);
      applyTheme(cfg0.theme);
    } catch (_) { /* If unreadable, keep Chinese */ }
    applyI18n();
    buildPresets(); // rebuild with the final language (options are built before the config is read)

    // Hotkey recorder: click the input and press keys directly; records combos like Ctrl+Shift+A
    installHotkeyRecorder(el("hotkey"));

    // The hotkey may fail to register at startup (taken by other software); the Rust side broadcasts that.
    // Without showing it, the user only sees "the app runs but the hotkey does nothing".
    await listen("hotkey-error", (event) => {
      const message = String(event.payload ?? t("未知错误"));
      toast(t("热键没注册上：") + message, "err");
      const hint = el("hotkey-hint");
      if (hint) {
        hint.textContent = t("当前热键不可用：") + message;
        hint.style.color = "var(--danger)";
      }
    });

    try {
      config = await call("get_config");
    } catch (err) {
      toast(err, "err");
      return;
    }

    writeGlobalFields(config);
    writeProfileFields(activeProfile());
    renderProfiles();

    // Show the user where the Key is stored, for backup or cleanup. The path comes from the backend,
    // so we never hard-code a directory name that could drift from reality.
    try {
      el("config-path").textContent = await call("get_config_path");
    } catch (_) {
      /* If unavailable, show nothing; nothing else is affected */
    }
  }

  // ---------------------------------------------------------- Event bindings

  el("btn-save").addEventListener("click", async () => {
    syncFormIntoConfig();
    const profile = activeProfile();
    if (!profile || !profile.api_key) {
      toast(t("还没填 API Key"), "err");
      return;
    }
    await persist(t("已保存"));
  });

  el("btn-add-profile").addEventListener("click", addProfile);
  el("btn-rename-profile").addEventListener("click", renameActive);

  /** Fill the model-name input's dropdown with IDs fetched from the provider. */
  function fillModelList(models) {
    const list = el("model-list");
    list.textContent = "";
    for (const id of models) {
      const option = document.createElement("option");
      option.value = id;
      list.appendChild(option);
    }
  }

  el("btn-test").addEventListener("click", async () => {
    syncFormIntoConfig();
    const profile = activeProfile();
    if (!profile || !profile.api_key) {
      toast(t("还没填 API Key"), "err");
      return;
    }
    const button = el("btn-test");
    button.disabled = true;
    testResult.textContent = t("正在请求…");
    try {
      const reply = await call("test_api", { cfg: config });
      let note = "";
      // Connection works: also fetch /models so the model-name input gets a dropdown
      try {
        const models = await call("list_models_api", { cfg: config });
        if (models.length) {
          fillModelList(models);
          note = t("，已读到 {n} 个模型").replace("{n}", String(models.length));
        }
      } catch (_) { /* no /models endpoint or blocked; the input stays free-form */ }
      testResult.textContent = t("连接正常，模型回复：") + String(reply).slice(0, 60) + note;
      toast(t("连接正常"), "ok");
    } catch (err) {
      testResult.textContent = String(err).slice(0, 200);
      toast(t("连接失败，详情见下方"), "err");
    } finally {
      button.disabled = false;
    }
  });

  el("btn-close").addEventListener("click", () => {
    call("close_self").catch(() => {});
  });

  el("btn-try").addEventListener("click", () => {
    call("trigger_snip").catch((err) => toast(String(err), "err"));
  });

  boot();
})();
