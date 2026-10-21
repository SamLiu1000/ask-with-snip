/*
 * 网页版读回答那套逻辑的验证：把 `src-tauri/src/webchat.rs` 里的 COLLECT_JS **原文**
 * 抽出来，喂给一个替身页面跑，看它推出来的东西能不能被对面拼回页面上那段正文。
 *
 * 为什么专门测这一段：面板里"回答和网页上不一样"这种毛病全出在这儿。
 * 实测过一次特别难看的——站点在流式过程中把光标那一小段重画了三次（半截的表格行
 * 被补全），而当时的写法要求"新文本一定接着旧文本往后写"，一碰上不成立就把整篇再推
 * 一遍，结果一段 600 字的回答落盘时变成了 157+293+395+602 的四遍重复。
 *
 * 用 `node tools/webchat_collect_test.cjs` 跑（仓库 package.json 是 type: module，
 * 所以这里必须是 .cjs）。不需要浏览器、不需要联网、不碰真页面。
 */
const fs = require("fs");
const path = require("path");

const SRC = path.join(__dirname, "..", "src-tauri", "src", "webchat.rs");

// ------------------------------------------------------------------ 从 Rust 源码里抽 JS

const rust = fs.readFileSync(SRC, "utf8");

function rawStringAfter(anchor) {
  const at = rust.indexOf(anchor);
  if (at < 0) throw new Error(`在 webchat.rs 里找不到 ${anchor}`);
  const start = rust.indexOf('r#"', at);
  if (start < 0) throw new Error(`${anchor} 后面不是 r#"..."# 字面量`);
  const end = rust.indexOf('"#', start + 3);
  if (end < 0) throw new Error(`${anchor} 的字面量没有收尾`);
  return rust.slice(start + 3, end);
}

function constString(name) {
  // 两种写法都要认：普通字符串，以及带 `"` 的那种内容用的 r#"..."#
  const raw = new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`).exec(rust);
  if (raw) return raw[1];
  const plain = new RegExp(`const ${name}: &str = "([^"]*)"`).exec(rust);
  if (plain) return plain[1];
  throw new Error(`在 webchat.rs 里找不到常量 ${name}`);
}

function constNumber(name) {
  const m = new RegExp(`const ${name}: usize = (\\d+)`).exec(rust);
  if (!m) throw new Error(`在 webchat.rs 里找不到常量 ${name}`);
  return m[1];
}

// deepseek 站点的选择器：从 webchat.rs 的 DEEPSEEK 注册表项里抽（测试只测 deepseek 这套）。
// 用纯字符串解析，不碰正则——Rust 源码里的 r#""# 和 Some() 包装不值得为此写正则。
const NL_SEMI = String.fromCharCode(10) + "};";
const DEEPSEEK = (() => {
  const anchor = "const DEEPSEEK: Site = Site {";
  const at = rust.indexOf(anchor);
  if (at < 0) throw new Error("webchat.rs 里找不到 DEEPSEEK 站点注册项");
  const block = rust.slice(at + anchor.length, rust.indexOf(NL_SEMI, at));
  const field = (name) => {
    const key = name + ": ";
    let i = block.indexOf(key);
    if (i < 0) throw new Error("DEEPSEEK 里缺字段 " + name);
    i += key.length;
    if (block.startsWith("Some(", i)) i += "Some(".length;
    if (block.startsWith('r#"', i)) {
      const from = i + 3;
      return block.slice(from, block.indexOf('"#', from));
    }
    return block.slice(i + 1, block.indexOf('"', i + 1));
  };
  return { composer: field("composer"), answer: field("answer"), think: field("think") };
})();
const SEL_COMPOSER = DEEPSEEK.composer;
const SEL_ANSWER = DEEPSEEK.answer;
const SEL_THINK = DEEPSEEK.think;
const CHUNK = constNumber("CHUNK");
const TAG = constString("TAG");

/** 和 Rust 里 `collect_js_with` 干的是同一件事：把占位符替换掉 */
function buildCollectJs({ force = false, have = 0, haveReasoning = 0, baseAnswers = 0, baseThinks = 0 } = {}) {
  const src = rawStringAfter("const COLLECT_JS: &str =");
  // 状态形状是共享的一份（Rust 里叫 STATE_SHAPE_JS），这里同样抽出来填进去
  const stateShape = rawStringAfter("const STATE_SHAPE_JS: &str =");
  // 页面诊断（标题 + 正文开头一截）也是共享的一份（PAGE_DIAG_JS）
  const pageDiag = rawStringAfter("const PAGE_DIAG_JS: &str =");
  return [
    ["__COMPOSER__", SEL_COMPOSER],
    ["__ANSWER__", SEL_ANSWER],
    ["__THINK__", SEL_THINK],
    ["__CHUNK__", CHUNK],
    ["__TAG__", TAG],
    ["__FORCE__", force ? "true" : "false"],
    ["__HAVE__", String(have)],
    ["__HAVER__", String(haveReasoning)],
    ["__BASE_A__", String(baseAnswers)],
    ["__BASE_T__", String(baseThinks)],
    ["__STATE_SHAPE__", stateShape],
    ["__PAGE_DIAG__", pageDiag],
    // The thinking fallback is gated per site in Rust; the test pages run the
    // fallback-enabled variant (deepseek-shaped), which the old cases expect.
    ["__THINK_FALLBACK__", "true"],
  ].reduce((acc, [from, to]) => acc.split(from).join(to), src);
}

const COLLECT = buildCollectJs();
const FLUSH = buildCollectJs({ force: true });

// ------------------------------------------------------------------ 替身页面

/**
 * 一个"页面"：正文和思考块的内容由用例直接摆布（`page.answer = "..."`），
 * 其余全是替身——这些用例关心的是**增量怎么推**，不是 DOM 怎么转 Markdown，
 * 所以 `__snapiMD` 直接返回节点的 `md` 字段。
 */
function makePage() {
  const state = { title: "", answers: [], thinks: [] };
  const document = {
    get title() {
      return state.title;
    },
    set title(value) {
      state.title = String(value);
    },
    querySelector: () => null, // 输入框：这批用例不关心（ready 会一直是 false）
    querySelectorAll: (sel) => (sel.includes("think") ? state.thinks : state.answers),
  };
  const window = { __snapiMD: (node) => node.md, __snapi: null };
  return { window, document, state };
}

const mmd = (text) => ({
  md: text,
  innerText: text,
  textContent: text,
  closest: () => null,
  parentElement: null,
});

/** 对面（Rust + 面板）那一侧：照事件语义拼，追加是追加，rst 是整段替换 */
function makeConsumer() {
  let text = "";
  let snap = null;
  const seen = [];
  return {
    apply(payload) {
      const d = payload.d || "";
      seen.push({ d, rst: !!payload.rst, more: !!payload.more });
      if (payload.rst) snap = "";
      if (snap !== null) {
        snap += d;
        if (!payload.more) {
          text = snap;
          snap = null;
        }
      } else {
        text += d;
      }
    },
    get text() {
      return text;
    },
    get seen() {
      return seen;
    },
    get resent() {
      return seen.filter((s) => s.rst).length;
    },
  };
}

// ------------------------------------------------------------------ 断言小工具

let failed = 0;
function check(name, ok, extra) {
  if (ok) {
    console.log(`  ok   ${name}`);
  } else {
    failed += 1;
    console.log(`  FAIL ${name}${extra === undefined ? "" : `  -> ${extra}`}`);
  }
}
const head = (s, n = 40) => JSON.stringify(s.slice(0, n));

// ------------------------------------------------------------------ 跑一"轮"

/** 跑一次 collect（可以立刻再跑 flush），把每次推出来的载荷按顺序交给 consumer */
function poll(ctx, consumer, js = COLLECT) {
  const fn = new Function("window", "document", js);
  fn(ctx.window, ctx.document);
  const title = ctx.state.title;
  if (!title.startsWith(TAG)) {
    check("页面把结果写进了标题", false, head(title));
    return null;
  }
  const payload = JSON.parse(title.slice(TAG.length));
  if (consumer && (payload.d || payload.dr || payload.rst || payload.rrst)) consumer.apply(payload);
  return payload;
}

// ================================================================== 用例

console.log("[1] 正常流式：只是往后写，应该全是增量");
{
  const ctx = makePage();
  const consumer = makeConsumer();
  const full = "第一段。第二段。第三段。";
  ctx.state.answers = [mmd("第一段。")];
  poll(ctx, consumer);
  ctx.state.answers = [mmd("第一段。第二段。")];
  poll(ctx, consumer);
  ctx.state.answers = [mmd(full)];
  poll(ctx, consumer);
  check("拼回来和页面上一模一样", consumer.text === full, head(consumer.text));
  check("一次整段重发都没有", consumer.resent === 0, `重发了 ${consumer.resent} 次`);
}

console.log("\n[2] 站点重画了半截的表格行（真实故障）");
{
  // 这就是落盘时变成四遍重复的那个场景：光标停在表格行中间时，
  // 行尾会被补成 `| 游戏 |`，下一轮又变成 `| 游戏主程序执行文件 |`——
  // 新文本不再以旧文本开头，旧写法于是把整篇又推了一遍
  const ctx = makePage();
  const consumer = makeConsumer();
  const p1 =
    "从这张截图来看，这极大概率是《侠盗猎车手5》。\n\n| 文件名 | 说明 |\n\n| --- | --- |\n\n| **GTA5.exe** | 游戏 |";
  const p2 =
    "从这张截图来看，这极大概率是《侠盗猎车手5》。\n\n| 文件名 | 说明 |\n\n| --- | --- |\n\n| **GTA5.exe** | 游戏主程序执行文件 |\n\n| **PlayGTAV.exe** | 启动工具 |";
  const full = `${p2}\n\n总之，它没有开源。`;
  ctx.state.answers = [mmd(p1)];
  poll(ctx, consumer);
  ctx.state.answers = [mmd(p2)];
  poll(ctx, consumer);
  ctx.state.answers = [mmd(full)];
  poll(ctx, consumer);
  check("拼回来和页面上一模一样", consumer.text === full, head(consumer.text, 60));
  check("没有出现重复段落", consumer.text.indexOf("从这张截图来看", 1) === -1, "回答里出现了两遍开头");
  check("重画触发了整段重发（而不是把整篇接在后面）", consumer.resent === 1, `重发了 ${consumer.resent} 次`);
  const pushed = consumer.seen.reduce((n, s) => n + s.d.length, 0);
  check("推的字数没有翻倍", pushed < full.length * 2, `推了 ${pushed} 字，最终 ${full.length} 字`);

  // 旧写法（新文本不以旧文本开头就整段追加）在这个场景下会留下重复，
  // 记下来是为了说明这个用例真的抓得住那个 bug
  let oldText = "";
  let last = "";
  for (const text of [p1, p2, full]) {
    if (text.length > last.length && text.startsWith(last)) oldText += text.slice(last.length);
    else if (text !== last) oldText += text;
    last = text;
  }
  check("（对照）旧写法确实会重复", oldText.length > full.length && oldText.indexOf("从这张截图来看", 1) !== -1,
    `${oldText.length} 字 vs 正确的 ${full.length} 字`);
}

console.log("\n[3] 思维链先出来：回答节点还没出现的时候也要读得到");
{
  const ctx = makePage();
  const consumer = makeConsumer();
  ctx.state.thinks = [mmd("先读题…")];
  const first = poll(ctx, consumer);
  check("第一节思考推出来了", first.dr === "先读题…", head(first.dr || ""));
  check("这时候还没有正文", first.answers === 0 && !first.d);
  ctx.state.thinks = [mmd("先读题…再想想条件。")];
  const second = poll(ctx, consumer);
  check("第二节是增量", second.dr === "再想想条件。", head(second.dr || ""));
  ctx.state.answers = [mmd("答案是 4。")];
  const third = poll(ctx, consumer);
  check("正文各自成一条流", third.d === "答案是 4。" && third.dr === "");
  check("思维链没被正文挤掉", consumer.text === "答案是 4。", head(consumer.text));
}

console.log("\n[4] 收尾复读：对面缺了一块就整段补上");
{
  const ctx = makePage();
  const full = "一二三四五六七八九十";
  ctx.state.answers = [mmd(full)];
  // 对面只收到了前半截（模拟标题通道把两次变化合并、丢了一条载荷）
  const consumer = makeConsumer();
  poll(ctx, consumer);
  const short = { text: consumer.text.slice(0, 4) };
  check("先造出一个缺了一块的对面", short.text.length === 4, short.text);

  // 复读时把对面的长度报给页面：对不上就整段重发
  const flushPayload = poll(ctx, consumer, buildCollectJs({ force: true, have: 4 }));
  check("复读把整段重发了一遍", flushPayload.rst === true, JSON.stringify(flushPayload.rst));
  check("重发的就是页面上那段", consumer.text === full, head(consumer.text));

  // 长度对得上就不该再推——否则收尾那个循环永远停不下来
  const quiet = poll(ctx, consumer, buildCollectJs({ force: true, have: full.length }));
  check("长度对上了就什么都不推", !quiet.d && !quiet.rst, JSON.stringify({ d: quiet.d, rst: quiet.rst }));
}

console.log("\n[5] 追问：新回答不该跟旧回答糊在一起");
{
  // 追问时页面没重载，上一轮的回答还挂在页面上，而对面手里是**空的**
  // （每一轮 Rust 都新开一个缓冲），所以这里必须走"整段重发"
  const ctx = makePage();
  const consumer = makeConsumer();
  ctx.state.answers = [mmd("第一问的回答。")];
  poll(ctx, consumer);
  ctx.state.answers = [mmd("第一问的回答。"), mmd("第二问的回答")];
  const p = poll(ctx, consumer, buildCollectJs());
  check("新回答是按整段重发推的", p.rst === true && p.d === "第二问的回答",
    JSON.stringify({ d: p.d, rst: p.rst }));
  check("对面手上只有新回答", consumer.text === "第二问的回答", head(consumer.text));
}

console.log("\n[6] 另起一段对话：状态清空，页面上那段从头推");
{
  const ctx = makePage();
  // 第一轮：页面和对面都有内容
  const first = makeConsumer();
  ctx.state.answers = [mmd("旧对话的回答。")];
  poll(ctx, first);
  check("第一轮推完了", first.text === "旧对话的回答。", head(first.text));

  // 点「开启新对话」：NEW_CHAT_JS 把 window.__snapi 置空；回到首页后页面上什么都没有
  ctx.window.__snapi = null;
  ctx.state.answers = [];
  poll(ctx, first);

  // 第二轮（Rust 那边是新开一个缓冲的）
  const second = makeConsumer();
  ctx.state.answers = [mmd("新对话的回答。")];
  const p = poll(ctx, second);
  check("新对话的正文从头推，不是接着上一轮的尾巴写", p.d === "新对话的回答。" && !p.rst,
    JSON.stringify({ d: p.d, rst: p.rst }));
  check("新那一轮里没有旧对话的内容", second.text === "新对话的回答。", head(second.text));
}

console.log("\n[6b] 状态是旧形状（compose 留下的）时，collect 要能自愈");
{
  // 真事：compose 建状态用旧形状（last/pending/rlast/rpending），collect 被重写成新形状（a/r）后，
  // 每次 collect 都在 feed(st.a, …) 上抛 TypeError——一条回报都发不出来，
  // 网页把回答生成完了面板也一个字收不到。collect 现在认得旧形状并重建。
  const ctx = makePage();
  ctx.window.__snapi = { nodes: 0, last: "", pending: "", rlast: "", rpending: "", i: 0 };
  ctx.state.answers = [mmd("新回答。")];
  let threw = null;
  let payload = null;
  try {
    payload = poll(ctx, null);
  } catch (err) {
    threw = err;
  }
  check("collect 没有崩", threw === null, threw && threw.message);
  check("旧形状被重建，正文照常推出来", payload && payload.d === "新回答。", payload && JSON.stringify(payload.d));
  check("重建时状态是新形状", ctx.window.__snapi.a && ctx.window.__snapi.a.read === "新回答。",
    JSON.stringify(ctx.window.__snapi && Object.keys(ctx.window.__snapi)));
}

console.log("\n[7] 长回答分块推，块序不能乱");
{
  const ctx = makePage();
  const consumer = makeConsumer();
  const full = "字".repeat(Math.floor(Number(CHUNK) * 2.5));
  ctx.state.answers = [mmd(full)];
  let rounds = 0;
  let payload = poll(ctx, consumer);
  while (payload.more && rounds < 20) {
    payload = poll(ctx, consumer);
    rounds += 1;
  }
  check("分成了好几块", rounds >= 2, `推了 ${rounds + 1} 轮`);
  check("跨块拼回来还是完整的一段", consumer.text === full, `${consumer.text.length} vs ${full.length}`);
}

console.log("\n[8] 复读的基线守卫：页面上没有新的回答节点时，不许把上一轮那条顶上来");
{
  // 场景：追问（或者上一轮读崩了之后）让页面整段重发，可页面上还是**上一轮**那条回答。
  // 这时候把它的正文当成这一轮推过去，就是"新问题配旧答案"——错得比读不到还难发现。
  const page = makePage();
  const consumer = makeConsumer();
  page.state.answers = [mmd("上一轮的回答")];
  poll(page, consumer);
  check("先把上一轮那条正常读出来", consumer.text === "上一轮的回答", head(consumer.text));

  // compose 会在打字那一刻把基线记在页面状态里：几条回答、最后一条多长。
  // 这里照它记（守卫认的就是这份，见 STATE_SHAPE_JS 的 baseA/baseLen）。
  const markBaseline = (count, lastLen) => {
    page.window.__snapi.baseA = count;
    page.window.__snapi.baseLen = lastLen;
  };

  markBaseline(1, "上一轮的回答".length);
  const guarded = buildCollectJs({ force: true, have: 0 });
  poll(page, consumer, guarded);
  check("有基线挡着：一个字都不重发", consumer.resent === 0, `重发了 ${consumer.resent} 次`);

  markBaseline(0, 0);
  const unguarded = buildCollectJs({ force: true, have: 0 });
  poll(page, consumer, unguarded);
  check("基线是 0（没读到，新会话那种）时不设限，照旧整段重发", consumer.resent === 1, `重发了 ${consumer.resent} 次`);

  // 页面上多出新的回答节点：这时候重发的是**这一轮**的
  markBaseline(1, "上一轮的回答".length);
  page.state.answers = [mmd("上一轮的回答"), mmd("这一轮的新回答")];
  const fresh = buildCollectJs({ force: true, have: 0 });
  const payload = poll(page, consumer, fresh);
  check("新的回答节点出现后，基线放行", payload.rst === true && consumer.resent === 2, JSON.stringify(payload).slice(0, 60));

  // 条数没长（甚至变少了）、但最后一条的内容变了：也要放行。
  // 真事：站点换页时基线记的是旧页面的条数，新会话的回答排进更小的序号，
  // 光比条数会把新回答永久挡在门外——页面报"回答 2 条、正文 0 字"卡到超时。
  markBaseline(2, "旧页面最后一条".length);
  page.state.answers = [mmd("新会话里的新回答")];
  const swapped = buildCollectJs({ force: true, have: 0 });
  const payload2 = poll(page, consumer, swapped);
  check("条数没长但内容变了，基线放行", payload2.rst === true && consumer.resent === 3, JSON.stringify(payload2).slice(0, 60));
}

console.log("\n[9] 采集脚本自己抛错时，也得推一条载荷出去说明白");
{
  // 真出过这个：转换器在某段内容上抛异常，整段 collect 每次都死在读正文之前，
  // 标题通道一条载荷都发不出去，面板干等两分钟，超时消息里只有"收到 0 条回报"。
  const page = makePage();
  page.state.answers = [mmd("页面上的正文")];
  page.window.__snapiMD = () => {
    throw new Error("转换炸了");
  };
  const payload = poll(page, null);
  check("没把异常抛出去（否则这条载荷就没了）", payload !== null, "页面没写标题");
  check("载荷里带上了原因", payload && payload.err === "转换炸了", payload && JSON.stringify(payload));
  check("顺手把页面上的实际长度报回来", payload && payload.chars === "页面上的正文".length, payload && String(payload.chars));
  check("回答节点数也报了", payload && payload.answers === 1, payload && String(payload.answers));
}

console.log(`\n结果：${failed === 0 ? "全部通过" : `${failed} 项失败`}`);
process.exit(failed === 0 ? 0 : 1);
