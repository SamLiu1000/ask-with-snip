use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// Use the OpenAI-compatible API (default)
pub const BACKEND_API: &str = "api";
/// Use the embedded web chat
pub const BACKEND_WEBCHAT: &str = "webchat";

/// One API configuration. Users can store multiple ("Profile 1", "Profile 2"...)
/// and switch between them with one click on the settings page.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    /// Base URL of the OpenAI-compatible API, down to the /v1 level
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Extra request headers, a JSON object string, e.g. {"X-Foo":"bar"}
    pub extra_headers: String,
    /// Proxy address for this profile (optional). Formats: "127.0.0.1:7890",
    /// "http://…", "socks5://…". Empty = direct connection. Each profile carries
    /// its own: ones that need a proxy via a relay and ones that connect directly
    /// can be stored side by side without interfering.
    #[serde(default)]
    pub proxy: String,
}

impl Profile {
    pub fn endpoint(&self) -> String {
        endpoint_for(&self.base_url)
    }

    pub fn extra_headers_map(&self) -> Vec<(String, String)> {
        parse_extra_headers(&self.extra_headers)
    }

    /// Whether this profile can be used to send requests. Missing key or model
    /// name means it cannot.
    pub fn is_usable(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// Build the full chat/completions URL.
///
/// Users enter all kinds of things; this does three things: strip trailing
/// slashes; don't append /chat/completions again if it is already there; and when
/// only a domain was entered (no /v1-style path), add /v1 — the common convention
/// for OpenAI-compatible APIs.
fn endpoint_for(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return String::new();
    }
    if base.ends_with("/chat/completions") {
        return base.to_string();
    }
    if has_no_path(base) {
        return format!("{base}/v1/chat/completions");
    }
    format!("{base}/chat/completions")
}

/// Whether the url has no path after the host. Used to decide whether to add /v1.
fn has_no_path(url: &str) -> bool {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    !after_scheme.contains('/')
}

fn parse_extra_headers(raw: &str) -> Vec<(String, String)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw) {
        Ok(map) => map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A web chat account: which site + what it is called. **Login state is isolated
/// per account** — each account gets its own WebView2 data directory, so you can
/// be logged into DeepSeek, ChatGPT, etc. at the same time without interference.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct WebchatAccount {
    pub name: String,
    /// Site id, matching the site registry in webchat.rs (deepseek / chatgpt /
    /// gemini / claude)
    pub site: String,
    /// [Legacy field, kept for compatibility] Whether this account's web window
    /// uses a proxy. The new logic is a per account×site selection (proxy_sites);
    /// normalize merges it in.
    #[serde(default)]
    pub proxy_enabled: bool,
    /// Which sites this account uses a proxy on (list of site ids). The same
    /// account can connect directly on deepseek and automatically go through a
    /// proxy on chatgpt, without manually toggling checkboxes.
    #[serde(default)]
    pub proxy_sites: Vec<String>,
}

impl WebchatAccount {
    /// Does this account use a proxy on the given site?
    pub fn proxy_for(&self, site_id: &str) -> bool {
        self.proxy_sites.iter().any(|s| s == site_id)
    }
}

/// UI configuration. **Does not include window geometry** — that part is stored in
/// a separate file, see WindowLayouts below.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AppConfig {
    /// Saved API profiles. The active one is the profile pointed to by
    /// active_profile.
    pub profiles: Vec<Profile>,
    /// Name of the currently active profile. Empty string means not configured yet.
    pub active_profile: String,

    /// Which route questions take:
    ///   `"api"`     the OpenAI-compatible API configured above, consumes API quota
    ///               (default)
    ///   `"webchat"` the embedded web chat, using your own web accounts, consumes
    ///               no API quota
    ///
    /// One click on the panel switches it and persists immediately. Both routes
    /// present the same panel and the same operations to the user.
    pub backend: String,

    /// Global hotkey, e.g. Ctrl+Shift+A
    pub hotkey: String,
    /// Whether a capture starts a new conversation or continues the current one.
    ///
    /// Defaults to a new conversation: each screenshot has independent context, no
    /// interference, and costs don't grow from piling up too many images. But for
    /// long documents — "capture a section, ask a question, capture the next
    /// section, keep asking" — context needs to accumulate, so this switch exists
    /// (changeable on the settings page and the panel).
    pub continue_conversation: bool,
    pub system_prompt: String,
    /// Before sending to the model, downscale the long edge to at most this value
    /// (vision models compress internally anyway; oversized only wastes tokens)
    pub max_image_edge: u32,
    pub jpeg_quality: u8,
    pub timeout_secs: u64,
    /// Whether to start with the system (writes HKCU\...\Run on Windows)
    pub autostart: bool,
    /// Whether a capture has been completed successfully at least once. Used to
    /// decide whether to bring the console window to the front on startup.
    pub has_used: bool,
    /// Proxy address for the web chat (optional). Follows Chromium's
    /// --proxy-server format: "127.0.0.1:7890", "http://…", "socks5://…" are all
    /// accepted. Empty = direct connection. Without a proxy, ChatGPT / Gemini /
    /// Claude are unreachable from some regions.
    pub webchat_proxy: String,
    /// Web chat account list (unique by name). Login state is isolated per
    /// account, see WebchatAccount.
    pub accounts: Vec<WebchatAccount>,
    /// Which account is active (by name). Empty = use the first one.
    pub active_account: String,
    /// UI language: "zh" (default) or "en". Clicking the language toggle on the
    /// panel switches it and persists immediately.
    #[serde(default)]
    pub language: String,
    /// UI color theme: "light" (default) or "dark". Pages apply it by setting
    /// <html data-theme> right after reading the config.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Quick-ask buttons (the row above the panel input). Each entry = display
    /// name + full prompt; right-click a button to edit, click "＋" to add.
    /// Empty prompt = use the name as the prompt when sending.
/// Older versions stored a plain list of strings; untagged reads them in for
/// compatibility.
    #[serde(default = "default_presets")]
    pub presets: Vec<PresetEntry>,
    /// Named groups of quick prompts (see PresetList). Kept in sync with
    /// `presets`, which mirrors the active list; when `preset_lists` is missing
    /// (config from an older build), normalize seeds one from `presets`.
    #[serde(default)]
    pub preset_lists: Vec<PresetList>,
    /// Which quick-prompt list is active (by name). Empty = use the first one.
    #[serde(default)]
    pub active_preset_list: String,
}

/// One quick prompt. Two forms are accepted: `"Translate to English"` (legacy
/// plain string) and `{ "name": "Translate", "prompt": "Translate the content in
/// the screenshot to English" }`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum PresetEntry {
    Full { name: String, #[serde(default)] prompt: String },
    Simple(String),
}

/// A named group of quick prompts. The panel shows a picker above the quick
/// prompts: click it to switch lists (each list = its own set of quick-ask
/// buttons), create new ones, rename or delete them. The last choice is
/// persisted, so the panel reopens on the list the user left active.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PresetList {
    pub name: String,
    #[serde(default)]
    pub presets: Vec<PresetEntry>,
}

fn default_theme() -> String {
    "light".into()
}

fn default_presets() -> Vec<PresetEntry> {
    vec![
        // These two are English all through: the label on the button and the prompt sent out
        PresetEntry::Full {
            name: "Translate into English".into(),
            prompt: "Translate everything in the screenshot into English, preserving the original layout."
                .into(),
        },
        PresetEntry::Full {
            name: "翻译成中文".into(),
            prompt: "把截图里的所有内容翻译成中文，保持原有排版".into(),
        },
        // Empty prompt: the name itself is sent as the question
        PresetEntry::Full {
            name: "解释这段内容".into(),
            prompt: String::new(),
        },
        PresetEntry::Full {
            name: "Explain this content".into(),
            prompt: "Explain the content in this screenshot. Answer in English.".into(),
        },
    ]
}

/// Shared preset cleaning, used for both the legacy flat field and each list's
/// entries: trim, convert legacy plain strings to {name, prompt}, drop entries
/// where both fields are empty.
fn normalize_presets(entries: Vec<PresetEntry>) -> Vec<PresetEntry> {
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            PresetEntry::Full { name, prompt } => {
                let name = name.trim().to_string();
                let prompt = prompt.trim().to_string();
                if name.is_empty() && prompt.is_empty() {
                    None
                } else {
                    Some(PresetEntry::Full { name, prompt })
                }
            }
            PresetEntry::Simple(text) => {
                let text = text.trim().to_string();
                if text.is_empty() { None } else { Some(PresetEntry::Full { name: text, prompt: String::new() }) }
            }
        })
        .collect()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            profiles: vec![Profile {
                name: "Profile 1".into(),
                base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
                api_key: String::new(),
                proxy: String::new(),
                model: "glm-4v-flash".into(),
                extra_headers: String::new(),
            }],
            active_profile: "Profile 1".into(),
            backend: BACKEND_API.into(),
            hotkey: "Ctrl+Shift+A".into(),
            continue_conversation: false,
            // Empty by default: the request then carries no system message at all
            // (see messages_from_turns), and the model just answers the question.
            system_prompt: String::new(),
            max_image_edge: 1568,
            jpeg_quality: 85,
            timeout_secs: 120,
            autostart: false,
            has_used: false,
            webchat_proxy: String::new(),
            accounts: vec![WebchatAccount { name: "Account 1".into(), site: "deepseek".into(), proxy_enabled: false, proxy_sites: Vec::new() }],
            active_account: "Account 1".into(),
            language: "zh".into(),
            theme: default_theme(),
            presets: default_presets(),
            preset_lists: vec![PresetList {
                name: "默认".into(),
                presets: default_presets(),
            }],
            active_preset_list: "默认".into(),
        }
    }
}

impl AppConfig {
    fn active_index(&self) -> Option<usize> {
        self.profiles
            .iter()
            .position(|p| p.name == self.active_profile)
            .or(if self.profiles.is_empty() { None } else { Some(0) })
    }

    /// The currently active profile. Falls back to the first one if the name does
    /// not match, avoiding "profile deleted, app completely unusable".
    pub fn active(&self) -> Option<&Profile> {
        self.active_index().map(|i| &self.profiles[i])
    }

    /// Whether the current profile is fully configured and can send requests.
    pub fn is_configured(&self) -> bool {
        self.active().map(Profile::is_usable).unwrap_or(false)
    }

    /// Does this question go through the web chat? Unrecognized values always
    /// count as api — even a hand-broken config file must stay usable.
    pub fn uses_webchat(&self) -> bool {
        self.backend.trim() == BACKEND_WEBCHAT
    }

    /// The active web chat account. Falls back to the first one if the name does
    /// not match; falls back to the built-in default when the list is empty
    /// (old config files have no accounts key and would read back empty, but must
    /// still work).
    pub fn active_webchat(&self) -> WebchatAccount {
        let accounts = if self.accounts.is_empty() {
            vec![WebchatAccount { name: "Account 1".into(), site: "deepseek".into(), proxy_enabled: false, proxy_sites: Vec::new() }]
        } else {
            self.accounts.clone()
        };
        accounts
            .iter()
            .find(|a| a.name == self.active_account)
            .or_else(|| accounts.first())
            .cloned()
            .unwrap_or_default()
    }

    /// Self-checks and repairs before persisting: drop unnamed entries, dedupe
    /// duplicate names, make sure active_profile points to an existing profile.
    fn normalize(mut self) -> Self {
        let mut seen: Vec<String> = Vec::new();
        self.profiles.retain_mut(|profile| {
            let name = profile.name.trim().to_string();
            if name.is_empty() {
                return false;
            }
            let mut unique = name.clone();
            let mut suffix = 2;
            while seen.contains(&unique) {
                unique = format!("{name}{suffix}");
                suffix += 1;
            }
            seen.push(unique.clone());
            profile.name = unique;
            true
        });

        if self.profiles.is_empty() {
            self.active_profile.clear();
        } else if !self.profiles.iter().any(|p| p.name == self.active_profile) {
            self.active_profile = self.profiles[0].name.clone();
        }

        // Only these two backend values are accepted; anything else (including
        // empty or hand-broken values) counts as api: when unrecognized, take the
        // API route so the user is never stuck with "nothing works".
        self.backend = if self.backend.trim() == BACKEND_WEBCHAT {
            BACKEND_WEBCHAT.to_string()
        } else {
            BACKEND_API.to_string()
        };

        // Web chat accounts: drop empty names, dedupe duplicates, make sure
        // active_account points to an existing account. Seed a default account
        // when the list is empty (old config files lack this key).
        if self.accounts.is_empty() {
            self.accounts = vec![WebchatAccount { name: "Account 1".into(), site: "deepseek".into(), proxy_enabled: false, proxy_sites: Vec::new() }];
        }
        let mut seen: Vec<String> = Vec::new();
        self.accounts.retain_mut(|account| {
            let name = account.name.trim().to_string();
            if name.is_empty() {
                return false;
            }
            let mut unique = name.clone();
            let mut suffix = 2;
            while seen.contains(&unique) {
                unique = format!("{name} {suffix}");
                suffix += 1;
            }
            seen.push(unique.clone());
            account.name = unique;
            // Unrecognized site falls back to deepseek (a hand-broken config file
            // must stay usable)
            if !matches!(account.site.as_str(), "deepseek" | "chatgpt" | "gemini" | "claude") {
                account.site = "deepseek".into();
            }
            // Legacy field migration: the old whole-account proxy checkbox means
            // at least the site it had at the time goes through a proxy. Migrate
            // only once (while proxy_sites is empty) and clear the old field
            // immediately afterwards — otherwise every save "resurrects"
            // checkboxes the user already unchecked (in practice: unchecking
            // DeepSeek and switching back and forth re-selected it for this
            // reason).
            if account.proxy_enabled && account.proxy_sites.is_empty() {
                account.proxy_sites.push(account.site.clone());
            }
            account.proxy_enabled = false;
            true
        });
        if !self.accounts.iter().any(|a| a.name == self.active_account) {
            self.active_account = self.accounts[0].name.clone();
        }

        // Only trim whitespace on the proxy: format validation is Chromium's job;
        // a bad value at worst means "cannot connect".
        self.webchat_proxy = self.webchat_proxy.trim().to_string();

        // Language only accepts zh/en; broken values fall back to Chinese.
        if self.language != "en" {
            self.language = "zh".to_string();
        }

        // Quick prompts: normalize to {name, prompt} form and trim; drop an entry
        // only when both name and prompt are empty. An empty prompt is valid
        // (= use the name as the prompt); no filling here, resolved at send time.
        self.presets = normalize_presets(std::mem::take(&mut self.presets));

        // Quick-prompt lists: seed one from the legacy flat `presets` field when
        // missing (config written by an older build), then normalize each list
        // (names trimmed & deduped, prompts cleaned). The active list is kept in
        // sync in both directions: this field always mirrors it.
        if self.preset_lists.is_empty() {
            self.preset_lists = vec![PresetList {
                name: "默认".into(),
                presets: self.presets.clone(),
            }];
        }
        let mut seen_lists: Vec<String> = Vec::new();
        self.preset_lists.retain_mut(|list| {
            let name = list.name.trim().to_string();
            if name.is_empty() {
                return false;
            }
            let mut unique = name.clone();
            let mut suffix = 2;
            while seen_lists.contains(&unique) {
                unique = format!("{name} {suffix}");
                suffix += 1;
            }
            seen_lists.push(unique.clone());
            list.name = unique;
            list.presets = normalize_presets(std::mem::take(&mut list.presets));
            true
        });
        if !self.preset_lists.iter().any(|l| l.name == self.active_preset_list) {
            self.active_preset_list = self
                .preset_lists
                .first()
                .map(|l| l.name.clone())
                .unwrap_or_default();
        }
        if let Some(list) = self.preset_lists.iter().find(|l| l.name == self.active_preset_list) {
            self.presets = list.presets.clone();
        }

        // Clamp numeric ranges so a hand-edited config file cannot produce absurd
        // values
        self.max_image_edge = self.max_image_edge.clamp(256, 8192);
        self.jpeg_quality = self.jpeg_quality.clamp(40, 100);
        self.timeout_secs = self.timeout_secs.clamp(10, 600);
        self
    }
}

// ---------------------------------------------------------------- window geometry

/// The last known size and position of a window (logical coordinates).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(default)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WindowRect {
    /// A too-small size means junk data (or the window was minimized); do not use
    /// it to restore.
    pub fn is_valid(&self) -> bool {
        self.width >= 200 && self.height >= 150
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct WindowLayouts {
    pub panel: Option<WindowRect>,
    pub settings: Option<WindowRect>,
    pub hub: Option<WindowRect>,
    /// The embedded web chat window (the route used when API quota runs out).
    pub webchat: Option<WindowRect>,
}

impl WindowLayouts {
    pub fn get(&self, label: &str) -> Option<WindowRect> {
        match label {
            "panel" => self.panel,
            "settings" => self.settings,
            "hub" => self.hub,
            "webchat" => self.webchat,
            _ => None,
        }
    }

    pub fn set(&mut self, label: &str, rect: WindowRect) {
        match label {
            "panel" => self.panel = Some(rect),
            "settings" => self.settings = Some(rect),
            "hub" => self.hub = Some(rect),
            "webchat" => self.webchat = Some(rect),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- load / save

fn app_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = crate::data_root(app);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create config directory: {e}"))?;
    Ok(dir)
}

pub fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_dir(app)?.join("config.json"))
}

/// Where the UI config lives, for display on the settings page.
pub fn config_path_display(app: &AppHandle) -> String {
    config_path(app)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".into())
}

fn layouts_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_dir(app)?.join("windows.json"))
}

/// Load the config. Falls back to defaults when the file is missing or fails to
/// parse, without blocking startup.
pub fn load(app: &AppHandle) -> AppConfig {
    config_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<AppConfig>(&s).ok())
        .unwrap_or_default()
        .normalize()
}

pub fn save(app: &AppHandle, cfg: &AppConfig) -> Result<(), String> {
    let path = config_path(app)?;
    let cfg = cfg.clone().normalize();
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("failed to write config: {e}"))
}

/// Window geometry is stored in a separate file.
///
/// Splitting it from config.json is intentional: window sizes are written by the
/// program itself at any moment, while config.json is overwritten wholesale from
/// the settings page. If they lived together, dragging a window and then clicking
/// "Save settings" would overwrite the freshly dragged sizes with the stale data
/// from the settings page.
pub fn load_layouts(app: &AppHandle) -> WindowLayouts {
    layouts_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_layout(app: &AppHandle, label: &str, rect: WindowRect) {
    if !rect.is_valid() {
        return;
    }
    let path = match layouts_path(app) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("cannot locate window layout file: {err}");
            return;
        }
    };
    let mut layouts = load_layouts(app);
    layouts.set(label, rect);
    match serde_json::to_string_pretty(&layouts) {
        Ok(text) => {
            if let Err(err) = std::fs::write(&path, text) {
                eprintln!("failed to write window layout: {err}");
            }
        }
        Err(err) => eprintln!("failed to serialize window layout: {err}"),
    }
}

/// Whether the UI config is fully configured. Used to decide whether to open the
/// settings page on first launch.
pub fn is_configured(app: &AppHandle) -> bool {
    load(app).is_configured()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with_base(base: &str) -> Profile {
        Profile {
            base_url: base.into(),
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_handles_common_base_url_shapes() {
        assert_eq!(
            profile_with_base("https://api.openai.com/v1").endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            profile_with_base("https://api.openai.com/v1/").endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        // Domain only: /v1 gets added
        assert_eq!(
            profile_with_base("https://api.openai.com").endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        // Already complete: not appended twice
        assert_eq!(
            profile_with_base("https://api.openai.com/v1/chat/completions").endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        // Common shape of domestic APIs: v4 instead of v1
        assert_eq!(
            profile_with_base("https://open.bigmodel.cn/api/paas/v4").endpoint(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
        // Empty address should not blow up or produce a weird url
        assert_eq!(profile_with_base("   ").endpoint(), "");
    }

    #[test]
    fn extra_headers_ignores_garbage() {
        let mut profile = Profile::default();
        profile.extra_headers = "not json".into();
        assert!(profile.extra_headers_map().is_empty());

        profile.extra_headers = r#"{"X-A":"1","X-B":2}"#.into();
        let map = profile.extra_headers_map();
        assert_eq!(map.len(), 1, "non-string values should be ignored");
        assert_eq!(map[0], ("X-A".to_string(), "1".to_string()));
    }

    #[test]
    fn active_falls_back_to_first_profile() {
        let mut cfg = AppConfig::default();
        cfg.active_profile = "nonexistent".into();
        assert_eq!(cfg.active().map(|p| p.name.as_str()), Some("Profile 1"));

        cfg.profiles.clear();
        assert!(cfg.active().is_none());
        assert!(!cfg.is_configured());
    }

    #[test]
    fn normalize_dedupes_names_and_repairs_active() {
        let cfg = AppConfig {
            profiles: vec![
                Profile { name: "Profile 1".into(), ..Default::default() },
                Profile { name: "Profile 1".into(), ..Default::default() },
                Profile { name: "  ".into(), ..Default::default() },
            ],
            active_profile: "already deleted".into(),
            ..Default::default()
        };
        let cfg = cfg.normalize();

        // Blank names dropped, duplicates renamed
        assert_eq!(cfg.profiles.len(), 2);
        assert_eq!(cfg.profiles[0].name, "Profile 1");
        assert_eq!(cfg.profiles[1].name, "Profile 12");
        // active points to an existing profile
        assert_eq!(cfg.active_profile, "Profile 1");
    }

    #[test]
    fn normalize_clamps_numeric_ranges() {
        let cfg = AppConfig {
            max_image_edge: 99999,
            jpeg_quality: 1,
            timeout_secs: 1,
            ..Default::default()
        }
        .normalize();
        assert_eq!(cfg.max_image_edge, 8192);
        assert_eq!(cfg.jpeg_quality, 40);
        assert_eq!(cfg.timeout_secs, 10);
    }

    #[test]
    fn profile_usability_requires_key_and_model() {
        let mut profile = Profile::default();
        assert!(!profile.is_usable(), "no key by default, must not count as usable");

        profile.api_key = "k".into();
        profile.model = "m".into();
        assert!(profile.is_usable());

        profile.api_key = "   ".into();
        assert!(!profile.is_usable(), "a whitespace-only key does not count as filled");
    }

    #[test]
    fn window_rect_validity_filters_junk() {
        assert!(!WindowRect::default().is_valid());
        assert!(!WindowRect { x: 0, y: 0, width: 120, height: 900 }.is_valid());
        assert!(WindowRect { x: 10, y: 20, width: 800, height: 600 }.is_valid());
    }

    #[test]
    fn backend_defaults_to_api_and_only_accepts_known_values() {
        // Old config files lack this key; it must read back as api (do not dump
        // old users onto the web chat)
        let old = r#"{"active_profile":"Profile 1","profiles":[{"name":"Profile 1","api_key":"k","model":"m"}]}"#;
        let cfg = serde_json::from_str::<AppConfig>(old).unwrap().normalize();
        assert_eq!(cfg.backend, BACKEND_API);
        assert!(!cfg.uses_webchat());

        // Hand-broken values still count as api
        let junk = AppConfig {
            backend: "  webcaht  ".into(),
            ..Default::default()
        }
        .normalize();
        assert_eq!(junk.backend, BACKEND_API);

        // Correct values are accepted; surrounding whitespace is not an error
        let ok = AppConfig {
            backend: " webchat ".into(),
            ..Default::default()
        }
        .normalize();
        assert_eq!(ok.backend, BACKEND_WEBCHAT);
        assert!(ok.uses_webchat());
    }

    #[test]
    fn webchat_backend_does_not_need_api_credentials() {
        // On the web chat route the API profile must not be required — that is
        // the whole point of the route
        let cfg = AppConfig {
            backend: BACKEND_WEBCHAT.into(),
            ..Default::default()
        }
        .normalize();
        assert!(!cfg.is_configured(), "the default profile has no key");
        assert!(cfg.uses_webchat(), "but the web chat route works");
    }

    #[test]
    fn webchat_accounts_seed_and_repair() {
        // Old config files have no accounts: seed a default account and keep
        // working
        let old = r#"{"backend":"webchat"}"#;
        let cfg = serde_json::from_str::<AppConfig>(old).unwrap().normalize();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].site, "deepseek");
        assert_eq!(cfg.active_webchat().site, "deepseek");

        // Duplicates renamed, bad sites fall back, active points to an existing
        // account
        let cfg = AppConfig {
            accounts: vec![
                WebchatAccount { name: "Work".into(), site: "chatgpt".into(), proxy_enabled: false, proxy_sites: Vec::new() },
                WebchatAccount { name: "Work".into(), site: "garbage site".into(), proxy_enabled: false, proxy_sites: Vec::new() },
            ],
            active_account: "nonexistent".into(),
            ..Default::default()
        }
        .normalize();
        assert_eq!(cfg.accounts.len(), 2);
        assert_eq!(cfg.accounts[1].name, "Work 2");
        assert_eq!(cfg.accounts[1].site, "deepseek", "unrecognized sites fall back to deepseek");
        assert_eq!(cfg.active_webchat().name, "Work");
    }

    #[test]
    fn layouts_without_webchat_still_load() {
        // Old windows.json files lack the webchat key; reading them must not
        // fail — otherwise all previously arranged window positions are lost
        let old = r#"{"panel":{"x":10,"y":20,"width":800,"height":600}}"#;
        let layouts: WindowLayouts = serde_json::from_str(old).unwrap();
        assert_eq!(layouts.panel.map(|r| r.width), Some(800));
        assert!(layouts.webchat.is_none());
    }

    #[test]
    fn layouts_roundtrip_by_label() {
        let mut layouts = WindowLayouts::default();
        assert!(layouts.get("panel").is_none());

        let rect = WindowRect { x: 5, y: 6, width: 700, height: 500 };
        layouts.set("panel", rect);
        assert_eq!(layouts.get("panel"), Some(rect));

        let webchat = WindowRect { x: 40, y: 50, width: 900, height: 700 };
        layouts.set("webchat", webchat);
        assert_eq!(layouts.get("webchat"), Some(webchat), "the web chat window position must be remembered too");

        // Unknown labels must not panic and must not be written in
        layouts.set("nope", rect);
        assert!(layouts.get("nope").is_none());
    }

    #[test]
    fn preset_lists_seed_from_legacy_presets() {
        // Old config files have only the flat `presets` field: normalize seeds
        // one default list from it, and the flat field mirrors the active list.
        let old = r#"{"presets":["解释报错","翻译"]}"#;
        let cfg = serde_json::from_str::<AppConfig>(old).unwrap().normalize();
        assert_eq!(cfg.preset_lists.len(), 1);
        assert_eq!(cfg.preset_lists[0].name, "默认");
        assert_eq!(cfg.preset_lists[0].presets.len(), 2);
        assert_eq!(cfg.active_preset_list, "默认");
        assert_eq!(cfg.presets, cfg.preset_lists[0].presets, "flat field mirrors the active list");
    }

    #[test]
    fn preset_lists_dedupe_and_repair_active() {
        // Duplicate list names get suffixes; a missing active name falls back to
        // the first list; empty names are dropped.
        let cfg = AppConfig {
            preset_lists: vec![
                PresetList { name: "  翻译  ".into(), presets: vec![PresetEntry::Simple("A".into())] },
                PresetList { name: "翻译".into(), presets: vec![PresetEntry::Simple("B".into())] },
                PresetList { name: "  ".into(), presets: vec![PresetEntry::Simple("C".into())] },
            ],
            active_preset_list: "gone".into(),
            ..Default::default()
        }
        .normalize();
        assert_eq!(cfg.preset_lists.len(), 2);
        assert_eq!(cfg.preset_lists[0].name, "翻译");
        assert_eq!(cfg.preset_lists[1].name, "翻译 2");
        assert_eq!(cfg.active_preset_list, "翻译");
        assert_eq!(cfg.presets.len(), 1, "flat field follows the active list");
    }
}
