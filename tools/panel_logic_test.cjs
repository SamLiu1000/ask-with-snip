/*
 * panel.js 的逻辑验证：跑真的 common.js + panel.js，只把 DOM 和 Tauri 换成替身。
 *
 * 覆盖这次新加的左侧对话记录列表：渲染、点击打开、单条删除、清空、接着追问，
 * 以及八个缩放手柄是不是把拖拽转给了系统缩放循环。
 * 用 `node tools/panel_logic_test.cjs` 跑（仓库的 package.json 是 type: module，
 * 所以这里必须是 .cjs）。不需要浏览器，也不需要装任何依赖。
 */
const fs = require("fs");
const vm = require("vm");
const path = require("path");

const SRC = path.join(__dirname, "..", "src") + path.sep;

// ------------------------------------------------------------------ DOM 替身

class El {
  constructor(tag) {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.dataset = {};
    this.style = {};
    this.attrs = {};
    this.value = "";
    this.checked = false;
    this.disabled = false;
    this.placeholder = "";
    this.title = "";
    this.scrollTop = 0;
    this.scrollHeight = 0;
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
  get innerHTML() {
    return this._html || "";
  }
  set innerHTML(v) {
    this._html = String(v);
    // 真实 DOM 里改了 innerHTML 之后 textContent 也跟着变，这里近似一下（去掉标签）
    if (!this.children.length) this._text = String(v).replace(/<[^>]*>/g, "");
  }
  scrollIntoView() {}
  appendChild(child) {
    this.children.push(child);
    child._parent = this;
    return child;
  }
  append(...children) {
    for (const c of children) this.appendChild(c);
  }
  insertBefore(node, ref) {
    const at = this.children.indexOf(ref);
    if (at < 0) return this.appendChild(node);
    this.children.splice(at, 0, node);
    node._parent = this;
    return node;
  }
  /** 只认 `.class` 这种最简单的选择器——面板里用到的就这一种 */
  querySelectorAll(sel) {
    const cls = String(sel).replace(/^\./, "");
    return this.descendants().filter((c) => c._classes.has(cls));
  }
  closest(sel) {
    const cls = String(sel).replace(/^\./, "");
    let node = this;
    while (node) {
      if (node._classes && node._classes.has(cls)) return node;
      node = node._parent;
    }
    return null;
  }
  remove() {
    if (this._parent) this._parent.children = this._parent.children.filter((c) => c !== this);
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
  removeAttribute(name) {
    // 真实 DOM 里删掉属性之后，IDL 属性（src/href 这类）读出来就是 undefined
    delete this.attrs[name];
  }
  get src() {
    return this.attrs.src;
  }
  set src(value) {
    this.attrs.src = String(value);
  }
  setAttribute(name, value) {
    this.attrs[name] = value;
  }
  getAttribute(name) {
    return this.attrs[name];
  }
  fire(type, event = {}) {
    return (this._listeners[type] || []).map((fn) =>
      fn({ stopPropagation() {}, preventDefault() {}, isComposing: false, keyCode: 0, ...event })
    );
  }
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
  find(cls) {
    return this.descendants().filter((c) => c._classes.has(cls));
  }
}

const IDS = [
  "thread", "thumb", "prompt", "btn-send", "status", "presets",
  "history-list", "btn-close", "btn-lang",
  "btn-settings", "btn-again", "btn-clear-history", "btn-new-chat",
  "btn-copy", "btn-continue", "btn-backend", "btn-reveal", "btn-reload", "btn-theme",
  "film", "turn-rail", "account-box", "account-btn", "account-menu", "site-select", "btn-login",
  "more-box", "more-menu", "btn-more",
  "lightbox", "lightbox-img", "lightbox-select", "lightbox-delete", "lightbox-close",
];
const elements = {};
for (const id of IDS) elements[id] = new El(id === "prompt" ? "textarea" : "div");
elements["presets"] = new El("div");

// 面板源码里 getElementById 的每个 id，替身文档都必须有——真事：HTML 里删了 #thumb，
// JS 还在引用，面板一启动就崩在 addEventListener 上，整页初始化中断，
// 而替身按 IDS 建元素永远"齐"，测不出来。这里直接对源码做静态检查。
{
  const src =
    fs.readFileSync(SRC + "panel.js", "utf8") +
    fs.readFileSync(SRC + "common.js", "utf8");
  const wanted = new Set(
    [...src.matchAll(/getElementById\("([^"]+)"\)/g)].map((m) => m[1])
  );
  // 源码自己造的元素（createElement 之后 .id = "xxx"）不算缺：toast 就是这么来的
  const created = new Set(
    [...src.matchAll(/\.id\s*=\s*"([^"]+)"/g)].map((m) => m[1])
  );
  const missing = [...wanted].filter((id) => !elements[id] && !created.has(id));
  if (missing.length) {
    console.log(
      `FAIL 面板引用的元素在替身文档里缺: ${missing.join(", ")}（panel.html 里也得有）`
    );
    process.exit(1);
  }
}

const resizeHandles = ["North", "South", "East", "West", "NorthWest", "NorthEast", "SouthWest", "SouthEast"]
  .map((dir) => {
    const el = new El("div");
    el.dataset.resize = dir;
    return el;
  });

const body = new El("body");
const documentListeners = {};
const document = {
  body,
  getElementById: (id) => elements[id] || null,
  createElement: (tag) => new El(tag),
  // 文本节点用 El 顶替：它的 textContent 就是那段文字，leaf 行为一致
  createTextNode: (text) => {
    const node = new El("#text");
    node.textContent = String(text);
    return node;
  },
  querySelectorAll: (sel) => (sel === "[data-resize]" ? resizeHandles : []),
  addEventListener: (type, fn) => {
    (documentListeners[type] ||= []).push(fn);
  },
};
const fireDocument = (type, event) =>
  (documentListeners[type] || []).map((fn) =>
    fn({ stopPropagation() {}, preventDefault() {}, isComposing: false, keyCode: 0, ...event })
  );

// ------------------------------------------------------------------ Tauri 替身

const resized = [];
const window = {
  __TAURI__: {
    core: {
      async invoke(command, args) {
        return backend(command, args);
      },
    },
    event: {
      async listen(name, handler) {
        (listeners[name] ||= []).push(handler);
        return () => {};
      },
    },
    window: {
      getCurrentWindow: () => ({
        async startResizeDragging(direction) {
          resized.push(direction);
        },
      }),
    },
  },
  addEventListener: () => {},
};

const listeners = {};
const emit = async (name, payload) => {
  for (const fn of listeners[name] || []) await fn({ payload });
};

// ------------------------------------------------------------------ 后端替身

const HISTORY = [
  { id: "1000-0", created_at: Date.now() - 60_000, title: "解释这段报错", turn_count: 1, crop_w: 400, crop_h: 200 },
  { id: "900-0", created_at: Date.now() - 3_600_000, title: "翻译一下", turn_count: 2, crop_w: 300, crop_h: 100 },
];

function conversation(id) {
  if (id === "900-0") {
    return {
      id: "900-0", createdAt: Date.now() - 3_600_000, dataUrl: "data:image/jpeg;base64,OLD",
      cropW: 300, cropH: 100, sentW: 300, sentH: 100,
      turns: [
        { question: "翻译一下", answer: "旧的第一答" },
        { question: "再详细点", answer: "旧的第二答" },
      ],
    };
  }
  return {
    id: "1000-0", createdAt: Date.now() - 60_000, dataUrl: "data:image/jpeg;base64,NEW",
    cropW: 800, cropH: 600, sentW: 700, sentH: 525,
    turns: [{ question: "解释这段报错", answer: "第一段的回答" }],
  };
}

const calls = [];
let current = null; // 当前对话（和后端 AppState 里那份对应）
/** 面板会读改写这份配置（"接着上文"那个开关），所以替身里也要有一份 */
let config = {
  profiles: [{ name: "配置1", base_url: "https://x/v1", api_key: "k", model: "m", extra_headers: "" }],
  active_profile: "配置1",
  hotkey: "Ctrl+Shift+A",
  continue_conversation: false,
  system_prompt: "",
  max_image_edge: 1568,
  jpeg_quality: 85,
  timeout_secs: 120,
  autostart: false,
  has_used: true,
  preset_lists: [
    { name: "默认", presets: [
      { name: "翻译成中文", prompt: "把截图里的所有内容翻译成中文，保持原有排版" },
      { name: "翻译成英文", prompt: "把截图里的所有内容翻译成英文，保持原有排版" },
      { name: "解释报错", prompt: "解释这张截图里的报错" },
      { name: "总结要点", prompt: "" },
    ] },
    { name: "工作", presets: [
      { name: "写周报", prompt: "根据截图内容写一份简洁的工作周报" },
    ] },
  ],
  active_preset_list: "默认",
};
const saves = [];

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const settle = async (n = 4) => {
  for (let i = 0; i < n; i += 1) await tick();
};

/** 真实情况下这次 await 会一直持续到流结束，增量在期间到达，所以这里挂起等人来收尾 */
let pendingAsk = null;
const resolveAsk = async (text) => {
  const pending = pendingAsk;
  pendingAsk = null;
  pending.resolve(text);
  await settle(4);
};
const rejectAsk = async (err) => {
  const pending = pendingAsk;
  pendingAsk = null;
  pending.reject(err);
  await settle(4);
};

async function backend(command, args) {
  calls.push({ command, args });
  switch (command) {
    case "get_config":
      return JSON.parse(JSON.stringify(config));
    case "save_config":
      config = JSON.parse(JSON.stringify(args.cfg));
      saves.push(JSON.parse(JSON.stringify(config)));
      return null;
    case "get_conversation":
      return current;
    case "list_history":
      return HISTORY.filter((item) => !deleted.has(item.id));
    case "load_conversation": {
      const conv = conversation(args.id);
      current = conv;
      return conv;
    }
    case "delete_conversation": {
      deleted.add(args.id);
      if (current && current.id === args.id) current = null;
      return null;
    }
    case "clear_history":
      deleted.add("*");
      HISTORY.length = 0;
      current = null;
      return null;
    case "toggle_staged": {
      const conv = current;
      const item = conv && conv.staged && conv.staged[args.index];
      if (!item) throw new Error("没有第 " + (args.index + 1) + " 张截图");
      item.selected = !item.selected;
      return item.selected;
    }
    case "remove_staged": {
      const conv = current;
      if (!conv || !conv.staged || !conv.staged[args.index]) {
        throw new Error("没有第 " + (args.index + 1) + " 张截图");
      }
      conv.staged.splice(args.index, 1);
      return null;
    }
    case "remove_turn_image": {
      const conv = current;
      const turn = conv && conv.turns && conv.turns[args.turn];
      if (!turn || !turn.images || !turn.images[args.image]) {
        throw new Error("第 " + (args.turn + 1) + " 轮没有第 " + (args.image + 1) + " 张图");
      }
      turn.images.splice(args.image, 1);
      return null;
    }
    case "snip_for_staging":
      return null;
    case "set_preset_lists":
      config.preset_lists = JSON.parse(JSON.stringify(args.lists));
      config.active_preset_list = args.active;
      return null;
    case "ask":
    case "regenerate":
    case "edit_question":
      return new Promise((resolve, reject) => {
        pendingAsk = { resolve, reject, prompt: args && args.prompt, command };
      });
    default:
      return null;
  }
}
const deleted = new Set();

const sandbox = {
  window, document, console, setTimeout, clearTimeout, setInterval, clearInterval,
  Promise, JSON, Number, String,
  Set, Map, Object, Array, Date, Math, Error, RegExp, Boolean, requestAnimationFrame: (fn) => setTimeout(fn, 0),
};
sandbox.globalThis = sandbox;
sandbox.window.__TAURI__ = window.__TAURI__;
vm.createContext(sandbox);

const unhandled = [];
process.on("unhandledRejection", (err) => {
  unhandled.push(String(err));
  // 断言卡在这一步之前的某处时会走到这里：直接打出来，别让整个脚本静悄悄地停住
  console.error("\n!! 未捕获的 promise 异常:", (err && err.stack) || err);
});

vm.runInContext(fs.readFileSync(SRC + "common.js", "utf8"), sandbox, { filename: "common.js" });
sandbox.window.AskWithSnip = sandbox.window.AskWithSnip; // common.js 自己挂上去的
vm.runInContext(fs.readFileSync(SRC + "panel.js", "utf8"), sandbox, { filename: "panel.js" });

const list = elements["history-list"];
const rows = () => list.children.filter((c) => c._classes.has("history-item"));
const rowTitles = () => rows().map((r) => r.children[0].textContent);
const activeRow = () => rows().find((r) => r.classList.contains("active"));

let failures = 0;
function check(label, condition, detail) {
  if (condition) console.log(`  ok   ${label}`);
  else {
    failures += 1;
    console.log(`  FAIL ${label}${detail ? "  -> " + detail : ""}`);
  }
}

(async () => {
  await settle();

  console.log("\n[1] 还没有截图时的空状态");
  check("左边列出了两条历史", rows().length === 2, `实际 ${rows().length}`);
  check("标题按后端给的顺序", JSON.stringify(rowTitles()) === '["解释这段报错","翻译一下"]', JSON.stringify(rowTitles()));
  check("没有哪条是高亮的", !activeRow());
  // 现在支持纯文字提问（没截图也能问），空状态输入框刻意保持可用——见 panel.js renderConversation
  check("空状态输入框可用（纯文字提问）", elements["prompt"].disabled === false);
  check("输入框占位文案说清楚了怎么开始", elements["prompt"].placeholder.includes("回车发送"), elements["prompt"].placeholder);
  check("发送按钮可用", elements["btn-send"].disabled === false);
  check("中间区给了上手提示", elements["thread"].children.length > 0 && elements["thread"].children[0].id === "empty-hint");
  check("预设按钮建好了（列表选择器 + 4 条 + 编辑按钮）", elements["presets"].children.length === 6, `实际 ${elements["presets"].children.length}`);
  check("列表选择器显示当前列表名", elements["presets"].children[0].children[0].textContent.includes("默认"), elements["presets"].children[0].children[0].textContent);
  check("八个缩放手柄都挂上了监听", resizeHandles.every((h) => (h._listeners.mousedown || []).length === 1));

  console.log("\n[2] 新截了一张图（收到 conversation-changed）");
  current = conversation("1000-0");
  await emit("conversation-changed");
  await settle();
  check("中间区渲染出了这一轮问答", elements["thread"].find("turn").length === 1, `实际 ${elements["thread"].find("turn").length}`);
  check("问句显示出来了", elements["thread"].textContent.includes("解释这段报错"));
  check("回答显示出来了", elements["thread"].textContent.includes("第一段的回答"));
  check("缩略图换成了新那张", elements["thumb"].src === "data:image/jpeg;base64,NEW", elements["thumb"].src);
  check("状态行已经删掉了（改由胶片条自己表达）", !elements["shot-meta"]);
  check("输入框可以用了", elements["prompt"].disabled === false);
  check("左边对应那条高亮了", activeRow() && activeRow().dataset.id === "1000-0", activeRow() && activeRow().dataset.id);

  console.log("\n[2b] 刚截完还没提问：界面上不该再多一句提示，输入框直接能用");
  // 真事：这里原来摆着"截图好了，输入问题就行——或者点下面的预设指令。"，用户要求删掉。
  const fresh = conversation("1000-0");
  fresh.turns = [];
  current = fresh;
  await emit("conversation-changed");
  await settle();
  check("那句提示（#shot-hint）整个没了", !elements["shot-hint"]);
  const hintSrc = fs.readFileSync(SRC + "panel.js", "utf8") + fs.readFileSync(SRC + "panel.html", "utf8");
  check("也没有人再往它上面写字", !/shot-hint|shotHint/.test(hintSrc));
  check("对话区里也不重复", !elements["thread"].textContent.includes("截图好了"), elements["thread"].textContent);
  check("输入框直接可用", elements["prompt"].disabled === false);
  check("预览照常显示", elements["thumb"].hidden === false);
  check("对话区是空的（没有第二处提示接手）", elements["thread"].textContent.trim() === "",
    JSON.stringify(elements["thread"].textContent));

  current = conversation("1000-0");
  await emit("conversation-changed");
  await settle();
  console.log("\n[3] 点左边的另一条 -> 调出来接着追问");
  rows()[1].fire("click");
  await settle();
  check("调的是 load_conversation 并且带上了 id", calls.some((c) => c.command === "load_conversation" && c.args.id === "900-0"));
  check("切换成了那一段的两轮问答", elements["thread"].find("turn").length === 2, `实际 ${elements["thread"].find("turn").length}`);
  check("内容是旧那段", elements["thread"].textContent.includes("旧的第二答"));
  check("高亮跟着挪过去", activeRow() && activeRow().dataset.id === "900-0");
  check("缩略图也跟着换", elements["thumb"].src.includes("OLD"));

  console.log("\n[4] 追问：回车发送");
  elements["prompt"].value = "那该怎么办";
  elements["prompt"].fire("keydown", { key: "Enter", shiftKey: false });
  await settle(3);
  check("问的是当前这一段", calls.filter((c) => c.command === "ask").length === 1);
  check("提问内容原样传过去", calls.find((c) => c.command === "ask").args.prompt === "那该怎么办");
  check("输入框被清空了", elements["prompt"].value === "");
  // 忙的时候按钮不disabled：它变成了「取消」，用户随时能中止
  check("进行中时按钮改成取消且可点", elements["btn-send"].textContent === "取消" && elements["btn-send"].disabled === false,
    `${elements["btn-send"].textContent} disabled=${elements["btn-send"].disabled}`);
  check("进行中时输入框锁住", elements["prompt"].disabled === true);
  await resolveAsk("流式回来的一整段回答");
  check("追答渲染进了同一段", elements["thread"].textContent.includes("流式回来的一整段回答"));
  check("答完把发送按钮恢复", elements["btn-send"].disabled === false && elements["btn-send"].textContent === "提问",
    `${elements["btn-send"].textContent} disabled=${elements["btn-send"].disabled}`);
  check("答完解锁输入框", elements["prompt"].disabled === false);
  check("答完刷新了一次列表", calls.filter((c) => c.command === "list_history").length >= 2);

  console.log("\n[5] 输入法组词时按回车不该发出去");
  const before = calls.filter((c) => c.command === "ask").length;
  elements["prompt"].value = "半截话";
  elements["prompt"].fire("keydown", { key: "Enter", isComposing: true });
  await settle(3);
  check("没有发出请求", calls.filter((c) => c.command === "ask").length === before);
  check("输入框里的字还在", elements["prompt"].value === "半截话");
  elements["prompt"].value = "";

  console.log("\n[6] 预设按钮");
  elements["presets"].find("preset")[3].fire("click");
  await settle(3);
  const asks = calls.filter((c) => c.command === "ask");
  check("点预设等于发了一条提问", asks.length === before + 1, `实际 ${asks.length}`);
  check("发的是按钮上的字", asks.at(-1).args.prompt === "总结要点", asks.at(-1).args.prompt);
  await resolveAsk("预设的回答");
  console.log("\n[6b] 快捷用语列表：切换并持久化");
  let pickerBox = elements["presets"].children[0];
  pickerBox.children[0].fire("click"); // 打开菜单
  check("点开列表菜单", pickerBox.children[1].hidden === false);
  const menuRows = pickerBox.children[1].find("preset-list-option");
  check("菜单里列了两条列表", menuRows.length === 2, `实际 ${menuRows.length}`);
  menuRows[1].fire("click"); // 切到 "工作"
  await settle(3);
  const listSetCalls = calls.filter((c) => c.command === "set_preset_lists");
  check("切换调用 set_preset_lists 且 active=工作", listSetCalls.length >= 1 && listSetCalls.at(-1).args.active === "工作",
    JSON.stringify(listSetCalls.at(-1) && listSetCalls.at(-1).args));
  check("按钮换成新列表的那一条", elements["presets"].find("preset").length === 1, `实际 ${elements["presets"].find("preset").length}`);
  check("选择器文本跟着变了", elements["presets"].children[0].children[0].textContent.includes("工作"),
    elements["presets"].children[0].children[0].textContent);
  elements["presets"].find("preset")[0].fire("click");
  await settle(3);
  check("发的是新列表里的预设", calls.filter((c) => c.command === "ask").at(-1).args.prompt === "根据截图内容写一份简洁的工作周报",
    calls.filter((c) => c.command === "ask").at(-1).args.prompt);
  await resolveAsk("新列表的回答");
  pickerBox = elements["presets"].children[0];
  pickerBox.children[0].fire("click");
  pickerBox.children[1].find("preset-list-option")[0].fire("click"); // 切回 "默认"
  await settle(3);
  check("切回默认列表", elements["presets"].find("preset").length === 4, `实际 ${elements["presets"].find("preset").length}`);


  console.log("\n[7] 删掉一条记录");
  rows()[0].children[2].fire("click"); // ✕
  await settle();
  check("调了 delete_conversation 带对了 id", calls.some((c) => c.command === "delete_conversation" && c.args.id === "1000-0"));
  check("列表刷新成一条", rows().length === 1, `实际 ${rows().length}`);
  check("点 ✕ 不会顺手把那一条打开", !calls.some((c) => c.command === "load_conversation" && c.args.id === "1000-0"));

  console.log("\n[8] 删掉正在看的那一条会退回空状态");
  rows()[0].children[2].fire("click"); // 现在唯一一条就是 900-0，也就是当前这段
  await settle();
  check("列表空了", rows().length === 0, `实际 ${rows().length}`);
  check("给了空状态文案", list.children.length === 1 && list.children[0]._classes.has("side-empty"), JSON.stringify(list.children.map((c) => c.className)));
  check("中间区退回未截图状态", elements["thread"].children[0].id === "empty-hint");
  check("输入框保持可用（纯文字提问）", elements["prompt"].disabled === false);

  console.log("\n[9] 清空全部");
  current = null;
  HISTORY.push({ id: "2000-0", created_at: Date.now(), title: "临时", turn_count: 1, crop_w: 10, crop_h: 10 });
  await emit("conversation-changed");
  await settle();
  check("又有一条了", rows().length === 1, `实际 ${rows().length}`);
  elements["btn-clear-history"].fire("click");
  await settle();
  check("调了 clear_history", calls.some((c) => c.command === "clear_history"));

  console.log("\n[10] 顶栏按钮");
  elements["btn-copy"].fire("click");
  await settle();
  check("没有内容时复制不报错、只提示", !calls.some((c) => c.command === "copy_text"));
  current = conversation("1000-0");
  await emit("conversation-changed");
  await settle();
  elements["btn-copy"].fire("click");
  await settle();
  const copy = calls.find((c) => c.command === "copy_text");
  check("有内容时把整段问答交给剪贴板", copy && copy.args.text.includes("问：解释这段报错") && copy.args.text.includes("第一段的回答"), copy && copy.args.text);

  // 网页版：和自动提问是两条独立的路，点它只把图交出去，不该动提问那条链路
  // 后端开关：接口 <-> 内嵌网页版。切它只改一个配置字段，不该碰提问那条链路
  const asksBeforeBackend = calls.filter((c) => c.command === "ask").length;
  const savesBefore = saves.length;
  check("默认显示走接口", elements["btn-backend"].textContent === "接口", elements["btn-backend"].textContent);
  check("走接口时不给看「显示网页」", elements["btn-reveal"].hidden === true);

  elements["btn-backend"].fire("click");
  await settle();
  const switched = saves[saves.length - 1];
  check("点一下切成网页版并落盘", saves.length === savesBefore + 1 && switched.backend === "webchat", JSON.stringify(switched && switched.backend));
  check("按钮上标出来当前是网页版", elements["btn-backend"].textContent.includes("网页版"), elements["btn-backend"].textContent);
  check("切完才给看「显示网页」", elements["btn-reveal"].hidden === false);
  check("切换不碰接口配置", switched.active_profile === config.active_profile);
  check("切换也不会去提问", calls.filter((c) => c.command === "ask").length === asksBeforeBackend);

  elements["btn-reveal"].fire("click");
  await settle();
  check("「显示网页」调 reveal_webchat", calls.some((c) => c.command === "reveal_webchat"));

  elements["btn-backend"].fire("click");
  await settle();
  check("再点一下切回接口", saves[saves.length - 1].backend === "api", JSON.stringify(saves[saves.length - 1].backend));
  check("切回接口后「显示网页」又收起来", elements["btn-reveal"].hidden === true);

  elements["btn-close"].fire("click");
  await settle();
  check("✕ 走的是 hide_panel（隐藏而不是销毁）", calls.some((c) => c.command === "hide_panel"));

  elements["btn-again"].fire("click");
  await settle();
  check("重选会先收起面板再触发框选", calls.some((c) => c.command === "hide_panel") && calls.some((c) => c.command === "trigger_snip"));

  const hidesBefore = calls.filter((c) => c.command === "hide_panel").length;
  fireDocument("keydown", { key: "Escape" });
  await settle(2);
  check("Esc 收起面板", calls.filter((c) => c.command === "hide_panel").length === hidesBefore + 1);
  const hidesNow = calls.filter((c) => c.command === "hide_panel").length;
  fireDocument("keydown", { key: "Escape", isComposing: true });
  await settle(2);
  check("输入法组词时的 Esc 不收起面板", calls.filter((c) => c.command === "hide_panel").length === hidesNow);

  console.log("\n[11] 缩放手柄把拖拽交给系统缩放循环");
  const southEast = resizeHandles.find((h) => h.dataset.resize === "SouthEast");
  southEast.fire("mousedown", { button: 0 });
  await settle(2);
  check("右下角那个报的是 SouthEast", resized.includes("SouthEast"), JSON.stringify(resized));
  resizeHandles[0].fire("mousedown", { button: 0 });
  await settle(2);
  check("上边缘报 North（PascalCase，符合 Tauri 的方向枚举）", resized.includes("North"), JSON.stringify(resized));
  const countBefore = resized.length;
  resizeHandles[0].fire("mousedown", { button: 2 });
  await settle(2);
  check("右键按下不触发缩放", resized.length === countBefore);

  console.log("\n[12] 流式事件");
  current = conversation("1000-0");
  await emit("conversation-changed");
  await settle();
  elements["prompt"].value = "流式问题";
  elements["prompt"].fire("keydown", { key: "Enter" });
  await settle(2);
  check("回答区在等待时是 pending", elements["thread"].find("pending").length === 1, `实际 ${elements["thread"].find("pending").length}`);
  await emit("ai-delta", "第一段");
  await emit("ai-delta", "第二段");
  await settle(3);
  check("增量拼进了正在回答的位置", elements["thread"].textContent.includes("第一段第二段"), elements["thread"].textContent.slice(-40));
  await resolveAsk("第一段第二段第三段");
  check(
    "收尾时用完整回答覆盖，不是把增量又拼一遍",
    elements["thread"].textContent.includes("第一段第二段第三段")
      && !elements["thread"].textContent.includes("第一段第二段第一段第二段"),
    elements["thread"].textContent.slice(-40)
  );
  check("答完不再是 pending", elements["thread"].find("pending").length === 0);

  console.log("\n[12b] 流到一半失败：同一句话不该显示两遍");
  elements["prompt"].value = "会失败的问题";
  elements["prompt"].fire("keydown", { key: "Enter" });
  await settle(2);
  await emit("ai-delta", "已经收到的部分");
  await settle(2);
  // 真实顺序：Rust 先广播 ai-error，再让 ask 以同样的消息失败
  await emit("ai-error", "接口返回 HTTP 401: invalid api key");
  await rejectAsk("接口返回 HTTP 401: invalid api key");
  const threadText = elements["thread"].textContent;
  check("错误显示在对话里", threadText.includes("401"));
  check("错误只出现一次（前端不会再补一遍）", threadText.split("401").length - 1 === 1, `出现 ${threadText.split("401").length - 1} 次`);
  check("状态栏也给出了原因", elements["status"].textContent.includes("401"), elements["status"].textContent);
  check("失败后按钮恢复可用", elements["btn-send"].disabled === false);

  console.log("\n[12c] 中途取消 + 思维链");
  elements["prompt"].value = "要思考很久的问题";
  elements["prompt"].fire("keydown", { key: "Enter" });
  await settle(2);
  await emit("ai-reasoning", "先读题…");
  await emit("ai-reasoning", "再想想条件。");
  await settle(2);
  const reasonBox = elements["thread"].find("reasoning")[0];
  check("思维链摆在可折叠块里", !!reasonBox && reasonBox.tagName === "DETAILS", reasonBox && reasonBox.tagName);
  check("默认就展开，不用再点一下", reasonBox.open === true);
  const reasonBody = reasonBox.find("reasoning-body")[0];
  check("推理文字原样拼进去", reasonBody.textContent === "先读题…再想想条件。", JSON.stringify(reasonBody.textContent));
  const lastTurn = elements["thread"].find("turn").at(-1);
  check("思考过程排在回答上面", lastTurn.children.indexOf(reasonBox) < lastTurn.children.indexOf(lastTurn.find("a")[0]));
  check("有思维链时状态栏说的是「思考中」", elements["status"].textContent.startsWith("思考中"), elements["status"].textContent);

  const asksBeforeCancel = calls.filter((c) => c.command === "ask").length;
  elements["btn-send"].fire("click");
  await settle(3);
  check("忙的时候点按钮是取消，不是又发一条", calls.filter((c) => c.command === "ask").length === asksBeforeCancel);
  check("取消调的是 cancel_ask", calls.some((c) => c.command === "cancel_ask"));
  check("状态栏立刻说明正在取消", elements["status"].textContent.includes("取消"), elements["status"].textContent);
  // 取消之后 Rust 那条 ask 会带着已收到的部分正常返回
  await resolveAsk("");
  check("一个字都没到手时说一声（已取消）", elements["thread"].textContent.includes("（已取消）"));
  // 取消是这条路正常结束（带着已收到的部分返回），所以状态栏清空、由 toast 说一声
  check("取消后状态栏不留下失败字样", !elements["status"].textContent.includes("401"), elements["status"].textContent);
  check("弹一条「已取消」的提示", body.descendants().some((el) => el.id === "toast" && el.textContent.includes("已取消")));
  check("取消后按钮回到「提问」", elements["btn-send"].textContent === "提问");

  console.log("\n[12d] 取消引起的失败不该再报一次错");
  elements["prompt"].value = "取消掉的问题";
  elements["prompt"].fire("keydown", { key: "Enter" });
  await settle(2);
  elements["btn-send"].fire("click");
  await settle(2);
  await emit("ai-error", "已取消，这次没有收到任何内容。");
  await rejectAsk("已取消，这次没有收到任何内容。");
  check("取消不算错误，这一轮不写红字", !elements["thread"].find("turn").at(-1).classList.contains("error"));
  check("状态栏也不显示那句失败", !elements["status"].textContent.includes("已取消，这次没有收到任何内容。"), elements["status"].textContent);

  console.log("\n[12e] ai-replace：网页版重画了输出过的部分，整段替换而不是追加");
  // 真事：站点在流式过程中把光标那一小段重画了（半截的表格行被补全），
  // 早先的写法把整篇又追加了一遍，600 字的回答在面板上变成了四遍重复。
  elements["prompt"].value = "表格那个问题";
  elements["prompt"].fire("keydown", { key: "Enter" });
  await settle(2);
  await emit("ai-delta", "从这张截图来看，这极大概率是《侠盗猎车手5》。\n\n| 文件名 | 说明 |\n\n| --- | --- |\n\n| **GTA5.exe** | 游戏 |");
  await settle(2);
  const redrawn = "从这张截图来看，这极大概率是《侠盗猎车手5》。\n\n| 文件名 | 说明 |\n\n| --- | --- |\n\n| **GTA5.exe** | 游戏主程序执行文件 |\n\n| **PlayGTAV.exe** | 启动工具 |";
  await emit("ai-replace", { answer: redrawn });
  await settle(2);
  const threadNow = elements["thread"].textContent;
  check("整段替换后只剩一份", threadNow.split("这极大概率是《侠盗猎车手5》").length - 1 === 1,
    `出现了 ${threadNow.split("这极大概率是《侠盗猎车手5》").length - 1} 次`);
  check("替换进来的新内容在", threadNow.includes("PlayGTAV.exe"));
  await emit("ai-replace", { reasoning: "重新想的这一段。" });
  await settle(2);
  check("思维链也能整段替换", elements["thread"].textContent.includes("重新想的这一段。"));
  await resolveAsk(redrawn);

  console.log("\n[12f] 重新生成：问题不动，回答重来");
  const actionButtons = () => elements["thread"].find("turn-actions").at(-1);
  check("最后一轮底下有两个按钮", (() => {
    const row = actionButtons();
    return row && row.children.length === 2 && row.textContent.includes("重新生成") && row.textContent.includes("修改提问");
  })(), actionButtons() && actionButtons().textContent);
  check("只有最后一轮才有按钮", elements["thread"].find("turn-actions").length === 1,
    `实际 ${elements["thread"].find("turn-actions").length} 组`);

  const asksBeforeRegen = calls.filter((c) => c.command === "ask").length;
  const turnsBeforeRegen = elements["thread"].find("turn").length;
  actionButtons().children[0].fire("click");
  await settle(3);
  check("调的是 regenerate，不是又发一条新问题", calls.some((c) => c.command === "regenerate")
    && calls.filter((c) => c.command === "ask").length === asksBeforeRegen);
  check("问题那一行还在", elements["thread"].textContent.includes("表格那个问题"));
  check("旧回答先清掉了", !elements["thread"].textContent.includes("PlayGTAV.exe"));
  check("跑着的时候按钮收起来", elements["thread"].classList.contains("busy") === true);
  await resolveAsk("重答的正文");
  check("新回答写进了同一轮", elements["thread"].textContent.includes("重答的正文"));
  check("答完按钮又出来", elements["thread"].classList.contains("busy") === false && actionButtons());
  check("没有多出一轮（重答是原地替换）", elements["thread"].find("turn").length === turnsBeforeRegen,
    `${turnsBeforeRegen} -> ${elements["thread"].find("turn").length}`);

  // 重答的时候思维链也要跟着换掉：上一轮的思考过程不能挂在新回答上面
  actionButtons().children[0].fire("click");
  await settle(2);
  await emit("ai-reasoning", "重答时的思考");
  await settle(2);
  check("重答中途的思维链照常显示", elements["thread"].textContent.includes("重答时的思考"));
  await resolveAsk("第二次重答");
  check("重答完思维链还在（这一轮的）", elements["thread"].textContent.includes("重答时的思考"));
  actionButtons().children[0].fire("click");
  await settle(2);
  check("再点一次重新生成：旧的思考过程清掉了", !elements["thread"].textContent.includes("重答时的思考"),
    elements["thread"].textContent.slice(-60));
  await resolveAsk("第三次重答");
  check("第三次重答也落在了同一轮", elements["thread"].textContent.includes("第三次重答"));

  console.log("\n[12g] 修改提问：原地改完重问");
  actionButtons().children[1].fire("click");
  await settle(2);
  const editor = elements["thread"].find("q-edit").at(-1);
  check("那一行变成编辑框", !!editor);
  const area = editor.children[1]; // [strip, area, buttons]
  check("框里是原来那句话", area.value === "表格那个问题", area.value);
  area.value = "改成这句话";
  editor.children[2].children[0].fire("click"); // 重问
  await settle(3);
  const editCall = calls.filter((c) => c.command === "edit_question").at(-1);
  check("调的是 edit_question 并带上改后的字", editCall && editCall.args.prompt === "改成这句话",
    JSON.stringify(editCall && editCall.args));
  check("那一行换成了新问题", elements["thread"].textContent.includes("改成这句话"));
  check("编辑框收起来了", elements["thread"].find("q-edit").length === 0);
  await resolveAsk("改问之后的新回答");
  check("新回答落在同一轮里", elements["thread"].textContent.includes("改问之后的新回答"));
  check("还是一轮，没有多出来", elements["thread"].find("turn").length === turnsBeforeRegen,
    `${turnsBeforeRegen} -> ${elements["thread"].find("turn").length}`);

  console.log("\n[12h] 修改提问按取消：什么都不动");
  actionButtons().children[1].fire("click");
  await settle(2);
  const cancelEditor = elements["thread"].find("q-edit").at(-1);
  cancelEditor.children[1].value = "改了又不算";
  cancelEditor.children[2].children[1].fire("click"); // 取消
  await settle(2);
  check("编辑框收起来了", elements["thread"].find("q-edit").length === 0);
  check("问题还是原来那句", elements["thread"].textContent.includes("改成这句话")
    && !elements["thread"].textContent.includes("改了又不算"));

  console.log("\n[12i] 胶片条：多图暂存、点选高亮、+ 追加截图");
  current = conversation("1000-0");
  current.staged = [
    { dataUrl: "data:image/jpeg;base64,ST1", selected: true, cropW: 800, cropH: 600, sentW: 700, sentH: 525 },
    { dataUrl: "data:image/jpeg;base64,ST2", selected: false, cropW: 400, cropH: 300, sentW: 400, sentH: 300 },
  ];
  await emit("conversation-changed");
  await settle();
  // 每张图外面套着一层 .film-cell（图 + 右上角的 ✕），取图要往里走一层
  const filmCells = () => elements["film"].children;
  const filmItems = () => filmCells().map((cell) => cell.children.find((c) => c.tagName === "IMG"));
  const filmDeletes = () =>
    filmCells().map((cell) => cell.children.find((c) => c.className === "film-del"));
  check("暂存区里画了两张", filmItems().length === 2, `实际 ${filmItems().length}`);
  check("选中状态跟着 Rust 走（第一张高亮）", filmItems()[0].classList.contains("selected")
    && !filmItems()[1].classList.contains("selected"));
  check("两张里只有一张高亮（高亮的才会发送）",
    filmItems().filter((i) => i.classList.contains("selected")).length === 1,
    String(filmItems().filter((i) => i.classList.contains("selected")).length));
  // 点击缩略图现在打开放大预览；选中/取消挪进了预览里的按钮（50px 的缩略图
  // 上误触一下就悄悄改了要发什么，太容易踩，也太难发现）。
  filmItems()[1].fire("click");
  await settle(2);
  check("点缩略图打开放大预览", elements["lightbox"].hidden === false
    && elements["lightbox-img"].src === "data:image/jpeg;base64,ST2");
  check("预览里是「选中发送」按钮（这张还没选中）",
    elements["lightbox-select"].textContent === "选中发送" && elements["lightbox-select"].hidden === false);
  await elements["lightbox-select"].onclick();
  await settle(2);
  check("预览按钮调的是 toggle_staged", calls.some((c) => c.command === "toggle_staged" && c.args.index === 1));
  check("高亮跟着切换", filmItems()[1].classList.contains("selected"));
  check("点过之后两张都高亮",
    filmItems().filter((i) => i.classList.contains("selected")).length === 2);
  check("按钮文字跟着翻面（已选中）", elements["lightbox-select"].textContent === "取消选中");

  console.log("\n[12k] 发完之后：已发出的那几张继续摆在胶片条上");
  {
    // 真事：发完之后暂存区清空，面板退回去显示"单张静态预览"，
    // 用户看到的是"我明明发了两张，怎么变成一张了"。
    const sentConv = conversation("1000-0");
    sentConv.staged = [];
    sentConv.sent = [
      { dataUrl: "data:image/jpeg;base64,SE1", turnIndex: 0, imageIndex: 0 },
      { dataUrl: "data:image/jpeg;base64,SE2", turnIndex: 0, imageIndex: 1 },
    ];
    sentConv.turns = [{ question: "这是谁，穿什么服装", answer: "答完了", imageCount: 2 }];
    current = sentConv;
    await emit("conversation-changed");
    await settle();
    const items = filmItems();   // 图现在包在 .film-cell 里，用助手取
    check("两张都还在胶片条上", items.length === 2, `实际 ${items.length}`);
    check("都标成已发送", items.length === 2 && items.every((i) => i.classList.contains("sent")));
    check("单张静态预览收起来了", elements["thumb"].hidden === true);
    check("两张都是「已发送」", items.length === 2 && items.every((i) => i.classList.contains("sent")));
    const callsBefore12k = calls.length;
    if (items.length) items[0].fire("click");
    await settle(2);
    check("已发送的那张点不动（没有 toggle_staged 调用）",
      !calls.slice(callsBefore12k).some((c) => c.command === "toggle_staged"));
  }

  console.log("\n[12l] 每张胶片右上角一个 ✕：暂存的=不发，已发出的=从记录里删");
  {
    const delConv = conversation("1000-0");
    delConv.turns = [
      { question: "两张图的问法", answer: "答", imageCount: 2, images: ["A", "B"] },
    ];
    delConv.staged = [
      { dataUrl: "data:image/jpeg;base64,ST1", selected: true, cropW: 800, cropH: 600, sentW: 700, sentH: 525 },
    ];
    delConv.sent = [
      { dataUrl: "data:image/jpeg;base64,SE1", turnIndex: 0, imageIndex: 0 },
      { dataUrl: "data:image/jpeg;base64,SE2", turnIndex: 0, imageIndex: 1 },
    ];
    current = delConv;
    await emit("conversation-changed");
    await settle();

    check("三张胶片各有一个 ✕", filmDeletes().filter(Boolean).length === 3,
      `实际 ${filmDeletes().filter(Boolean).length}`);
    check("顺序是「已发送」在前、待发的在后",
      filmItems().length === 3 && filmItems()[0].classList.contains("sent")
        && filmItems()[1].classList.contains("sent") && !filmItems()[2].classList.contains("sent"));

    // 删暂存的那张：走 remove_staged，而且不该顺带切换选中。
    // （点图本身开的是放大预览；选中在预览里操作，见 [12i]。）
    const callsBefore = calls.length;
    const stagedDel = filmDeletes()[2];
    const stagedImg = filmItems()[2];
    stagedImg.fire("click");
    check("点图打开的是预览（不切换选中）",
      elements["lightbox"].hidden === false
        && !calls.slice(callsBefore).some((c) => c.command === "toggle_staged"));
    elements["lightbox-close"].fire("click");
    const beforeDel = calls.length;
    stagedDel.fire("click");
    await settle(2);
    const afterDel = calls.slice(beforeDel);
    check("点 ✕ 调的是 remove_staged", afterDel.some((c) => c.command === "remove_staged" && c.args.index === 0));
    check("✕ 的点击不会同时被当成选中/取消",
      !afterDel.some((c) => c.command === "toggle_staged"));
    check("删完重画：暂存区空了", filmItems().length === 2, `实际 ${filmItems().length}`);

    // 删已发出的那张：走 remove_turn_image，参数是"哪一轮的第几张"
    const beforeSent = calls.length;
    filmDeletes()[1].fire("click");
    await settle(2);
    check("点暗着那张的 ✕ 调的是 remove_turn_image",
      calls.slice(beforeSent).some((c) => c.command === "remove_turn_image" && c.args.turn === 0 && c.args.image === 1),
      JSON.stringify(calls.slice(beforeSent).map((c) => [c.command, c.args])));
  }

  console.log("\n[12n] 按钮样式统一 + 接着上文的位置（静态盯 panel.html / style.css）");
  {
    const html = fs.readFileSync(SRC + "panel.html", "utf8");
    const css = fs.readFileSync(SRC + "style.css", "utf8");
    const js = fs.readFileSync(SRC + "panel.js", "utf8");

    check("状态行（#shot-meta）已经从 HTML 和 JS 里彻底删掉",
      !html.includes("shot-meta") && !js.includes("shot-meta"));

    // 接着上文要在 composer 里，且在提问按钮**前面**（视觉上就是"上方"）
    const composerAt = html.indexOf('id="composer"');
    const continueAt = html.indexOf('id="btn-continue"');
    const sendAt = html.indexOf('id="btn-send"');
    check("接着上文挪到了输入区里、提问按钮上方",
      composerAt >= 0 && continueAt > composerAt && continueAt < sendAt,
      `composer=${composerAt} continue=${continueAt} send=${sendAt}`);
    const strip = html.slice(html.indexOf('id="shot-strip"'), html.indexOf('id="thread"'));
    check("胶片条那一行只剩 + / 胶片 / 预览 / 提示",
      strip.length > 0 && !strip.includes("btn-continue") && !strip.includes("shot-meta"), strip.slice(0, 80));

    // 统一：这几个选择器不许再各写各的尺寸（尺寸归 button 基样式）
    const blocks = [
      /\.turn-actions button\s*\{[^}]*\}/,
      /\.turn \.q \.q-edit-buttons button\s*\{[^}]*\}/,
      /#presets button\s*\{[^}]*\}/,
      /#btn-continue\s*\{[^}]*\}/,
      /\.side-head button\s*\{[^}]*\}/,
    ];
    const offenders = blocks
      .map((re) => re.exec(css)?.[0] || "")
      .filter((block) => /font-size|padding\s*:/.test(block));
    check("没有哪处按钮还自己写 font-size/padding", offenders.length === 0, offenders.join(" | "));

    const base = /^button\s*\{[^}]*\}/m.exec(css)?.[0] || "";
    check("基样式管住尺寸（font-size + padding + 圆角）",
      /font-size:/.test(base) && /padding:/.test(base) && /border-radius:\s*8px/.test(base), base.slice(0, 60));
    check("开关的「开着」状态是共用的 button.on", /^button\.on\s*[,{]/m.test(css));
    // 后端那颗两种文案长度不同，必须有固定宽度，否则切换时顶栏那一排会跟着挪
    const backend = /#btn-backend\s*\{[^}]*\}/.exec(css)?.[0] || "";
    check("后端按钮有固定最小宽度（切换文案时不挤动顶栏）", /min-width:\s*\d+px/.test(backend), backend);
  }

  console.log("\n[12m] 顶栏高度不许被截图数量撑开（布局规矩，用 CSS 静态盯）");
  {
    // 真事：胶片条变宽之后缩不下去（min-width 默认是内容宽度），把旁边的东西挤变形，
    // 截得越多顶栏越高。（当时被挤的是那句提示，现在提示删了，规矩照旧：顶栏不许长高。）
    const css = fs.readFileSync(SRC + "style.css", "utf8");
    const html = fs.readFileSync(SRC + "panel.html", "utf8");
    const strip = /#shot-strip\s*\{[^}]*\}/.exec(css)?.[0] || "";
    const filmCss = /#film\s*\{[^}]*\}/.exec(css)?.[0] || "";
    const hintCss = /#shot-hint\s*\{[^}]*\}/.exec(css)?.[0] || "";
    check("顶栏是固定高度（里面有东西也不许长高）",
      /height:\s*\d+px/.test(strip) && /box-sizing:\s*border-box/.test(strip), strip.slice(0, 80));
    check("胶片条缩得下去，给提示和按钮留地方", /min-width:\s*0/.test(filmCss), filmCss.slice(0, 80));
    check("胶片条铺满整条宽度（flex: 1），自己横向滚",
      /flex:\s*1/.test(filmCss) && /overflow-x:\s*auto/.test(filmCss) && !/max-width:/.test(filmCss),
      filmCss.slice(0, 60));
    // 滚动条得看得见（藏起来时，图被切在边上会像显示坏了），但**不能压在图上**：
    // 图高 + 滚动条高 必须塞得进顶栏的净高，否则图底边会被 overflow:hidden 切掉。
    const bar = /#film::-webkit-scrollbar\s*\{([^}]*)\}/.exec(css)?.[1] || "";
    const barHeight = Number(/height:\s*(\d+)px/.exec(bar)?.[1] || 0);
    const itemHeight = Number(/max-height:\s*(\d+)px/.exec(/\.film-item\s*\{[^}]*\}/.exec(css)?.[0] || "")?.[1] || 0);
    const stripHeight = Number(/height:\s*(\d+)px/.exec(strip)?.[1] || 0);
    const padY = Number(/padding:\s*(\d+)px/.exec(strip)?.[1] || 0);
    check("滚动条看得见（有高度）", barHeight > 0 && barHeight <= 6, `${barHeight}px`);
    check("缩略图 + 滚动条塞得进顶栏净高（不会被切底边）",
      itemHeight > 0 && stripHeight > 0 && itemHeight + barHeight <= stripHeight - padY * 2,
      `${itemHeight} + ${barHeight} vs ${stripHeight} - ${padY * 2}`);
    check("那句提示已经删干净（HTML/CSS/JS 里都不许再出现）",
      !hintCss && !css.includes("shot-hint") && !html.includes("shot-hint"));
  }

  console.log("\n[12j] 一张都没选时，第一问拦下来");
  const noSelConv = conversation("1000-0");
  noSelConv.turns = [];
  noSelConv.staged = [
    { dataUrl: "data:image/jpeg;base64,S1", selected: false, cropW: 10, cropH: 10, sentW: 10, sentH: 10 },
  ];
  current = noSelConv;
  await emit("conversation-changed");
  await settle();
  const asksBefore12j = calls.filter((c) => c.command === "ask").length;
  elements["prompt"].value = "没选图的提问";
  elements["prompt"].fire("keydown", { key: "Enter" });
  await settle(3);
  check("没选中任何暂存图时拦住不发", calls.filter((c) => c.command === "ask").length === asksBefore12j);
  check("问题还留在输入框里", elements["prompt"].value === "没选图的提问");

  console.log("\n[13] 接着上文：新截的图接到这段后面");
  current = conversation("1000-0");
  await emit("conversation-changed");
  await settle();
  check("默认是关的（另起一段）", elements["btn-continue"].classList.contains("on") === false);
  check("文案说明了「现在是关的」", elements["btn-continue"].textContent === "○ 接着上文", elements["btn-continue"].textContent);
  const continueLabel = elements["btn-continue"].textContent;

  // 用"这次点了几笔"来判断，而不是绝对条数：前面别的开关也会落盘
  const savesBeforeContinue = saves.length;
  elements["btn-continue"].fire("click");
  await settle(3);
  check("点一下打开了", elements["btn-continue"].classList.contains("on") === true);
  const firstContinue = saves[savesBeforeContinue];
  check("写进了配置并且立刻落盘", saves.length === savesBeforeContinue + 1 && firstContinue.continue_conversation === true, JSON.stringify(firstContinue && firstContinue.continue_conversation));
  check("配置里别的字段没被弄丢", firstContinue.hotkey === "Ctrl+Shift+A" && firstContinue.profiles.length === 1 && firstContinue.active_profile === "配置1");
  check("开着时换成实心点", elements["btn-continue"].textContent === "● 接着上文", elements["btn-continue"].textContent);
  // 真事：以前开着是"接着上文 ✓"，多一个字符就把这一列撑宽、输入框跟着缩，
  // 点一下按钮整条都在跳。这里钉住"两种状态长度必须一样"。
  check("开和关的文案一样长（按钮不会因此变大变小）",
    elements["btn-continue"].textContent.length === continueLabel.length,
    `${continueLabel.length} -> ${elements["btn-continue"].textContent.length}`);

  elements["btn-continue"].fire("click");
  await settle(3);
  check("再点一下关回去", saves.length === savesBeforeContinue + 2 && saves[savesBeforeContinue + 1].continue_conversation === false, JSON.stringify(saves[savesBeforeContinue + 1] && saves[savesBeforeContinue + 1].continue_conversation));
  check("关回去又变回空心点", elements["btn-continue"].textContent === "○ 接着上文", elements["btn-continue"].textContent);

  console.log("\n[13b] 一段对话里好几张图时，界面上要看得出来");
  const multiShot = conversation("1000-0");
  multiShot.turns = [
    { question: "第一段", answer: "第一答", imageCount: 1 },
    { question: "追问", answer: "追问的答", imageCount: 0 },
    { question: "第二段", answer: "第二答", imageCount: 1 },
  ];
  multiShot.cropW = 800;
  multiShot.sentW = 700;
  current = multiShot;
  await emit("conversation-changed");
  await settle();

  const badges = elements["thread"].find("from-shot");
  check("带新截图的那两轮才有标记", badges.length === 2, `实际 ${badges.length} 个`);
  check("纯追问那轮没有标记", elements["thread"].textContent.includes("追问的答"));
  check("标记上写了'新截图'", badges[0] && badges[0].textContent === "新截图", badges[0] && badges[0].textContent);

  console.log("\n[13c] 单张图的对话不该多出别的花样");
  current = conversation("1000-0");
  await emit("conversation-changed");
  await settle();
  check("有图时预览是显示的", elements["thumb"].hidden === false && elements["thumb"].getAttribute("src").startsWith("data:image"), elements["thumb"].getAttribute("src").slice(0, 24));

  console.log("\n[13d] 没有截图时不该留一个破图占位");

  // 还没框选过：预览必须整个收起来，只留一句说明
  current = null;
  await emit("conversation-changed");
  await settle();
  check("空状态下 img 没有 src", elements["thumb"].getAttribute("src") === undefined, String(elements["thumb"].getAttribute("src")));
  check("空状态下 img 被 hidden 收起来", elements["thumb"].hidden === true);
  check("空状态在对话区里说明了原因", elements["thread"].textContent.includes("还没有截图"),
    elements["thread"].textContent.slice(0, 60));

  // 记录里真的一张图都没有（正常不会有）时也不能画破图
  const noImage = conversation("1000-0");
  noImage.dataUrl = "";
  current = noImage;
  await emit("conversation-changed");
  await settle();
  check("没有图片数据时同样收起来", elements["thumb"].hidden === true && elements["thumb"].getAttribute("src") === undefined);
  check("没图的记录照常渲染，不摆破图也不报错",
    elements["thread"].textContent.includes("第一段的回答"));

  // 再切回有图的记录，预览要恢复
  current = conversation("1000-0");
  await emit("conversation-changed");
  await settle();
  check("切回有图的记录预览恢复显示", elements["thumb"].hidden === false && elements["thumb"].getAttribute("src").startsWith("data:image"));

  // CSS 那条兜底规则：JS 跑起来之前（比如窗口刚开）也不能出现破图
  const css = fs.readFileSync(path.join(__dirname, "..", "src", "style.css"), "utf8");
  check(
    "CSS 里有无 src 就隐藏的兜底规则",
    /#thumb:not\(\[src\]\)/.test(css),
    "缺少 #thumb:not([src]) 规则，窗口刚打开的那一帧还会闪一个破图"
  );

  console.log("\n[13e] 表格和公式要渲染出来（走网页版时回答里常有）");
  const render = (md) => vm.runInContext(`window.AskWithSnip.renderMarkdown(${JSON.stringify(md)})`, sandbox);

  const table = render("| 名称 | 许可 |\n| --- | --- |\n| Snipaste | 闭源 |\n| Flameshot | 开源 |");
  check(
    "竖线表变成真正的 table",
    table.includes("<table>") && table.includes("<th>名称</th>") && table.includes("<td>闭源</td>"),
    table
  );
  check("表格没被当成段落" , !table.includes("<p>| 名称"), table);

  const notTable = render("| 只有一行竖线 |\n| 没有分隔行 |");
  check("缺分隔行就不当表格", !notTable.includes("<table>"), notTable);

  const math = render("面积是 $S = \\pi r^2$ 这么大");
  check("行内公式被认出来", math.includes('class="math"') && math.includes("\\pi r^2"), math);

  const stars = render("公式里的 $a*b*c$ 不该变成强调");
  check("公式里的星号没被吃成强调", !stars.includes("<em>"), stars);

  const blockMath = render("$$\n\\int_0^1 x\\,dx\n$$");
  check("行间公式独立成块", blockMath.includes("math-block"), blockMath);

  const codeWithDollar = render("```js\nconst a = $1;\n```");
  check("代码块里的 $ 不当公式", codeWithDollar.includes("<pre>") && codeWithDollar.includes("$1"), codeWithDollar);

  console.log("\n[14] 全程没有未捕获的异常");
  check("unhandledRejection 为空", unhandled.length === 0, JSON.stringify(unhandled));

  console.log(`\n结果：${failures === 0 ? "全部通过" : failures + " 项失败"}`);
  process.exit(failures === 0 ? 0 : 1);
})();
