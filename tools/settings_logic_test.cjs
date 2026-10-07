/*
 * settings.js 的逻辑验证：用一个极简 DOM 替身把页面跑起来，检查多配置列表的行为。
 *
 * 只验证"点了之后发了什么、列表变成什么样"，不验证样式和观感。
 * 用 `node tools/settings_logic_test.cjs` 跑（仓库的 package.json 是 type: module，
 * 所以这里必须是 .cjs）。不需要浏览器，也不需要装任何依赖。
 */
const fs = require("fs");
const vm = require("vm");
const path = require("path");

const APP = path.join(__dirname, "..", "src", "settings.js");

// ------------------------------------------------------------------ DOM 替身

class El {
  constructor(tag) {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parent = null;
    this.dataset = {};
    this.style = {};
    this.value = "";
    this.checked = false;
    this.type = "";
    this.placeholder = "";
    this.title = "";
    this._text = "";
    this._classes = new Set();
    this._listeners = {};
  }
  get className() {
    return [...this._classes].join(" ");
  }
  set className(v) {
    this._classes = new Set(String(v).split(/\s+/).filter(Boolean));
  }
  get classList() {
    const self = this;
    return {
      add: (c) => self._classes.add(c),
      remove: (c) => self._classes.delete(c),
      contains: (c) => self._classes.has(c),
      toggle: (c, on) => (on ? self._classes.add(c) : self._classes.delete(c)),
    };
  }
  get textContent() {
    return this.children.length ? this.children.map((c) => c.textContent).join("") : this._text;
  }
  set textContent(v) {
    this._text = String(v == null ? "" : v);
    this.children = [];
  }
  appendChild(child) {
    child.parent = this;
    this.children.push(child);
    return child;
  }
  append(...children) {
    for (const c of children) this.appendChild(c);
  }
  replaceChildren(...children) {
    this.children = [];
    this._text = "";
    this.append(...children);
  }
  addEventListener(type, fn) {
    (this._listeners[type] ||= []).push(fn);
  }
  focus() {}
  select() {}
  /** 触发监听器，返回全部返回值（handler 可能是 async，调用方自己 await Promise.all） */
  fire(type, event = {}) {
    const handlers = this._listeners[type] || [];
    return handlers.map((fn) =>
      fn({ stopPropagation() {}, preventDefault() {}, isComposing: false, keyCode: 0, ...event })
    );
  }
  matches(selector) {
    const m = /^\.([\w-]+)(?:\.([\w-]+))?$/.exec(selector);
    if (!m) throw new Error("替身只会匹配 .cls 或 .cls.cls，收到: " + selector);
    return this._classes.has(m[1]) && (!m[2] || this._classes.has(m[2]));
  }
  querySelector(selector) {
    const walk = (node) => {
      for (const child of node.children) {
        if (child.matches(selector)) return child;
        const deep = walk(child);
        if (deep) return deep;
      }
      return null;
    };
    return walk(this);
  }
  /** 找到所有后代（元素名或选择器都给不了时的兜底） */
  descendants() {
    const out = [];
    const walk = (node) => {
      for (const child of node.children) {
        out.push(child);
        walk(child);
      }
    };
    walk(this);
    return out;
  }
}

const IDS = {
  base_url: "input", api_key: "input", model: "input", extra_headers: "input", proxy: "input",
  hotkey: "input", timeout_secs: "input", system_prompt: "textarea", webchat_proxy: "input",
  max_image_edge: "input", jpeg_quality: "input", autostart: "checkbox", theme: "select",
  continue_conversation: "checkbox",
  preset: "select", "test-result": "div", "config-path": "span",
  "profile-list": "div", "profile-hint": "span", "hotkey-hint": "div",
  "btn-save": "button", "btn-test": "button", "btn-close": "button",
  "btn-try": "button", "btn-add-profile": "button", "btn-rename-profile": "button",
};

const elements = {};
for (const [id, kind] of Object.entries(IDS)) {
  const el = new El(kind === "checkbox" ? "input" : kind === "select" ? "select" : kind);
  if (kind === "checkbox") {
    el.type = "checkbox";
    el.checked = false;
  }
  elements[id] = el;
}

const document = {
  getElementById: (id) => elements[id] || null,
  createElement: (tag) => new El(tag),
  addEventListener: () => {},
};

// ------------------------------------------------------------------ 后端替身

const DEFAULT = {
  profiles: [
    { name: "配置1", base_url: "https://api.example.com/v1", api_key: "key-1", model: "m-1", extra_headers: "" },
    { name: "配置2", base_url: "https://backup.example.com/v1", api_key: "key-2", model: "m-2", extra_headers: "" },
  ],
  active_profile: "配置1",
  hotkey: "Ctrl+Shift+A",
  continue_conversation: true,
  system_prompt: "系统提示",
  max_image_edge: 1568,
  jpeg_quality: 85,
  timeout_secs: 120,
  autostart: false,
  has_used: true,
};

let store = JSON.parse(JSON.stringify(DEFAULT));
const saves = [];

function normalize(cfg) {
  const seen = [];
  cfg.profiles = cfg.profiles.filter((p) => {
    const name = String(p.name || "").trim();
    if (!name) return false;
    let unique = name, n = 2;
    while (seen.includes(unique)) unique = `${name}${n++}`;
    seen.push(unique);
    p.name = unique;
    return true;
  });
  if (!cfg.profiles.length) cfg.active_profile = "";
  else if (!cfg.profiles.some((p) => p.name === cfg.active_profile)) {
    cfg.active_profile = cfg.profiles[0].name;
  }
  return cfg;
}

const calls = [];
async function call(command, args) {
  calls.push({ command, args });
  switch (command) {
    case "get_config":
      return JSON.parse(JSON.stringify(store));
    case "get_config_path":
      return "C:\\Users\\test\\AppData\\Roaming\\com.askwithsnip.desktop\\config.json";
    case "save_config": {
      // 模拟后端：save 时也做一次 normalize，并且不会碰 has_used
      const incoming = JSON.parse(JSON.stringify(args.cfg));
      store = normalize(incoming);
      saves.push(JSON.parse(JSON.stringify(store)));
      return null;
    }
    default:
      return null;
  }
}

const toasts = [];
const window = {
  AskWithSnip: {
    call,
    listen: async () => {},
    installErrorReporting: () => {},
    toast: (msg, kind) => toasts.push({ msg, kind }),
    t: (s2) => s2,
    setLanguage: () => {},
    applyTheme: () => {},
    getLanguage: () => 'zh',
    applyI18n: () => {},
    installHotkeyRecorder: () => {},
  },
};

// ------------------------------------------------------------------ 跑起来

const sandbox = { window, document, console, setTimeout, Promise, JSON, Number, String, Set, Object };
vm.createContext(sandbox);
vm.runInContext(fs.readFileSync(APP, "utf8"), sandbox, { filename: "settings.js" });

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const chips = () => elements["profile-list"].children;
const chipNames = () => chips().map((c) => c.children[0].textContent);
const activeChip = () => chips().find((c) => c.classList.contains("active"));

let failures = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  ok   ${label}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${label}${detail ? "  -> " + detail : ""}`);
  }
}

(async () => {
  await tick();
  await tick();

  console.log("\n[1] 初始渲染");
  check("渲染出两个配置芯片", chips().length === 2, `实际 ${chips().length}`);
  check("名字是 配置1/配置2", JSON.stringify(chipNames()) === '["配置1","配置2"]', JSON.stringify(chipNames()));
  check("当前启用的那张高亮", activeChip() && activeChip().children[0].textContent === "配置1");
  check("表单填的是启用配置的内容", elements["base_url"].value === "https://api.example.com/v1", elements["base_url"].value);
  check("全局字段也填了", elements["hotkey"].value === "Ctrl+Shift+A");
  check("配置位置来自后端而不是写死", elements["config-path"].textContent.includes("com.askwithsnip.desktop"), elements["config-path"].textContent);

  console.log("\n[2] 点第二张芯片 -> 切换并立即落盘");
  chips()[1].fire("click");
  await tick();
  await tick();
  check("保存请求已发出", saves.length === 1, `实际 ${saves.length} 次`);
  check("active_profile 变成配置2", store.active_profile === "配置2", store.active_profile);
  check("高亮跟着换到第二张", activeChip() && activeChip().children[0].textContent === "配置2");
  check("表单换成配置2的地址", elements["base_url"].value === "https://backup.example.com/v1", elements["base_url"].value);
  check("并发出去的通知是成功的", toasts.at(-1) && toasts.at(-1).kind === "ok", JSON.stringify(toasts.at(-1)));

  console.log("\n[3] 改完字段再切走，改动不该丢");
  elements["base_url"].value = "https://edited.example.com/v1";
  elements["model"].value = "m-edited";
  chips()[0].fire("click");
  await tick();
  await tick();
  check("切回配置1生效", store.active_profile === "配置1", store.active_profile);
  check("刚才对配置2的编辑被收下了", store.profiles[1].base_url === "https://edited.example.com/v1", JSON.stringify(store.profiles[1]));
  check("表单显示配置1的地址", elements["base_url"].value === "https://api.example.com/v1", elements["base_url"].value);

  console.log("\n[4] 存为新配置：跳过已占用的名字");
  elements["base_url"].value = "https://third.example.com/v1";
  elements["api_key"].value = "key-3";
  elements["model"].value = "m-3";
  elements["btn-add-profile"].fire("click");
  await tick();
  await tick();
  check("新增了一张", store.profiles.length === 3, `实际 ${store.profiles.length}`);
  check("名字是配置3（配置1/2 已被占用）", store.profiles[2].name === "配置3", store.profiles[2].name);
  check("新配置继承了表单里的内容", store.profiles[2].api_key === "key-3" && store.profiles[2].model === "m-3", JSON.stringify(store.profiles[2]));
  check("新配置变成当前启用", store.active_profile === "配置3", store.active_profile);
  check("列表里渲染了三张", chips().length === 3, `实际 ${chips().length}`);

  console.log("\n[5] 重命名：撞名要拦住");
  elements["btn-rename-profile"].fire("click");
  const input = elements["profile-list"].querySelector(".profile-chip.active").children[0];
  check("高亮那张变成了输入框", input && input.tagName === "INPUT", input && input.tagName);
  input.value = "配置1"; // 已经被占用
  input.fire("keydown", { key: "Enter" });
  await tick();
  await tick();
  check("撞名被拒绝，名字没变", store.profiles[2].name === "配置3", store.profiles[2].name);
  check("给了错误提示", toasts.at(-1) && toasts.at(-1).kind === "err", JSON.stringify(toasts.at(-1)));

  console.log("\n[6] 重命名：正常改名");
  elements["btn-rename-profile"].fire("click");
  const input2 = elements["profile-list"].querySelector(".profile-chip.active").children[0];
  input2.value = "中转站";
  input2.fire("keydown", { key: "Enter" });
  await tick();
  await tick();
  check("名字改成了中转站", store.profiles[2].name === "中转站", store.profiles[2].name);
  check("active_profile 跟着改名", store.active_profile === "中转站", store.active_profile);
  check("改名不需要动配置1/2", store.profiles[0].name === "配置1" && store.profiles[1].name === "配置2");

  console.log("\n[7] 删除");
  chips()[1].children[2].fire("click"); // 第二张的 ✕
  await tick();
  await tick();
  check("少了一张", store.profiles.length === 2, `实际 ${store.profiles.length}`);
  check("删掉的是配置2", !store.profiles.some((p) => p.name === "配置2"), JSON.stringify(store.profiles.map((p) => p.name)));
  check("当前那张（中转站）不受影响", store.active_profile === "中转站", store.active_profile);

  console.log("\n[8] 删到只剩一张时不再显示删除按钮");
  chips()[0].children[2].fire("click"); // 删掉配置1
  await tick();
  await tick();
  check("只剩一张", store.profiles.length === 1, `实际 ${store.profiles.length}`);
  check("这唯一一张没有 ✕ 了", chips()[0].children.length === 2, `子元素 ${chips()[0].children.length} 个`);
  check("active_profile 回落到了剩下的那张", store.active_profile === "中转站", store.active_profile);

  console.log("\n[9] 保存时不会丢掉表单外的字段");
  elements["hotkey"].value = "Alt+Q";
  elements["system_prompt"].value = "新的系统提示";
  elements["max_image_edge"].value = "1024";
  elements["btn-save"].fire("click");
  await tick();
  await tick();
  check("热键写进去了", store.hotkey === "Alt+Q", store.hotkey);
  check("系统提示写进去了", store.system_prompt === "新的系统提示", store.system_prompt);
  check("图片上限写进去了", store.max_image_edge === 1024, String(store.max_image_edge));
  check("has_used 没被表单覆盖成默认值", store.has_used === true, String(store.has_used));

  console.log("\n[9b] 框选行为那个开关");
  check("打开时读出来是勾上的", elements["continue_conversation"].checked === true);
  elements["continue_conversation"].checked = false;
  elements["btn-save"].fire("click");
  await tick();
  await tick();
  check("取消勾选后落盘是 false", store.continue_conversation === false, String(store.continue_conversation));
  elements["continue_conversation"].checked = true;
  elements["btn-save"].fire("click");
  await tick();
  await tick();
  check("重新勾上后落盘是 true", store.continue_conversation === true, String(store.continue_conversation));
  check("切换它不影响别的字段", store.hotkey === "Alt+Q" && store.profiles.length === 1, JSON.stringify({ hotkey: store.hotkey, n: store.profiles.length }));

  console.log("\n[10] save_config 的调用形状");
  const payload = saves.at(-1);
  check("用的是 cfg 这个参数名（和后端命令签名一致）", calls.some((c) => c.command === "save_config" && c.args && c.args.cfg), JSON.stringify(Object.keys(calls.at(-1).args || {})));
  check("profiles 是数组", Array.isArray(payload.profiles), typeof payload.profiles);
  check("每次保存都带了 active_profile", typeof payload.active_profile === "string" && payload.active_profile.length > 0, payload.active_profile);

  console.log(`\n结果：${failures === 0 ? "全部通过" : failures + " 项失败"}`);
  process.exit(failures === 0 ? 0 : 1);
})();
