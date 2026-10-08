/* ask-with-snip frontend common code: Tauri invoke wrappers, a lightweight
   Markdown renderer, and the toast. Deliberately dependency-free — every page
   is a plain <script> with no bundler. */
(function () {
  const core = window.__TAURI__ && window.__TAURI__.core;
  const event = window.__TAURI__ && window.__TAURI__.event;

  /** Invoke a Rust command. Throws a readable string on failure (Rust always returns Result<_, String>). */
  async function call(command, args) {
    try {
      return await core.invoke(command, args);
    } catch (err) {
      // Rust returns errors as strings; stringify anything else to avoid "[object Object]"
      throw typeof err === "string" ? err : err && err.message ? err.message : String(err);
    }
  }

  function listen(name, handler) {
    return event.listen(name, handler);
  }

  function escapeHtml(input) {
    return String(input == null ? "" : input).replace(/[&<>"']/g, (c) => ({
      "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
    }[c]));
  }

  function renderCodeBlock(block) {
    if (!block) return "";
    const lang = block.lang ? ` class="lang-${escapeHtml(block.lang)}"` : "";
    return `<pre><code${lang}>${escapeHtml(block.code.replace(/\n$/, ""))}</code></pre>`;
  }

  /** Math is not typeset (no KaTeX here); show the raw LaTeX as-is so it at least stays readable. */
  function renderMath(tex, display) {
    return `<span class="math${display ? " math-block" : ""}">${escapeHtml(String(tex).trim())}</span>`;
  }

  function renderTable(rows) {
    const cells = (tag, row) => row.map((c) => `<${tag}>${c}</${tag}>`).join("");
    const head = `<tr>${cells("th", rows[0])}</tr>`;
    const body = rows.slice(1).map((row) => `<tr>${cells("td", row)}</tr>`).join("");
    return `<table><thead>${head}</thead><tbody>${body}</tbody></table>`;
  }

  /** GFM pipe table: `| a | b |` followed by a `| --- | --- |` line */
  function isTableSeparator(line) {
    return /^\|[\s:|-]+\|$/.test(line.trim()) && line.includes("-");
  }

  function splitTableRow(line) {
    const trimmed = line.trim().replace(/^\|/, "").replace(/\|$/, "");
    return trimmed.split("|").map((cell) => cell.trim());
  }

  /**
   * A good-enough Markdown renderer.
   *
   * Order matters: stash code blocks and math as placeholders first, then HTML-escape;
   * otherwise `<div>`, `*`, and `_` inside them would be eaten as tags or emphasis.
   * Placeholders are restored to real HTML at the end. The model's stream is often
   * mid-fence, so unclosed fences are handled too.
   */
  function renderMarkdown(source) {
    const blocks = [];
    let text = String(source == null ? "" : source).replace(/\r\n/g, "\n");

    const stash = (html) => {
      blocks.push(html);
      return `\u0000${blocks.length - 1}\u0000`;
    };
    const stashCode = (lang, code) => stash(renderCodeBlock({ lang, code }));

    // Code blocks come first: stash them as placeholders so $ and * inside are never touched by later rules
    text = text.replace(/```([^\n`]*)\n([\s\S]*?)```/g, (_, lang, code) => stashCode(lang, code));
    text = text.replace(/```([^\n`]*)\n([\s\S]*)$/, (_, lang, code) => stashCode(lang, code));
    // Display math
    text = text.replace(/\$\$([\s\S]+?)\$\$/g, (_, tex) => stash(renderMath(tex, true)));

    text = escapeHtml(text);

    // Inline rules. Inline math, like inline code, must run before emphasis:
    // the * in x^2 * y inside a formula must not become <em>.
    text = text.replace(/\$([^$\n]+?)\$/g, (_, tex) => stash(renderMath(tex, false)));
    text = text.replace(/`([^`\n]+)`/g, (_, code) => `<code>${code}</code>`);
    text = text.replace(
      /\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g,
      (_, label, url) => `<a href="${url}" data-external="1">${label}</a>`
    );
    text = text.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
    text = text.replace(/(^|[\s(（])\*([^*\n]+)\*/g, "$1<em>$2</em>");

    // Block rules: sweep line by line, folding lists, tables, and paragraphs
    const out = [];
    let paragraph = [];
    let list = null;
    const rows = text.split("\n");

    const flushParagraph = () => {
      if (paragraph.length) {
        out.push(`<p>${paragraph.join("<br>")}</p>`);
        paragraph = [];
      }
    };
    const closeList = () => {
      if (list) {
        out.push(`</${list}>`);
        list = null;
      }
    };

    for (let i = 0; i < rows.length; i += 1) {
      const line = rows[i].trim();

      // A line that is exactly one placeholder (code block / math) becomes its own block
      if (/^\u0000\d+\u0000$/.test(line)) {
        flushParagraph();
        closeList();
        out.push(line);
        continue;
      }

      // Table: a `| a | b |` line followed by a `| --- | --- |` line
      if (line.startsWith("|") && i + 1 < rows.length && isTableSeparator(rows[i + 1])) {
        flushParagraph();
        closeList();
        const table = [splitTableRow(line)];
        i += 2; // skip the header and separator rows
        while (i < rows.length && rows[i].trim().startsWith("|")) {
          table.push(splitTableRow(rows[i]));
          i += 1;
        }
        i -= 1; // the loop above advanced one row too far; give it back
        out.push(renderTable(table));
        continue;
      }

      if (!line) {
        flushParagraph();
        closeList();
        continue;
      }

      let match;
      if ((match = /^(#{1,6})\s+(.*)$/.exec(line))) {
        flushParagraph();
        closeList();
        const level = match[1].length;
        out.push(`<h${level}>${match[2]}</h${level}>`);
        continue;
      }
      if (/^(-{3,}|\*{3,}|_{3,})$/.test(line)) {
        flushParagraph();
        closeList();
        out.push("<hr>");
        continue;
      }
      if ((match = /^&gt;\s?(.*)$/.exec(line))) {
        flushParagraph();
        closeList();
        out.push(`<blockquote>${match[1]}</blockquote>`);
        continue;
      }
      if ((match = /^[-*+]\s+(.*)$/.exec(line))) {
        flushParagraph();
        if (list !== "ul") {
          closeList();
          out.push("<ul>");
          list = "ul";
        }
        out.push(`<li>${match[1]}</li>`);
        continue;
      }
      if ((match = /^\d+[.)]\s+(.*)$/.exec(line))) {
        flushParagraph();
        if (list !== "ol") {
          closeList();
          out.push("<ol>");
          list = "ol";
        }
        out.push(`<li>${match[1]}</li>`);
        continue;
      }

      if (list) closeList();
      paragraph.push(line);
    }
    flushParagraph();
    closeList();

    return out
      .join("\n")
      .replace(/\u0000(\d+)\u0000/g, (_, index) => blocks[Number(index)]);
  }

  /** Throttle repaints to one per frame while streaming, instead of reflowing per delta. */
  function createStreamRenderer(element) {
    let text = "";
    let scheduled = false;
    const paint = () => {
      scheduled = false;
      element.innerHTML = renderMarkdown(text);
      element.scrollIntoView({ block: "end" });
    };
    return {
      push(delta) {
        text += delta;
        if (!scheduled) {
          scheduled = true;
          requestAnimationFrame(paint);
        }
      },
      /** Discard what has streamed so far and replace it wholesale (when the site repaints its output) */
      replace(full) {
        text = String(full ?? "");
        if (!scheduled) {
          scheduled = true;
          requestAnimationFrame(paint);
        }
      },
      finish(extra) {
        if (extra != null) text = extra;
        if (scheduled) return;
        paint();
      },
      get text() {
        return text;
      },
    };
  }

  /** Open external links from AI replies in the system browser instead of navigating in-app. */
  function handleExternalLinks(root) {
    root.addEventListener("click", (e) => {
      const anchor = e.target.closest("a[data-external]");
      if (!anchor) return;
      e.preventDefault();
      const url = anchor.getAttribute("href") || "";
      call("open_url", { url }).catch((err) => toast(err, "err"));
    });
  }

  let toastTimer = null;
  function toast(message, kind) {
    let el = document.getElementById("toast");
    if (!el) {
      el = document.createElement("div");
      el.id = "toast";
      document.body.appendChild(el);
    }
    el.textContent = String(message);
    el.className = "show " + (kind || "");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      el.className = "";
    }, kind === "err" ? 5200 : 2600);
  }

  // ---------------------------------------------------------- i18n (zh / en)
  //
  // Keys are the Chinese source strings: code keeps writing Chinese, and t("复制")
  // swaps in English when in English mode; unknown keys are returned as-is
  // (better a missed translation than a raw key name). Static copy is marked in
  // HTML with data-i18n (text) / data-i18n-ph (placeholder) / data-i18n-title
  // (title) and applied by applyI18n(); dynamic strings (toasts, status lines)
  // are wrapped in t() directly.
  const I18N = {
    en: {
      // Panel
      "复制": "Copy",
      "复制整段对话": "Copy the whole conversation",
      "接口": "API",
      "切换提问走哪条路": "Switch how questions are sent",
      "显示网页": "Show page",
      "把网页版窗口调出来：登录、或者看那边出了什么事": "Bring up the web window: sign in, or see what happened there",
      "重选": "Re-snip",
      "重新框选": "Start a new snip",
      "设置": "Settings",
      "收起到托盘": "Minimize to tray",
      "对话记录": "History",
      "清空": "Clear",
      "＋ 新建": "＋ New",
      "删除全部记录": "Delete all history",
      "再截一张加进来；可以连截多张，一次发给 AI": "Add another snip; take several and send them together",
      "想问什么？回车发送，Shift+回车换行": "Ask anything… Enter to send, Shift+Enter for a new line",
      "接着上文": "Continue",
      "决定下次框选是另起一段对话，还是接在当前这段后面": "Decide whether the next snip starts a new conversation or continues this one",
      "提问": "Ask",
      // Panel dynamic strings
      "已排队：等这轮答完就发": "Queued: will send when this turn finishes",
      "已输出 {n} 字": "{n} chars out",
      "字/秒": " chars/s",
      "完成 · {n} 字 · {s} 秒": "Done · {n} chars · {s}s",
      "已取消": "Cancelled",
      "（已取消）": "(cancelled)",
      "先框选一个区域再提问": "Snip a region first",
      "先点选至少一张截图（高亮的才会发送）": "Select at least one snip (highlighted ones get sent)",
      // Drop / paste files
      "图片读不出来": "Can't decode this image",
      "（文件太长，已截断）": "(file too long, truncated)",
      "暂不支持这种文件：{n}": "Unsupported file type: {n}",
      "已添加 {n} 个文件": "Added {n} file(s)",
      "松开，添加文件或图片": "Release to add files or images",
      "添加本地图片（也可以直接把图片拖进来，或截图后 Ctrl+V 粘贴）": "Add local images (drop them onto the panel, or paste with Ctrl+V)",
      "新加的图片{t}：点一下移除": "Added image{t}: click to remove",
      // Settings page
      "ask-with-snip 设置": "ask-with-snip Settings",
      "接口配置": "API profiles",
      "模型接口": "Model API",
      "使用方式": "Usage",
      "图片压缩（省 token 用）": "Image compression (saves tokens)",
      "全局热键": "Global hotkey",
      "请求超时（秒）": "Request timeout (seconds)",
      "网页版代理（可选）": "Web proxy (optional)",
      "系统提示词": "System prompt",
      "长边上限（像素）": "Max long edge (px)",
      "JPEG 质量": "JPEG quality",
      "接口地址 base url": "Base URL",
      "API Key": "API Key",
      "模型名称": "Model name",
      "额外请求头（可选，JSON 对象）": "Extra headers (optional, JSON object)",
      "接口代理（可选）": "API proxy (optional)",
      "更多操作": "More actions",
      "常见服务商（只帮你填地址和模型名，不改变下面的内容）": "Presets (fills URL & model only, never changes fields below)",
      "存为新配置": "Save as profile",
      "重命名": "Rename",
      "测试连接": "Test",
      "保存": "Save",
      "关闭": "Close",
      "立即试一下框选": "Try snipping now",
      "开机自动启动（写注册表 Run 项）": "Start with Windows (writes Run registry key)",
      "框选后接着当前对话继续": "Continue current conversation after snipping",
      "不用记热键，点这里等同于按一次全局热键（不用先保存）": "No need to memorize the hotkey — clicking here equals pressing it (no save needed)",
      // Settings dynamic strings
      "还没填 API Key": "API Key is empty",
      "已保存": "Saved",
      "正在请求…": "Requesting…",
      "连接正常": "Connected",
      "连接失败，详情见下方": "Connection failed, details below",
      // Turn buttons / retry actions / quick-prompt editor
      "重新生成": "Regenerate",
      "这个问题不动，让模型重答一次": "Ask again without changing the question",
      "修改提问": "Edit question",
      "改掉这一轮的问题，然后重问": "Change the question, then ask again",
      "点一下剔除这张，重发时不带；再点一下保留": "Click to drop this snip from the re-ask; click again to keep it",
      "取消": "Cancel",
      "重问": "Re-ask",
      "原样重发这个问题（连截图一起）": "Resend this question as-is (snips included)",
      "改掉这个问题（截图可增删），然后重发": "Change the question (add/drop images), then resend",
      "编辑快捷用语": "Edit quick prompts",
      "新增快捷用语": "Add a quick prompt",
      "点击发送；右键编辑": "Click to send; right-click to edit",
      "名称（按钮上显示的）": "Name (shown on the button)",
      "例如：翻译成中文": "e.g. Translate to Chinese",
      "提示词（实际发送的内容，留空 = 用名称）": "Prompt (what actually gets sent; empty = use the name)",
      "可引用截图让模型干活；留空则把名称当提示词发出去": "Describe what the model should do with the snip; empty sends the name as the prompt",
      "删除": "Delete",
      "快捷用语：每行一条，删行=删按钮": "Quick prompts: one per line; removing a line removes its button",
      // Quick-prompt lists (named groups of quick prompts, switchable from the panel)
      "快捷用语列表（点开切换；也可以新建 / 重命名 / 删除）": "Quick prompt list (click to switch; you can also create, rename or delete)",
      "新建列表…": "New list…",
      "重命名当前列表…": "Rename current list…",
      "删除当前列表…": "Delete current list…",
      "列表名字": "List name",
      "删除列表「{n}」？它里面的快捷用语也会一起删掉。": "Delete list \"{n}\"? Its quick prompts will be removed too.",
      "已新建列表「{n}」": "Created list \"{n}\"",
      "已经叫「{n}」了": "A list named \"{n}\" already exists",
      // Account menu
      "+ 添加账号…": "+ Add account…",
      "- 删除当前账号…": "- Remove current account…",
      "勾上 = 这个账号的网页窗口走设置页里配的代理；不勾 = 直连": "Checked = this account's web window uses the proxy from Settings; unchecked = direct",
      "勾上 = 这个账号在这个站点走代理；不勾 = 直连。保存后窗口自动重建": "Checked = use the proxy for this account on this site; unchecked = direct. The window is rebuilt on save",
      "新增快捷用语": "Add a quick prompt",
      "点一下切换到这个账号": "Click to switch to this account",
      "等这次回答结束再切换": "Wait for the current answer to finish before switching",
      // History / preview / misc
      "删除这条记录": "Delete this entry",
      "记录已清空": "History cleared",
      "还没有可以复制的内容": "Nothing to copy yet",
      "已复制整段对话": "Conversation copied",
      "先框选一个区域，才能提问": "Snip a region first to ask",
      "截图预览加载失败，图片数据可能已损坏": "Snip preview failed to load; image data may be corrupt",
      "正在取消…": "Cancelling…",
      "还没有记录。框选一张图并提问，就会出现在这里。": "No history yet. Snip a region and ask; it will show up here.",
      "思考过程": "Reasoning",
      "已经发出去的图（不能再改选）": "Snips already sent (can't be changed)",
      "已经发出去的图。点一下放大预览": "Snip already sent. Click to preview it full-size",
      "已选中，会跟着下一次提问发出去。点一下放大预览": "Selected — goes out with the next question. Click to preview it full-size",
      "没选中。点一下放大预览，在预览里选中": "Not selected. Click to preview it full-size; select it from the preview",
      "点一下放大预览": "Click to preview it full-size",
      "取消选中": "Deselect",
      "选中发送": "Select to send",
      "问题不能是空的": "The question can't be empty",
      "保存账号失败: ": "Failed to save accounts: ",
      "已添加「{n}」：点「登录」登一次": "Added \"{n}\": click \"Sign in\" to sign in once",
      "已删除「{n}」": "Deleted \"{n}\"",
      // Console window
      "框选屏幕上的任意区域，直接提问": "Snip any region on screen and ask away",
      "开始框选": "Start snipping",
      "也可以随时按 {hk}": "or press {hk} anytime",
      "退出程序": "Quit",
      "这个窗口只是入口，关掉它程序仍在后台运行。": "This window is just an entry point; the app keeps running if you close it.",
      "没看到托盘图标是正常的 —— Windows 11 会把新图标收进溢出区，点任务栏时钟旁边的 ^ 就能找到，左键单击即可框选。": "Can't see the tray icon? On Windows 11 new icons are tucked into the overflow area — click the ^ next to the clock and left-click the icon to snip.",
      "要完全退出，请用托盘图标右键菜单里的「退出」。": "To quit completely, use \"Exit\" in the tray icon's right-click menu.",
      "账号": "Account",
      // Settings dynamic
      "当前热键不可用：": "The current hotkey is unavailable: ",
      "热键没注册上：": "Hotkey registration failed: ",
      "缺模型名": "missing model",
      "（没填地址）": "(no URL)",
      "连接正常，模型回复：": "Connected. Model replied: ",
      "，已读到 {n} 个模型": ", fetched {n} models",
      "当前用「{n}」提问": "Asking with \"{n}\"",
      "「{n}」还缺 API Key 或模型名": "\"{n}\" is missing API Key or model name",
      "还没有配置": "No profile yet",
      "配置{n}": "Profile {n}",
      "已切换到「{n}」": "Switched to \"{n}\"",
      "已存为「{n}」，改完地址和 Key 记得再点保存": "Saved as \"{n}\"; after editing URL and Key, click Save again",
      "已经有叫「{n}」的配置了": "A profile named \"{n}\" already exists",
      "已重命名为「{n}」": "Renamed to \"{n}\"",
      "直接对话（不用截图）": "Ask without a snip",
      // Snip overlay
      "按住左键拖动框选；或者点一下定起点、移动鼠标、再点框内确认 · Esc 取消": "Drag to select; or click, move, click to confirm · Esc to cancel",
      "点一下收起": "Click to dismiss",
      "收起": "Dismiss",
      "ask-with-snip 已启动": "ask-with-snip is running",
      "正在后台待命。按": "Waiting in the background. Press",
      "或点托盘图标开始框选。": "or click the tray icon to snip.",
      "按住修饰键，再按一个键…": "Hold a modifier, then press a key…",
      "移动鼠标定大小，点一下确认（按住拖可以重新框）· Esc 撤销这个框": "Move to size, click to confirm (hold and drag to redraw) · Esc to undo this rect",
      "截图没能完成": "The screenshot didn't complete",
      "按 Esc 关闭这个窗口。": "Press Esc to close this window.",
      "拿到的画面是空的，请重新按热键。": "The captured frame is empty; press the hotkey again.",
      "正在裁剪…": "Cropping…",
      "截图已经收到了，但这个遮罩没能自动关掉，按 Esc 关掉就行。": "The screenshot was captured, but this overlay didn't close by itself - just press Esc.",
      "单键热键会全局拦截这个键：平时打字就打不出它了，建议配合修饰键使用": "A bare-key hotkey is intercepted system-wide — you won't be able to type that character anymore. Prefer a modifier combo",
      // Panel: static copy marked late / dynamic strings
      "登录": "Sign in",
      "选择用哪个网页账号（登录态互相独立）": "Choose which web account to use (login sessions are independent)",
      "当前账号用哪个站点": "Which site the current account uses",
      "弹出当前账号的网页窗口，登录 / 看现场": "Open the current account's web window: sign in / inspect it",
      "对话进度": "Conversation progress",
      "思考中": "Thinking",
      "分析中": "Analyzing",
      "刚刚": "just now",
      "{n} 分钟前": "{n} min ago",
      "轮": "turns",
      "新截图": "New shot",
      "新截图 ×{n}": "New shot ×{n}",
      "第 {n} 轮": "Turn {n}",
      "把这张从这一轮的记录里删掉（之后追问、重新生成都不再带上它）": "Remove this snip from this turn's record (later follow-ups and regenerations won't include it)",
      "已选中，会跟着下一次提问发出去。点一下取消": "Selected; will be sent with the next question. Click to deselect",
      "没选中。点一下选中它": "Not selected. Click to select it",
      "删掉这张（不发给 AI）": "Delete this snip (won't be sent to the AI)",
      "还没有截图。": "No screenshot yet.",
      "可以直接在这里打字提问；要带图的话按 Ctrl+Shift+A，或者点托盘图标框选区域。": "Type a question here directly; to include an image press Ctrl+Shift+A, or click the tray icon to snip an area.",
      "左边的列表是以前的对话，点一下就能打开，也可以接着追问。": "The list on the left holds past conversations; click to open one and keep asking follow-ups.",
      "下次框选会把新截图接到当前这段对话后面（点一下改回另起一段）": "The next snip attaches the new screenshot after this conversation (click to switch back to starting a new one)",
      "当前：每次框选都另起一段对话。点一下改成接在当前这段后面": "Currently every snip starts a new conversation. Click to continue after this one instead",
      "下次框选会接到这一段后面": "The next snip will continue after this conversation",
      "下次框选会另起一段对话": "The next snip will start a new conversation",
      "请求失败": "Request failed",
      "语言：中文": "Language: Chinese",
      "网页版 ✓": "Web ✓",
      "当前走内嵌网页版：用你的网页账号提问，不消耗接口额度。点一下改回接口。": "Using the built-in webchat: questions go through your web account, no API quota used. Click to switch back to API.",
      "当前走接口：消耗 API 额度。点一下改成内嵌网页版（不消耗额度）。": "Using the API: consumes API quota. Click to switch to the built-in webchat (no quota used).",
      "无账号": "no account",
      "「{n}」在 {s} 上走代理（窗口已重建，立即生效）": "\"{n}\" now uses a proxy on {s} (window rebuilt, effective immediately)",
      "「{n}」在 {s} 上改为直连（窗口已重建，立即生效）": "\"{n}\" switched to a direct connection on {s} (window rebuilt, effective immediately)",
      "代理": "proxy",
      "已添加「{n}」（已勾上代理，站点直连不通时必须走）：点「登录」登一次": "Added \"{n}\" (proxy pre-checked; required where a direct connection fails): click \"Sign in\" to sign in once",
      "删除账号「{n}」？它保存的网页登录态也会一并删掉，撤销不了。": "Delete account \"{n}\"? Its saved web login session will be removed too. This cannot be undone.",
      "（该站点已自动勾上代理）": " (proxy auto-checked for this site)",
      "该账号改用 {n}，点「显示网页」登录": "This account now uses {n}; click \"Show page\" to sign in",
      "已切到网页版：用你的网页账号提问，不消耗接口额度。没登录过就先点「显示网页」登录一次": "Switched to webchat: questions use your web account, no API quota. If you haven't signed in, click \"Show page\" once first.",
      "已切回接口": "Switched back to API",
      "问：": "Q: ",
      "答：": "A: ",
      // Settings page: static copy marked late
      "接口按 OpenAI 兼容协议调用，所以只要服务商支持": "Requests use the OpenAI-compatible protocol, so any provider works as long as it supports",
      "且模型能读图，填哪家都可以。": "and the model can read images.",
      "API Key 只保存在你本机的配置文件里，不会上传到任何第三方（除了你自己填的服务商）。": "The API Key is stored only in the local config file on your machine and is never uploaded to any third party (other than the provider you enter yourself).",
      "可以存好几套（比如一套国内直连、一套中转），点一下就切换成它，当前启用的那张会高亮。": "You can store several profiles (e.g. one direct domestic, one via a relay); click one to switch to it — the active one is highlighted.",
      "下面填的地址、Key、模型名都属于": "The URL, Key, and model name below all belong to",
      "当前这张": "the current profile",
      "。": ".",
      "给当前配置改个名字": "Rename the current profile",
      "填到": "Enter up to",
      "这一层就行，程序会自动补上": "— the app appends",
      "只填域名时会自动补": "If only a domain is entered, it appends",
      "；如果服务商的路径不是 v1（比如智谱是": "; if the provider's path isn't v1 (e.g. Zhipu uses",
      "），就按服务商文档填全。": "), enter the full path per the provider's docs.",
      "粘贴你的 Key": "Paste your Key",
      "以明文保存在本机配置文件里（具体位置见页面底部），请自行注意该文件的权限。": "Stored in plain text in the local config file (see the bottom of this page for its location); mind the file's permissions yourself.",
      "必须填服务商当前提供的、支持图片输入的模型名，写错会返回 400 或 404。": "Must be a model name the provider currently offers that supports image input; a wrong name returns 400 or 404.",
      "这套接口的请求走哪个代理（host:port，或 http://、socks5:// 开头）。留空 = 直连。": "Which proxy this API's requests go through (host:port, or prefixed http:// / socks5://). Empty = direct.",
      "属于": "Belongs to",
      "当前这套配置": "the current profile",
      "：中转要代理、国内直连不用，混着存各走各的。网页版的代理在下面「使用方式」里，两边互不影响。": ": relays need a proxy, domestic direct connections don't; profiles are stored mixed, each with its own. The webchat proxy is under \"Usage\" below; the two don't affect each other.",
      "有些中转服务需要额外头时用得上。留空即不发送。": "Useful when a relay service requires extra headers. Empty = not sent.",
      "写法如": "Format like ",
      "、": ", ",
      "；": "; ",
      "点进输入框直接按键盘即可录制": "click the input and press keys to record directly",
      "（Esc 取消）。保存后立即生效。": "(Esc cancels). Takes effect immediately after saving.",
      "网页版走哪家站点都用它（国内直连不上 ChatGPT/Gemini/Claude 时填这个）。留空 = 直连。": "Used by the webchat for every site (fill this in when ChatGPT/Gemini/Claude are unreachable directly). Empty = direct.",
      "每次请求都会带上，用来固定回答的语言和风格。": "Sent with every request to pin down the language and style of the answers.",
      "不勾选（默认）：每次框选都是": "Unchecked (default): every snip starts",
      "勾选：新截的图会": "Checked: new snips get",
      "接到当前这段对话后面": "appended to the current conversation",
      "新的一段对话": "a new conversation",
      "，上下文互不影响，每张图从零开始理解。": "; contexts don't affect each other and each image is understood from scratch.",
      "勾选：新截的图会": "Checked: the new snip is",
      "接到当前这段对话后面": "appended to the current conversation",
      "，前面看过的图仍在上下文里，适合\"截一段问一句、再截一段接着问\"（读长文档、翻长代码）。注意每轮的图都会随请求一起发出去，攒得越长越贵。这条也能在问答窗口里临时切换。": "; previously seen images stay in context — good for \"snip a section, ask, snip more\" (long documents, long code). Note that every image is sent with its request, so the longer it grows the more it costs. This can also be toggled in the ask window.",
      "1568 是大多数视觉模型的最优输入尺寸。再大模型内部也会压，只是白付 token 钱。": "1568 is the optimal input size for most vision models. Anything larger gets compressed internally anyway — you just pay for wasted tokens.",
      "报错信息和代码截图建议保持 85 以上，不然小字会糊。": "For error messages and code screenshots keep it at 85 or above, or small text will blur.",
      "删除「{n}」": "Delete \"{n}\"",
      "自定义（自己填）": "Custom (fill in yourself)",
      "Claude（Anthropic）": "Claude (Anthropic)",
      "GLM（BigModel 智谱开放平台）": "GLM (BigModel, Zhipu open platform)",
      "GLM（Z.ai 海外版）": "GLM (Z.ai international)",
      "Grok（xAI）": "Grok (xAI)",
      "Kimi（月之暗面）": "Kimi (Moonshot AI)",
      "Qwen（通义千问）": "Qwen (Alibaba Cloud)",
      "切换语言 / Switch language": "Switch language",
      "默认": "Default",
      "新列表": "New list",
      "界面主题": "Theme",
      "浅色": "Light",
      "深色": "Dark",
      "保存后所有窗口立即换肤。": "Applies to every window as soon as you save.",
      "刷新网页": "Reload page",
      "让网页版窗口重新加载当前页：白屏、卡在验证页、代理刚改完时按一下": "Reload the current page in the web window: press it when the page is blank, stuck on a captcha, or the proxy just changed",
      "另起一段新对话（没发出去的截图会带过去）": "Start a new conversation (unsent screenshots carry over)",
      "未知错误": "Unknown error",
      "已让网页版重新加载": "Reloaded the web page",
    },
  };
  let lang = "zh";

  /** Look up copy in the current language; keys are Chinese source strings, unknown keys pass through. */
  function t(text) {
    const dict = I18N[lang];
    return (dict && dict[text]) || text;
  }

  /** Switch language and re-render marked static copy on the current page. */
  function setLanguage(next) {
    lang = next === "en" ? "en" : "zh";
    applyI18n();
  }

  function getLanguage() {
    return lang;
  }

  /** Switch the color theme by setting <html data-theme> ("dark" / "light"; anything
     else falls back to the light default). The HTML files pre-set light so the first
     paint is already correct; pages call this once the config has been read. */
  function applyTheme(theme) {
    document.documentElement.dataset.theme = theme === "dark" ? "dark" : "light";
  }

  /** Re-render every element carrying a data-i18n* attribute in the current language. */
  function applyI18n(root) {
    const scope = root || document;
    scope.querySelectorAll("[data-i18n]").forEach((n) => {
      n.textContent = t(n.getAttribute("data-i18n"));
    });
    scope.querySelectorAll("[data-i18n-ph]").forEach((n) => {
      n.setAttribute("placeholder", t(n.getAttribute("data-i18n-ph")));
    });
    scope.querySelectorAll("[data-i18n-title]").forEach((n) => {
      n.setAttribute("title", t(n.getAttribute("data-i18n-title")));
    });
  }

  /**
   * Hotkey recorder: focus the input and press keys directly; records combos like
   * "Ctrl+Shift+A". Escape exits recording without changing the value. Hold
   * modifiers first, press the main key last; F1-F24 may stand alone. The format
   * matches what the hotkey plugin parses (the Ctrl+Shift+A default does too).
   */
  function installHotkeyRecorder(input, onRecorded) {
    const isModifier = (code) => /^(Control|Shift|Alt|Meta|OS)(Left|Right)?$/.test(code);
    // Special keys use keyboard-types Code names (the hotkey plugin parses by Code name)
    const SPECIAL = new Set([
      "Space", "Tab", "Enter", "Minus", "Equal", "Backquote", "Backslash",
      "BracketLeft", "BracketRight", "Semicolon", "Quote", "Comma", "Period",
      "Slash", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight",
      "PageUp", "PageDown", "Home", "End", "Insert", "Delete",
      "Numpad0", "Numpad1", "Numpad2", "Numpad3", "Numpad4", "Numpad5",
      "Numpad6", "Numpad7", "Numpad8", "Numpad9",
    ]);
    let mods = [];

    const combo = (key) => {
      const parts = mods.slice();
      if (key) parts.push(key);
      return parts.join("+");
    };

    input.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        input.blur();
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      if (isModifier(e.code)) {
        const base = e.code.replace(/(Left|Right)$/, "");
        const name = { Control: "Ctrl", Alt: "Alt", Shift: "Shift", Meta: "Win", OS: "Win" }[base];
        if (name && !mods.includes(name)) mods.push(name);
        input.placeholder = combo("") ? combo("") + "+…" : t("按住修饰键，再按一个键…");
        return;
      }
      let key = null;
      if (/^F([1-9]|1[0-9]|2[0-4])$/.test(e.code)) key = e.code;
      else if (/^Key[A-Z]$/.test(e.code)) key = e.code.slice(3);
      else if (/^Digit[0-9]$/.test(e.code)) key = e.code.slice(5);
      else if (SPECIAL.has(e.code)) key = e.code;
      if (!key) return; // unrecognized key: wait for the next one
      // A bare key (like "·" / "`") is allowed: RegisterHotKey accepts it. But it is
      // intercepted system-wide — normal typing can no longer produce that character —
      // so warn instead of silently registering.
      if (mods.length === 0 && !/^F[0-9]+$/.test(key)) {
        toast(t("单键热键会全局拦截这个键：平时打字就打不出它了，建议配合修饰键使用"), "err");
      }
      input.value = combo(key);
      input.placeholder = "";
      mods = [];
      input.blur();
      if (onRecorded) onRecorded(input.value);
    });
    input.addEventListener("keyup", () => {
      mods = [];
    });
    input.addEventListener("blur", () => {
      mods = [];
    });
  }

  /**
   * Report uncaught frontend exceptions to the Rust side, which writes them to stderr.
   *
   * Errors inside the webview are invisible by default (no console), and a single
   * frontend exception can leave a window stuck in a "should show but doesn't" state
   * that is painful to debug. This reporting is the only clue.
   */
  function installErrorReporting(scope) {
    const label = scope || "webview";
    const report = (kind, detail) => {
      try {
        call("report_js_error", { message: `[${label}] ${kind}: ${detail}` }).catch(() => {});
      } catch (_) {
        /* if reporting itself fails, let it go — don't throw again */
      }
    };

    window.addEventListener("error", (e) => {
      report("error", `${e.message} @ ${e.filename || "?"}:${e.lineno || 0}`);
      // Surface it in the UI too: stderr goes nowhere on a release Windows build, so
      // an uncaught exception used to look like "clicked and nothing happened".
      toast(`JS error: ${e.message}`, "err");
    });
    window.addEventListener("unhandledrejection", (e) => {
      const reason = e.reason;
      const detail = reason && reason.message ? reason.message : String(reason);
      report("rejection", detail);
      toast(`JS error: ${detail}`, "err");
    });
  }

  window.AskWithSnip = {
    call,
    listen,
    renderMarkdown,
    createStreamRenderer,
    handleExternalLinks,
    installErrorReporting,
    toast,
    escapeHtml,
    t,
    setLanguage,
    getLanguage,
    applyTheme,
    applyI18n,
    installHotkeyRecorder,
  };
})();
