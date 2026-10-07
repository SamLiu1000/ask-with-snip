// 框选遮罩的逻辑测试：把真的 src/overlay.js 跑在一个极简 DOM 替身上，
// 用"鼠标事件序列 → 提交了什么"来断言。
//
// 为什么单独有一份：遮罩的状态机（点一下、拖一下、Esc）是用户踩过好几次的地方——
// "点一下就被弹回去""没完没了地框选"都出在这里。它没有窗口也能测，别浪费。
//
//   node tools/overlay_logic_test.cjs
const fs = require("fs");
const path = require("path");

const SRC = path.join(__dirname, "..", "src");
const source = fs.readFileSync(path.join(SRC, "overlay.js"), "utf8");

let failed = 0;
function check(name, ok, extra) {
  if (ok) {
    console.log(`  ok   ${name}`);
  } else {
    failed += 1;
    console.log(`  FAIL ${name}${extra === undefined ? "" : `  -> ${extra}`}`);
  }
}

const rejections = [];
process.on("unhandledRejection", (err) => rejections.push(err));

/** 极简元素：只需要 overlay.js 碰到的那些属性和方法 */
class El {
  constructor(id, tag = "div") {
    this.id = id;
    this.tagName = tag.toUpperCase();
    this.style = {};
    this.className = "";
    this.textContent = "";
    this.hidden = false;
    this.src = "";
    this.children = [];
    this._html = "";
  }
  append(...children) {
    this.children.push(...children);
  }
  removeAttribute(name) {
    if (name === "src") this.src = "";
  }
  setAttribute(name, value) {
    if (name === "src") this.src = value;
  }
  getBoundingClientRect() {
    return { left: 0, top: 0, width: W, height: H };
  }
}

const W = 3840;
const H = 2160;
const IDS = ["shot", "shade", "rect", "size", "guide-v", "guide-h", "hint", "screen-error"];

/** 造一个"页面"：元素、事件表、以及记录下来的调用 */
function makePage() {
  const elements = {};
  for (const id of IDS) elements[id] = new El(id, id === "shot" ? "img" : "div");
  const handlers = {};
  const calls = [];

  const document = {
    getElementById: (id) => elements[id] || null,
    createElement: (tag) => new El("", tag),
    addEventListener: (type, fn) => {
      (handlers[type] = handlers[type] || []).push(fn);
    },
  };

  const backend = {
    get_frame: async () => ({ dataUrl: "data:image/jpeg;base64,FRAME", width: W, height: H }),
    overlay_ready: async () => null,
    finish_selection: async (args) => {
      calls.push({ command: "finish_selection", args });
      return null;
    },
    cancel_snip: async () => {
      calls.push({ command: "cancel_snip" });
      return null;
    },
    report_js_error: async (args) => {
      calls.push({ command: "report_js_error", args });
      return null;
    },
    dismiss_overlay: async () => null,
  };

  const window = {
    innerWidth: W,
    innerHeight: H,
    AskWithSnip: {
      call: (command, args) => {
        const fn = backend[command];
        if (!fn) return Promise.reject(new Error(`替身没有实现命令 ${command}`));
        return Promise.resolve(fn(args));
      },
      listen: async () => null,
      installErrorReporting: () => null,
    },
  };

  new Function("window", "document", source)(window, document);

  return {
    elements,
    calls,
    /** 发一个鼠标/键盘事件，把该跑的监听器都跑完 */
    async fire(type, event) {
      const list = handlers[type] || [];
      for (const fn of list) await fn({ button: 0, preventDefault() {}, ...event });
      await settle();
    },
    submits: () => calls.filter((c) => c.command === "finish_selection").map((c) => c.args),
    cancels: () => calls.filter((c) => c.command === "cancel_snip").length,
  };
}

const settle = async (rounds = 6) => {
  for (let index = 0; index < rounds; index += 1) await Promise.resolve();
};

/** 完整的"点一下"：按下 + 松开在同一点 */
async function click(page, x, y) {
  await page.fire("mousedown", { clientX: x, clientY: y });
  await page.fire("mouseup", { clientX: x, clientY: y });
}

async function main() {
  console.log("[1] 点上一下不算提交，但要有进展（进入'点出来的起点'状态）");
  {
    const page = makePage();
    await settle();
    await click(page, 100, 100);
    check("点一下不提交", page.submits().length === 0, JSON.stringify(page.submits()));
    check(
      "提示改成'点一下确认'那一句",
      page.elements["hint"].textContent.includes("点一下确认"),
      page.elements["hint"].textContent
    );
  }

  console.log("\n[2] 不按左键也能截图：点起点 → 移动 → 再点一下");
  {
    const page = makePage();
    await settle();
    await click(page, 100, 100);
    await page.fire("mousemove", { clientX: 400, clientY: 300 });
    check("框跟着鼠标走（尺寸标签在报数）", page.elements["size"].textContent === "300 × 200",
      page.elements["size"].textContent);
    await click(page, 400, 300);
    const submits = page.submits();
    check("第二下点击提交了", submits.length === 1, JSON.stringify(submits));
    check(
      "提交的正是点出来的那个框",
      submits[0] && submits[0].x === 100 && submits[0].y === 100 && submits[0].width === 300 && submits[0].height === 200,
      JSON.stringify(submits[0])
    );
  }

  console.log("\n[3] 老习惯不变：按住拖、松手即成");
  {
    const page = makePage();
    await settle();
    await page.fire("mousedown", { clientX: 50, clientY: 60 });
    await page.fire("mousemove", { clientX: 250, clientY: 360 });
    await page.fire("mouseup", { clientX: 250, clientY: 360 });
    const submits = page.submits();
    check("拖完松手就提交一张", submits.length === 1, JSON.stringify(submits));
    check(
      "框是按下那点到松手那点",
      submits[0] && submits[0].width === 200 && submits[0].height === 300,
      JSON.stringify(submits[0])
    );
  }

  console.log("\n[4] 拖得太小也算'点了那一下'，接着动一动还能成");
  {
    const page = makePage();
    await settle();
    await page.fire("mousedown", { clientX: 10, clientY: 10 });
    await page.fire("mousemove", { clientX: 12, clientY: 10 }); // 2px：在容差里，不算拖
    await page.fire("mouseup", { clientX: 12, clientY: 10 });
    check("太小不提交", page.submits().length === 0, JSON.stringify(page.submits()));
    await page.fire("mousemove", { clientX: 500, clientY: 400 });
    await click(page, 500, 400);
    const submits = page.submits();
    check("接着点一下就成", submits.length === 1, JSON.stringify(submits));
    check(
      "起点还是最开始那一点",
      submits[0] && submits[0].x === 10 && submits[0].y === 10 && submits[0].width === 490,
      JSON.stringify(submits[0])
    );
  }

  console.log("\n[5] 已经有框时按住拖：重新框，而不是被当成'确认'把旧框顶掉");
  {
    const page = makePage();
    await settle();
    await click(page, 100, 100);
    await page.fire("mousemove", { clientX: 400, clientY: 300 });
    await page.fire("mousedown", { clientX: 350, clientY: 250 });
    await page.fire("mousemove", { clientX: 700, clientY: 600 });
    await page.fire("mouseup", { clientX: 700, clientY: 600 });
    const submits = page.submits();
    check("提交的是新框", submits.length === 1 && submits[0].x === 350 && submits[0].width === 350,
      JSON.stringify(submits));
  }

  console.log("\n[6] Esc：先撤掉框，再按一次才关掉整个遮罩");
  {
    const page = makePage();
    await settle();
    await click(page, 80, 80);
    await page.fire("mousemove", { clientX: 300, clientY: 200 });
    await page.fire("keydown", { key: "Escape" });
    check("第一下 Esc 不关遮罩", page.cancels() === 0, `调了 ${page.cancels()} 次 cancel_snip`);
    check(
      "提示回到'怎么开始'那句",
      page.elements["hint"].textContent.includes("再点一下确认"),
      page.elements["hint"].textContent
    );
    await page.fire("keydown", { key: "Escape" });
    check("第二下 Esc 才真的取消", page.cancels() === 1, `调了 ${page.cancels()} 次 cancel_snip`);
  }

  console.log("\n[7] 全程没有未捕获的异常");
  await settle(20);
  check("unhandledRejection 为空", rejections.length === 0, String(rejections[0] || ""));

  console.log(`\n结果：${failed === 0 ? "全部通过" : `${failed} 项失败`}`);
  process.exit(failed === 0 ? 0 : 1);
}

main();
