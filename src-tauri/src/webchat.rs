//! Uses the embedded web UI as a "backend": pastes screenshots, types the
//! question and hits send on the page on the user's behalf, then streams the
//! answer back to the panel. To the user the panel experience is **exactly
//! the same** as going through the API — it just no longer consumes API
//! quota, because it uses the user's own web account.
//!
//! # Why not call the sites' private APIs directly
//!
//! That path requires reverse-engineering three things: the login token,
//! a per-request proof-of-work (`x-ds-pow-response`, a SHA3 wasm blob), and
//! Cloudflare's interstitial. All three break the moment the site changes,
//! and scripting the API is not within the terms of service anyway.
//!
//! What we do here is **the same path a human takes when operating the page**:
//! a real browser, a real session — to the site it is indistinguishable from
//! a person using it. The cost is a dependency on the page's DOM structure;
//! all selectors live in the constants below, so when a site redesigns,
//! only these strings need to change, not the logic.
//!
//! # How data gets out of the page
//!
//! The remote page **deliberately has no Tauri permissions** (see
//! capabilities/default.json), so it cannot call our commands in reverse.
//! The read-back channel that needs no permissions is `document.title`:
//! the page writes its payload into the title and the Rust side receives it
//! via `on_document_title_changed` (Tauri does not wire this event up by
//! default — we register it when creating the window, so the window title
//! itself stays untouched).
//!
//! Two quirks of the title channel were measured the hard way, noted here so
//! nobody trips on them again:
//!   * **Newlines get eaten**, so the payload is a single line;
//!   * **There is a length limit**, so only a small chunk (`CHUNK`) is pushed
//!     each time, the rest on the next round.
//!
//! We do NOT "re-send the whole answer every time": it gets more and more
//! expensive and the title cannot hold it. Instead we push increments, the
//! page keeps a buffer, and the Rust side assembles.
//!
//! Note: strings that exist to *match Chinese page content* (selectors,
//! button-text regexes) stay in Chinese on purpose — see the Site registry.

use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Manager};
use tokio::time::sleep;

use crate::lock;

/// Label of the embedded web UI window (same one as in lib.rs)
pub const WINDOW: &str = "webchat";
/// Prefix of the read-back payload: only titles starting with it are our signal
pub const TAG: &str = "SNAPI|";

/// One independent window per web account (both the label and the WebView2
/// data directory are split per account), so login sessions stay separate —
/// that is what makes "being logged into several sites at once" possible.
/// Non-ASCII characters in the label are folded to '-' to keep window
/// labels / paths out of trouble.
pub fn window_label(account: &str) -> String {
    let clean: String = account
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("webchat-{clean}")
}

/// The "driving parameters" of one site: selectors + page habits. Everything
/// lives in this registry — when a site redesigns, only the strings here
/// change, the logic stays untouched.
///
/// The selectors were probed from the **real pages** (see the probe in
/// bin/webchat_probe.rs, pass `--site` to switch sites). The deepseek set was
/// calibrated one selector at a time against real sessions; the chatgpt /
/// gemini / claude sets are **first-guess values** written from the known
/// structure of each site — never calibrated under a real login. Run the
/// `--collect` probe to check, and fix the class names here if they don't match.
pub struct Site {
    pub id: &'static str,
    /// Name shown in the UI
    pub label: &'static str,
    /// Site home page (where "start a new chat" falls back to)
    pub home: &'static str,
    /// Composer (input box)
    pub composer: &'static str,
    /// Whether the composer is a textarea (deepseek) or contenteditable
    /// (chatgpt/gemini/claude) — the two are typed into differently:
    /// textarea uses the native setter + input event, contenteditable gets
    /// textContent written directly + input event
    pub composer_is_textarea: bool,
    /// Send button
    pub send: &'static str,
    /// Body container of the assistant's answer
    pub answer: &'static str,
    /// Thinking-process container. None = this site has no readable chain of
    /// thought (the body still works)
    pub think: Option<&'static str>,
    /// The "deep thinking" toggle: (toggle selector, marker class for "on").
    /// None = don't touch any toggle
    pub think_toggle: Option<(&'static str, &'static str)>,
    /// Text of the sidebar "new chat" button. Empty = go back to the home page
    /// to start over (slower but always correct)
    pub new_chat_text: &'static str,
    /// URL fragment that identifies "this is a conversation page" (used both
    /// to store the resume link and to confirm a new chat really left the old
    /// conversation). The sites differ: deepseek /a/chat/s/, chatgpt /c/,
    /// claude /chat/, gemini /app/.
    pub chat_url_marker: &'static str,
}

const DEEPSEEK: Site = Site {
    id: "deepseek",
    label: "DeepSeek",
    home: "https://chat.deepseek.com/",
    // Matching strings against the real (Chinese) page stay in Chinese on purpose.
    composer: r#"textarea[placeholder*="发送消息"]"#,
    composer_is_textarea: true,
    send: ".ds-button--primary",
    answer: ".ds-assistant-message-main-content",
    // Measured, hash-free class name (found by the --think probe in a real
    // session), hanging under .ds-message
    think: Some(".ds-think-content"),
    // "Deep thinking" toggle: on = extra --selected class (measured with the
    // --toggle probe). In chats opened by the driver it defaults to off; if we
    // don't turn it on, the model won't think at all.
    think_toggle: Some((".ds-toggle-button", "ds-toggle-button--selected")),
    // Text of the sidebar "new chat" button on the real page — keep as-is for matching.
    new_chat_text: "开启新对话",
    chat_url_marker: "/a/chat/s/",
};

const CHATGPT: Site = Site {
    id: "chatgpt",
    label: "ChatGPT",
    home: "https://chatgpt.com/",
    // Recalibrated against the real page (probe --site chatgpt --dom, 2026-10):
    // the composer is now a plain ProseMirror contenteditable — the old
    // #prompt-textarea is gone, and with it the driver waited forever on
    // "composer never appeared" while the page itself was fine.
    composer: "div.ProseMirror",
    composer_is_textarea: false,
    // Attribute values inside selectors must always use **double quotes**: the
    // driver JS wraps selectors in single quotes, and a stray single quote is
    // a syntax error for the whole script (measured: chatgpt/claude returned
    // zero payloads because of this — symptom "the page answered / the input
    // box is right there, but the panel spins forever").
    // The send-button testid is gone with the redesign; keep it for older
    // builds and lean on the localized aria-labels (SEND_JS additionally
    // scans every button by label, so this is a hint list, not the only path).
    send: "button[data-testid=\"send-button\"], button[aria-label*=\"发送\"], button[aria-label*=\"Send\"]",
    // Recalibrated on the real page (probe --site chatgpt, 2026-10): the old
    // [data-message-author-role="assistant"] no longer exists at all — the
    // page renders replies as a markdown root marked by this style attribute.
    // Chosen over [data-chatgpt-search-unit-key$=":assistant"] because that
    // one also wraps the sr-only "ChatGPT 说：" heading, which would leak
    // into the collected markdown.
    answer: "[data-markdown-text-style=\"assistant-message\"]",
    think: None,
    think_toggle: None,
    new_chat_text: "",
    chat_url_marker: "/c/",
};

const GEMINI: Site = Site {
    id: "gemini",
    label: "Gemini",
    home: "https://gemini.google.com/app",
    composer: "div.ql-editor.textarea",
    composer_is_textarea: false,
    // "发送" kept for matching the Chinese-localized aria-label.
    send: "button.send-button, button[aria-label*=\"发送\"], button[aria-label*=\"Send\"]",
    answer: "message-content",
    think: None,
    think_toggle: None,
    new_chat_text: "",
    chat_url_marker: "/app/",
};

const CLAUDE: Site = Site {
    id: "claude",
    label: "Claude",
    home: "https://claude.ai/new",
    composer: "div.ProseMirror",
    composer_is_textarea: false,
    // Recalibrated on the real page (probe --site claude --asktest, 2026-10):
    // the send button's aria-label is gone (the stable hook is the testid);
    // the old aria-labels stay for older builds.
    send: "button[data-testid=\"chat-input-send\"], button[aria-label=\"Send message\"], button[aria-label=\"发送消息\"]",
    // Recalibrated the same run: replies render inside a turn row marked by
    // this testid; the old .font-claude-response class no longer exists, and
    // with it the driver read 0 answers while the page was already answering.
    answer: "[data-testid=\"assistant-message\"]",
    think: None,
    think_toggle: None,
    new_chat_text: "",
    chat_url_marker: "/chat/",
};

/// Site registry. Looked up by id; unrecognized ids fall back to deepseek
/// (it must still work even if the config was hand-edited into garbage).
pub fn site_by_id(id: &str) -> &'static Site {
    match id.trim() {
        "chatgpt" => &CHATGPT,
        "gemini" => &GEMINI,
        "claude" => &CLAUDE,
        _ => &DEEPSEEK,
    }
}

pub const SITES: [&Site; 4] = [&DEEPSEEK, &CHATGPT, &GEMINI, &CLAUDE];

/// Upper bound per increment pushed to Rust (in characters). The title has a
/// length limit; leave plenty of headroom.
const CHUNK: usize = 400;
/// Polling interval
const POLL: Duration = Duration::from_millis(250);
/// The answer counts as finished after this many consecutive unchanged polls
const STABLE_POLLS: u32 = 6;
/// Time between pasting the image and hitting send: the page needs to accept
/// the image first (draw the thumbnail), otherwise the send carries no attachment
const ATTACH_SETTLE: Duration = Duration::from_millis(1200);
/// After starting a new chat, wait for the page to render the new conversation
/// before touching the toggle. We have measured cases where the toggle could
/// not be clicked yet.
const SETTLE_AFTER_NEW_CHAT: Duration = Duration::from_millis(900);

/// **Converts the answer's HTML to Markdown** instead of grabbing innerText.
///
/// Why the conversion is mandatory: `innerText` only gives text and loses all
/// structure — headings lose their `#`, lists lose `-`/`1.`, tables collapse
/// into one cell per line, code blocks lose their fences, citation markers
/// become a trail of lonely digits. The panel renders Markdown, so once
/// structure is lost "a lot of the content just looks wrong".
///
/// A few details written against the real page:
///   * Formulas: KaTeX keeps the original LaTeX inside
///     `<annotation encoding="application/x-tex">`, far more reliable than
///     grabbing the rendered glyphs; take it and wrap it in `$...$`;
///   * Citations: keeping them only yields scattered digits, drop the whole
///     block;
///   * `svg` / `button` / input boxes are not body content, drop them.
///
/// Defined as `window.__snapiMD` so every later poll can call it directly.
const MARKDOWN_JS: &str = r#"
window.__snapiMD = (function () {
  // TIME: Claude's action bar carries a relative timestamp ("现在") inside the
  // reply container; it is chrome, not body text.
  const SKIP = new Set(['SCRIPT', 'STYLE', 'SVG', 'BUTTON', 'TEXTAREA', 'INPUT', 'CANVAS', 'TIME']);
  // Screen-reader-only text (Claude ships a "Claude 已回复：<问题>" h2.sr-only
  // at the top of every reply) must not leak into the markdown: as a heading
  // it renders as a huge bold line, and the thinking fallback then re-reads
  // it as "chain of thought" — the same passage shown twice.
  const skipEl = (n) => n.classList && (n.classList.contains('sr-only') || /(^|\s)sr-only(\s|$)/.test(String(n.className)));

  const inlineOf = (node) => {
    let out = '';
    for (const child of node.childNodes) out += inlineNode(child);
    return out;
  };

  const inlineNode = (n) => {
    if (n.nodeType === 3) return n.nodeValue;
    if (n.nodeType !== 1) return '';
    if (SKIP.has(n.tagName)) return '';
    if (skipEl(n)) return '';
    if (typeof n.className === 'string' && /citation/i.test(n.className)) return '';

    // Formula: prefer the raw LaTeX that KaTeX keeps around
    if (n.classList && n.classList.contains('katex')) {
      const ann = n.querySelector('annotation[encoding="application/x-tex"]');
      if (ann) return '$' + ann.textContent.trim() + '$';
      return inlineOf(n);
    }

    switch (n.tagName) {
      case 'BR': return '\n';
      case 'CODE': return '`' + (n.textContent || '').replace(/`/g, '') + '`';
      case 'STRONG': case 'B': return '**' + inlineOf(n) + '**';
      case 'EM': case 'I': return '*' + inlineOf(n) + '*';
      case 'DEL': case 'S': return '~~' + inlineOf(n) + '~~';
      case 'A': {
        const href = n.getAttribute('href') || '';
        let text = inlineOf(n).trim();
        // Citation link text is usually decorated (-1, [1], ...); keeping only
        // the digits looks more like body text
        const cite = /^[\[\-\s]*(\d+)[\]\-\s]*$/.exec(text);
        if (cite) text = cite[1];
        if (!href || href.startsWith('javascript') || href === '#') return text;
        return '[' + (text || href) + '](' + href + ')';
      }
      case 'IMG': {
        const src = n.getAttribute('src') || '';
        const alt = n.getAttribute('alt') || 'image';
        return src ? '![' + alt + '](' + src + ')' : '';
      }
      default: return inlineOf(n);
    }
  };

  // Table -> GFM pipe table. Without this conversion the cells collapse into
  // one cell per line and the row/column relationships become unreadable
  const tableOf = (table) => {
    const rows = [];
    for (const tr of table.querySelectorAll('tr')) {
      const cells = [...tr.children].map((cell) =>
        inlineOf(cell).replace(/\s*\n\s*/g, ' ').trim().replace(/\|/g, '\\|'));
      if (cells.length) rows.push(cells);
    }
    if (!rows.length) return '';
    const width = Math.max(...rows.map((r) => r.length));
    const pad = (row) => { while (row.length < width) row.push(''); return row; };
    const head = pad(rows[0].slice());
    const out = [
      '| ' + head.join(' | ') + ' |',
      '| ' + head.map(() => '---').join(' | ') + ' |',
    ];
    for (const row of rows.slice(1)) out.push('| ' + pad(row).join(' | ') + ' |');
    return out.join('\n');
  };

  const out = [];
  const push = (text) => {
    for (const line of String(text).split('\n')) {
      const trimmed = line.replace(/\s+$/, '');
      if (trimmed.trim()) out.push(trimmed);
    }
  };

  const walk = (node) => {
    for (const child of node.childNodes) {
      if (child.nodeType === 3) { push(child.nodeValue); continue; }
      if (child.nodeType !== 1) continue;
      if (SKIP.has(child.tagName)) continue;
      if (skipEl(child)) continue;
      if (typeof child.className === 'string' && /citation/i.test(child.className)) continue;

      const tag = child.tagName;
      if (/^H[1-6]$/.test(tag)) { push('#'.repeat(Number(tag[1])) + ' ' + inlineOf(child).trim()); continue; }
      if (tag === 'P') { push(inlineOf(child).trim()); continue; }
      if (tag === 'PRE') {
        push('```\n' + (child.textContent || '').replace(/\n+$/, '') + '\n```');
        continue;
      }
      if (tag === 'UL' || tag === 'OL') {
        let index = 1;
        for (const li of child.children) {
          if (li.tagName !== 'LI') continue;
          const marker = tag === 'OL' ? (index++) + '. ' : '- ';
          push(marker + inlineOf(li).trim());
        }
        continue;
      }
      if (tag === 'BLOCKQUOTE') {
        push(inlineOf(child).trim().split('\n').map((l) => '> ' + l).join('\n'));
        continue;
      }
      if (tag === 'HR') { push('---'); continue; }
      if (tag === 'TABLE') { push(tableOf(child)); continue; }
      // Other containers (div/section and the like): keep digging down
      walk(child);
    }
  };

  return (root) => {
    if (!root) return '';
    out.length = 0;
    try {
      walk(root);
      return out.join('\n\n');
    } catch (e) {
      // If one small piece cannot be read (super-deep DOM, unseen structure...)
      // it must **not take down the whole collection**: if thrown, this script
      // dies here every single time, not a single payload gets sent, and the
      // symptom is "the answer is fully written on the page but the panel never
      // receives a word". Fall back to plain text — at least the body survives —
      // and record the failure reason on window, reported with the payload for
      // the Rust side to log.
      window.__snapiMdErr = String((e && e.message) || e);
      out.length = 0;
      const raw = typeof root.innerText === 'string' ? root.innerText : root.textContent;
      return String(raw || '').replace(/\n{3,}/g, '\n\n').trim();
    }
  };
})();
"#;

/// The page-side state hangs off window and accumulates across evals.
///
/// `nodes` records how many answers were on the page before this round
/// started: only a **new** one is read, otherwise follow-up questions would
/// treat the previous round's answer as the beginning of this round and repeat
/// it from the top.
///
/// Each run pushes one increment (possibly empty) plus the `ready` / `answers`
/// / `chars` status, so the Rust side can both wait for the page to be ready
/// and tell "hasn't started talking yet" apart from "finished talking" —
/// on timeout those numbers are the only thing that can explain whether the
/// page didn't render or we didn't receive.
const COLLECT_JS: &str = r#"
(() => {
  /**
   * Fallback when the collection script itself breaks.
   *
   * There used to be no fallback: the moment the script threw, the whole eval
   * died there and the title channel could not send a single payload. The user
   * saw "the answer is fully written on the page, the panel still says
   * analyzing", waited two minutes for the timeout, and the timeout message
   * only said "received 0 payloads" — nothing to go on. Now the reason
   * (plus the actual body length on the page) is put into an `err` field.
   * Note the field is `err`, not `error`: `error` fails the round immediately,
   * while an occasional collection failure should not interrupt a perfectly
   * good question — `err` only goes to the log and the timeout hint.
   */
  const fail = (e) => {
    try {
      const st = window.__snapi || (window.__snapi = { i: 0 });
      st.i = (st.i || 0) + 1;
      const nodes = document.querySelectorAll('__ANSWER__');
      const last = nodes.length ? nodes[nodes.length - 1] : null;
      document.title = '__TAG__' + JSON.stringify({
        i: st.i,
        err: String((e && e.message) || e),
        ready: !!document.querySelector('__COMPOSER__'),
        answers: nodes.length,
        chars: last ? String(last.textContent || '').length : 0,
      });
    } catch (e2) { /* can't even ship the error; give up */ }
  };
  try {
    // Page-side state. **Its shape is owned here**: the compose side uses this
    // same copy when building state (see STATE_SHAPE_JS). If the two shapes
    // disagree, this script throws a TypeError on feed(st.a, ...) — symptom:
    // "the page answered but the panel receives nothing", because the script
    // dies before reading the body every time and no payload gets out.
    // So we not only create it when missing, we **rebuild it when the shape is
    // wrong (left by an older version)**, keeping the nodes baseline.
    if (!window.__snapi || !window.__snapi.a) {
      // One state each for the body and the chain of thought (for
      // read/sent/pending/rstNext see STATE_SHAPE_JS).
      // Rebuild when the shape is wrong (older version) too, keeping the nodes baseline if possible.
      __STATE_SHAPE__
    }
    const st = window.__snapi;
    __PAGE_DIAG__
    const box = document.querySelector('__COMPOSER__');
    const nodes = document.querySelectorAll('__ANSWER__');
    const norm = (s) => (s || '').replace(/\s+/g, ' ').trim();

    /**
     * How one stream is pushed out.
     *
     * The sites **redraw while streaming**: the small span under the cursor is
     * rewritten over and over. In a measured 600-char answer, a half-finished
     * table row was redrawn three times (`| **GTA5.exe** | game |` later
     * completed into a full row). So the assumption "new text always continues
     * after the last text" does not hold — the earlier code re-pushed the whole
     * thing whenever that broke, and the stored answer became the same passage
     * repeated four times (157+293+395+602 chars).
     *
     * The rule now: while it continues writing, push increments; otherwise
     * re-send the whole span and have the other side **replace** instead of
     * append. The re-send only happens after the rewritten span, and it is
     * pushed in chunks, so the cost is bounded.
     */
    const feed = (s, text) => {
      if (text === s.read) return;
      s.read = text;
      let n = 0;
      const max = Math.min(text.length, s.sent.length);
      while (n < max && text[n] === s.sent[n]) n += 1;
      if (n >= s.sent.length) {
        s.pending += text.slice(n);
      } else {
        s.pending = text;        // Drop the unfinished old content: a full re-send means "start over"
        s.rstNext = true;
      }
      s.sent = text;
    };

    // Body. We follow the **last** answer node: on follow-ups the previous
    // round's answer is still on the page; the comparison inside feed (new text
    // not continuing the old) treats it as a "full re-send" and overwrites it.
    // The baseline gate has **two** ways through: more nodes than at compose
    // time, **or** the last one's content changed. Counting alone deadlocks:
    // when the site switches pages the baseline holds the old page's count,
    // the new conversation's answers get smaller indexes and never pass
    // (measured: page reports "2 answers, 0 chars", the cursor spins to timeout).
    const lastA = nodes.length ? nodes[nodes.length - 1] : null;
    const freshA = nodes.length > (st.baseA || 0)
      || (lastA ? String(lastA.textContent || '').length : 0) !== (st.baseLen || 0);
    if (freshA && nodes.length) feed(st.a, window.__snapiMD(nodes[nodes.length - 1]));
    st.nodes = nodes.length;

    // Thinking process: identified via `.ds-think-content` (measured stable,
    // see the comment at the constant).
    //
    // The body and the thinking are **two independent streams**, each tracked
    // separately: the model may think for a long time before speaking, during
    // which the answer node hasn't appeared at all — reading off the answer
    // node would lose the whole chain of thought ("stuck at analyzing, no
    // chain of thought visible" comes exactly from that).
    //
    // textContent instead of innerText: the thinking block may be collapsed by
    // default; innerText returns an empty string for folded content while
    // textContent can still read it.
    const thinks = document.querySelectorAll('__THINK__');
    const lastT = thinks.length ? thinks[thinks.length - 1] : null;
    const freshT = thinks.length > (st.baseT || 0)
      || (lastT ? String(lastT.textContent || '').length : 0) !== (st.baseLenR || 0);
    if (freshT && thinks.length) {
      feed(st.r, norm(thinks[thinks.length - 1].textContent));
    } else if (__THINK_FALLBACK__ && freshA && nodes.length) {
      // Fallback: when the class name changed, or this model has no thinking
      // block at all, go back to the old trick — take the text in the message
      // root **other than the answer** as the thinking process (filter list below).
      try {
        const node = nodes[nodes.length - 1];
        const msg = node.closest('.ds-message') || node.parentElement;
        if (msg) {
          const answer = norm(node.innerText);
          let extra = norm(msg.innerText);
          if (answer) extra = extra.split(answer).join('');
          // The message root also mixes in buttons/citations/footer text;
          // remove the ones we recognize.
          // (Regexes below match Chinese UI text on the real page — keep as-is.)
          extra = extra
            .replace(/深度思考|已深度思考|智能搜索|搜索到\s*\d+\s*个网页|\d+\s*个网页/g, '')
            .replace(/收起|展开|复制|重新生成|分享|编辑|朗读|内容由 AI 生成，请仔细甄别/g, '')
            .replace(/用时\s*[\d.]+\s*秒/g, '')
            .replace(/\s+/g, ' ')
            .trim();
          // Anything too short is probably just button text; better to not show it
          if (extra.length >= 40) feed(st.r, extra);
        }
      } catch (e) { /* if the thinking process can't be read, so be it; the body is unaffected */ }
    }
    st.thinks = thinks.length;

    // Final re-read: when the length reported back doesn't match the page (the
    // title channel may merge two adjacent changes, and losing one payload on
    // the increment path is a permanent gap), re-send the whole body once.
    // This guarantees: **the answer stored in the panel is identical to the one
    // on the page**.
    if (__FORCE__) {
      // Baseline: how many answers / thinking blocks were on the page before
      // the question (recorded by the Rust side before asking). Only what is
      // **new on top of it** certainly belongs to this round; otherwise it is
      // likely residue from the previous round, and pushing it wholesale
      // creates "new question with old answer" — harder to notice than not
      // reading anything. When the baseline is 0 (new conversation, or nothing
      // read) no limit is applied, so the normal path is never blocked.
      // The baseline comes from the page state recorded at compose time
      // (st.baseA/baseT), with a second gate for "content changed" — same
      // reason as the body gate above (measured "2 answers, 0 chars").
      const lastFA = nodes.length ? nodes[nodes.length - 1] : null;
      const freshA = nodes.length > (st.baseA || 0) || (st.baseA || 0) === 0
        || (lastFA ? String(lastFA.textContent || '').length : 0) !== (st.baseLen || 0);
      const lastFT = thinks.length ? thinks[thinks.length - 1] : null;
      const freshR = thinks.length > (st.baseT || 0) || (st.baseT || 0) === 0
        || (lastFT ? String(lastFT.textContent || '').length : 0) !== (st.baseLenR || 0);
      if (nodes.length && freshA) {
        const text = window.__snapiMD(nodes[nodes.length - 1]);
        if (text.length && text.length !== __HAVE__) {
          st.a.pending = text;
          st.a.rstNext = true;
          st.a.sent = text;
          st.a.read = text;
        }
      }
      if (thinks.length && freshR) {
        const text = norm(thinks[thinks.length - 1].textContent);
        if (text.length && text.length !== __HAVER__) {
          st.r.pending = text;
          st.r.rstNext = true;
          st.r.sent = text;
          st.r.read = text;
        }
      }
    }

    const aChunk = st.a.pending.slice(0, __CHUNK__);
    st.a.pending = st.a.pending.slice(__CHUNK__);
    const aRst = !!st.a.rstNext;
    st.a.rstNext = false;
    // Separate buffer for the chain of thought: it and the body are two
    // streams; mixing them makes the panel unable to display them apart
    const rChunk = st.r.pending.slice(0, __CHUNK__);
    st.r.pending = st.r.pending.slice(__CHUNK__);
    const rRst = !!st.r.rstNext;
    st.r.rstNext = false;
    // The composer fields (typed char count, images in the container) are only
    // used by the probe: in a dry run it confirms "the image was accepted and
    // the question was typed in" — at that point sending must not be clicked.
    // Going up three levels is measured: the thumbnails hang in a container
    // three levels above the composer, one level up isn't enough.
    const box3 = box && box.parentElement && box.parentElement.parentElement
      ? box.parentElement.parentElement.parentElement : null;
    const form = (box && box.closest('form')) || box3;
    // Still generating? Check whether a button labelled "stop" exists (while
    // generating the site swaps send for stop — same recognizer as STOP_JS).
    // "Answered" is judged by this, more reliable than "body unchanged for
    // 1.5s" — deep thinking pauses longer than 1.5s between breaths all the
    // time, and judging by pause would declare a still-thinking round finished
    // (measured: panel says "no body at all" while the page is writing away).
    const gen = [...document.querySelectorAll('div[role=button], button')].some((n) => {
      // Guard getBoundingClientRect: the test bench's DOM stand-in lacks it
      if (typeof n.getBoundingClientRect !== 'function' || !(n.getBoundingClientRect().width > 0)) return false;
      // "停止/停止生成" match the Chinese stop button — keep for matching.
      return /停止|停止生成|Stop/i.test(((n.innerText || '') + ' ' + (n.getAttribute('aria-label') || '')).trim());
    });
    // i increments every time so the title is guaranteed to "change" — an
    // identical title does not fire the event again
    st.i += 1;
    document.title = '__TAG__' + JSON.stringify({
      i: st.i,
      d: aChunk,
      more: st.a.pending.length > 0,
      rst: aRst,
      dr: rChunk,
      rmore: st.r.pending.length > 0,
      rrst: rRst,
      ready: !!box,
      answers: nodes.length,
      thinks: thinks.length,
      gen: gen,
      chars: st.a.read.length,
      mderr: (() => { const m = window.__snapiMdErr || ''; window.__snapiMdErr = ''; return m; })(),
      input: box ? ((box.value || box.textContent || '').length) : -1,
      imgs: form ? form.querySelectorAll('img').length : -1,
      pt: pageDiag.pt,
      bx: pageDiag.bx,
    });
  } catch (e) { fail(e); }
})()
"#;

/// Page diagnosis fields (`pt` = the page's own title, `bx` = the first slice
/// of the body).
///
/// Added because "composer never appeared" never said **what the page was
/// actually showing** — a login page, a Cloudflare challenge, a loading white
/// screen all look identical, and the user could only wait against a vague
/// guess (measured: exactly what happens when a chatgpt session expires).
/// These two fields let the Rust side write the page's actual appearance into
/// logs and timeout messages. Must be read **before** overwriting the title:
/// once title is written it is no longer the site's.
const PAGE_DIAG_JS: &str = r#"
  if (window.__snapiPageTitle === undefined) window.__snapiPageTitle = document.title;
  const pageDiag = {
    pt: String(window.__snapiPageTitle || '').slice(0, 80),
    bx: document.body ? String(document.body.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 140) : '',
  };
"#;

/// Start a new conversation.
///
/// Prefer clicking "new chat"; **if it can't be clicked, go back to the home
/// page directly** — with the sidebar collapsed that button is not rendered at
/// all (measured: in a narrow window the sidebar is collapsed by default and
/// only appears after the user clicks), and without the fallback "start a new
/// chat" would silently continue the old conversation, mixing up context.
///
/// Both routes report back to Rust via the title: going home reloads the page,
/// so readiness has to be awaited again.
const NEW_CHAT_JS: &str = r#"
(() => {
  const visible = (n) => n.getBoundingClientRect().width > 0;
  const exact = [...document.querySelectorAll('div, button, span, a')]
    .find((n) => (n.innerText || '').trim() === '__TEXT__' && visible(n));
  const loose = exact || [...document.querySelectorAll('[role=button], a')]
    .find((n) => (n.innerText || '').includes('__TEXT__') && visible(n));

  if (loose) {
    loose.click();
    window.__snapi = null;
    document.title = '__TAG__' + JSON.stringify({ newchat: 'clicked' });
    return;
  }
  document.title = '__TAG__' + JSON.stringify({ newchat: 'fallback' });
  location.href = '__HOME__';
})()
"#;

/// The shape of the page-side driving state. **There is only this one copy**:
/// collect rebuilds from it when it has no state (or an old-shaped one), and
/// compose rebuilds from it before every round (also recording the "how many
/// answers existed before this round" baseline).
///
/// The two sides used to have separate initializers; changing one and
/// forgetting the other made collect crash in place on feed(st.a, …) with a
/// TypeError and not a single payload got out — "the page answered, the panel
/// spins forever" with no way to tell where the chain broke. That was it.
const STATE_SHAPE_JS: &str = r#"
window.__snapi = {
  nodes: (window.__snapi && window.__snapi.nodes) || 0,
  thinks: (window.__snapi && window.__snapi.thinks) || 0,
  // Baseline: how many answers/thinking blocks were on the page at compose
  // time. collect only reads whatever is **in addition** — when old answers
  // render a beat late (page switching, re-render gap), without this gate they
  // would be collected as this round's answer (measured: three rounds of
  // records all stored the same old answer).
  baseA: (window.__snapi && window.__snapi.baseA) || 0,
  baseT: (window.__snapi && window.__snapi.baseT) || 0,
  // Body length of the "last one" at baseline moment: counts are unreliable
  // (page switch: old page 2, new conversation 1 — comparing counts alone
  // would block the new answer forever; measured "2 answers, 0 chars" spinning
  // to timeout), so a content change also passes.
  baseLen: (window.__snapi && window.__snapi.baseLen) || 0,
  baseLenR: (window.__snapi && window.__snapi.baseLenR) || 0,
  i: 0,
  a: { read: '', sent: '', pending: '', rstNext: false },
  r: { read: '', sent: '', pending: '', rstNext: false },
};
"#;

/// Put images and text into the composer, **without clicking send** (sending
/// is a separate step, see SEND_JS).
///
/// Text must go through React's native setter plus an input event; assigning
/// value directly is invisible to it. Images go into the hidden
/// input[type=file]: `files` likewise must be assigned via the native setter,
/// followed by a change event — measured: the page accepts it and draws the
/// thumbnail.
///
/// **The page-side state shape has only one copy** (`STATE_SHAPE_JS`); compose
/// builds the baseline and collect reads increments from it. The two scripts
/// used to have separate initializers; changing collect's shape without
/// touching compose's made collect throw a TypeError on `feed(st.a, …)` every
/// time — the page finished generating the answer, the panel received not a
/// word, and because not a single payload got out there wasn't even a clue
/// where it broke. That chain is now watched by a test
/// (`compose_and_collect_agree_on_the_state_shape`).
const COMPOSE_JS: &str = r#"
(async () => {
  const args = __ARGS__;
  const composer = document.querySelector('__COMPOSER__');
  if (!composer) {
    document.title = '__TAG__' + JSON.stringify({ error: 'Composer (input box) not found on the page' });
    return;
  }

  // How many answers existed before this round; only whatever is in addition
  // is this round's new answer. The shape must be **character-for-character
  // identical** to the one in COLLECT_JS — that side expects exactly this
  // structure, and on mismatch it crashes entirely (true story: compose kept
  // an old shape, the answer came out and the panel received nothing).
  // So this references the same shape constant as collect, see STATE_SHAPE_JS.
  __STATE_SHAPE__
  window.__snapi.nodes = document.querySelectorAll('__ANSWER__').length;
  window.__snapi.thinks = document.querySelectorAll('__THINK__').length;
  // Baseline recorded **at this moment**: collect only reads what exceeds it
  // (see baseA/baseT in STATE_SHAPE_JS)
  window.__snapi.baseA = window.__snapi.nodes;
  window.__snapi.baseT = window.__snapi.thinks;
  const lastAns = document.querySelectorAll('__ANSWER__');
  window.__snapi.baseLen = lastAns.length
    ? String(lastAns[lastAns.length - 1].textContent || '').length : 0;
  const lastThink = document.querySelectorAll('__THINK__');
  window.__snapi.baseLenR = lastThink.length
    ? String(lastThink[lastThink.length - 1].textContent || '').length : 0;

  // Two paths for pasting images: **synthetic paste events** (every editor
  // supports Ctrl+V image paste; a synthetic paste goes through the same
  // handling pipeline) and the **hidden input[type=file]**. Order depends on
  // the site: on a textarea (deepseek) the file input is the chat upload box,
  // so try the file input first; on the contenteditable three, the first
  // file input found may not be the chat one (measured on chatgpt: stuffing
  // the first input[type=file] did nothing), so try paste first.
  // Which path was actually taken is reported in the composed payload
  // (img field), so failures can be correlated.
  const images = args.images || [];
  let imgPath = 'none';
  if (images.length) {
    const mkDt = async () => {
      const dt = new DataTransfer();
      for (const url of images) {
        const blob = await (await fetch(url)).blob();
        dt.items.add(new File([blob], 'snip.png', { type: blob.type || 'image/png' }));
      }
      return dt;
    };
    const tryPaste = async () => {
      try {
        const dt = await mkDt();
        composer.focus();
        composer.dispatchEvent(new ClipboardEvent('paste', {
          clipboardData: dt, bubbles: true, cancelable: true,
        }));
        return true;
      } catch (e) { return false; }
    };
    const tryFile = async () => {
      try {
        // There may be several file inputs on the page (measured on chatgpt:
        // 5 of them, the first has nothing to do with chat). Selection rule:
        // only ones whose accept contains image count; among those, one whose
        // id contains media wins; if none qualify, don't stuff anything.
        let best = null;
        for (const input of document.querySelectorAll('input[type=file]')) {
          const accept = input.getAttribute('accept') || '';
          if (!accept.includes('image')) continue;
          if (!best || ((input.id || '').includes('media') && !(best.id || '').includes('media'))) {
            best = input;
          }
        }
        if (!best) return false;
        // **multiple must be turned on first** — without it, assigning several
        // files makes the browser keep only the first
        best.multiple = true;
        const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'files').set;
        setter.call(best, await mkDt());
        best.dispatchEvent(new Event('change', { bubbles: true }));
        best.dispatchEvent(new Event('input', { bubbles: true }));
        return true;
      } catch (e) { return false; }
    };
    let pasted = __COMPOSER_IS_TA__ ? false : await tryPaste();
    if (pasted) imgPath = 'paste';
    else {
      const filed = await tryFile();
      if (filed) imgPath = 'file';
      else {
        // On textarea sites, if the file input failed, still try paste as a fallback
        if (await tryPaste()) imgPath = 'paste';
      }
    }
    if (imgPath === 'none') {
      // When both JS paths fail, **don't declare defeat here**: on the three
      // contenteditable sites (chatgpt/gemini/claude) images still have a
      // third path — CDP native upload (file chooser interception / clipboard
      // Ctrl+V, see cdp_deliver), which goes through the real user-upload
      // pipeline and can deliver what synthetic events cannot. Measured: after
      // a chatgpt redesign both paste and file-input paths went dead; if an
      // error were raised here the whole round would abort and the CDP path
      // would never get its turn. So leave 'none' for the Rust side to recheck
      // (cdp_deliver does a native upload whenever it sees something other
      // than 'paste'); only textarea sites (deepseek, no CDP image fallback)
      // are truly out of luck and report the error as before.
      if (__COMPOSER_IS_TA__) {
        document.title = '__TAG__' + JSON.stringify({
          error: 'Failed to hand the screenshot to the web UI (neither the paste path nor the file input path worked)',
        });
        return;
      }
    }
  }
  // Keep the path in the page state: the CDP path rechecks it before typing
  // (falls back to CDP native file setting when paste failed)
  window.__snapi.imgPath = imgPath;

  if (__COMPOSER_IS_TA__) {
    // textarea (deepseek): React's native setter plus an input event;
    // assigning value directly is invisible to it
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set;
    setter.call(composer, args.prompt);
    composer.dispatchEvent(new Event('input', { bubbles: true }));
  } else {
    // contenteditable (chatgpt/gemini/claude): **text is NOT typed here** —
    // these editors (Quill/tiptap) roll back all synthetic events (measured:
    // three injection methods, all vanished within 2 seconds). Text goes
    // through the CDP real input pipeline (Input.insertText, see cdp.rs).
    // Only focus here.
    composer.focus();
  }
  document.title = '__TAG__' + JSON.stringify({ composed: true, img: imgPath });
})()
"#;

/// Click send.
///
/// Why not "press Enter in the composer": synthetic keyboard events are not
/// trusted events, the browser won't let them trigger default behavior, and
/// React likely never sees them either. Clicking the button is the real deal.
const SEND_JS: &str = r#"
(() => {
  const cut = (x, n) => (typeof x === 'string' ? x.slice(0, n) : '');
  const vis = (n) => n.getBoundingClientRect && n.getBoundingClientRect().width > 0;
  const disabled = (n) => n.disabled === true || n.getAttribute('aria-disabled') === 'true';
  // Widened candidates: the registry's selector + buttons recognized as "send"
  // by aria-label/class name. (发送/提交 regexes match the Chinese UI — keep.)
  const sel = [...document.querySelectorAll('button, [role=button]')].filter(vis);
  const bySel = [...document.querySelectorAll('__SEND__')];
  const lookSend = (n) => {
    const a = (n.getAttribute('aria-label') || '').trim();
    const t = (n.innerText || '').trim();
    const c = typeof n.className === 'string' ? n.className : '';
    return /^(发送|Send|Submit|提交)(消息)?$/.test(a) || a === '发送提示词'
      || /(^|\s)(send|submit)(-|\s|$)/i.test(c) || t === '发送';
  };
  const pool = [...bySel, ...sel.filter(lookSend)];
  // Exclude disabled ones (the send button is grey until the site accepts the
  // input — wait a moment and try again)
  const usable = pool.filter((n) => !disabled(n));
  if (!usable.length && pool.length) {
    // Disabled only: click again after 900ms (there is a gap between synthetic
    // input and the site's state settling)
    setTimeout(() => {
      const again = pool.filter((n) => vis(n) && !disabled(n));
      if (again.length) { again[again.length - 1].click(); return; }
      document.title = '__TAG__' + JSON.stringify({
        error: 'Send button stayed disabled (the site never accepted the input): ' + JSON.stringify(
          pool.slice(0, 6).map((n) => cut(typeof n.className === 'string' ? n.className : '', 40))
        ),
      });
    }, 900);
    return;
  }
  if (!usable.length) {
    // Nothing clickable at all: report what the candidates look like so the
    // site registry can be fixed accordingly — don't just sit waiting
    // (measured: pasting and typing succeeded, the send button wasn't
    // recognized, and the panel spun on "analyzing")
    const cand = pool.slice(0, 6).map((n) => ({
      aria: cut(n.getAttribute('aria-label') || '', 24),
      cls: cut(typeof n.className === 'string' ? n.className : '', 40),
      dis: disabled(n),
    }));
    document.title = '__TAG__' + JSON.stringify({
      error: cand.length
        ? 'Send buttons found but all disabled: ' + JSON.stringify(cand)
        : 'Send button not found (no button on the page was recognizable)',
    });
    return;
  }
  usable[usable.length - 1].click();
})()
"#;

/// Stop the page.
///
/// Used when the user presses cancel: just stopping the read is not enough —
/// the page is still generating, it keeps burning the user's quota, and it may
/// still be writing when the next question comes. Only click the button that
/// says "stop"; **if it cannot be found, do nothing** — there used to be a
/// "step back and click the primary button again" fallback, but at the moment
/// generation has ended the primary button is "send", and blindly clicking it
/// re-sends whatever is in the composer (it got its share of sending the same
/// image twice). If nothing works, no error either; at worst the site finishes
/// writing on its own.
const STOP_JS: &str = r#"
(() => {
  const visible = (n) => n.getBoundingClientRect().width > 0;
  const byText = [...document.querySelectorAll('div[role=button], button, [aria-label]')]
    .find((n) => {
      const text = ((n.innerText || '') + ' ' + (n.getAttribute('aria-label') || '')).trim();
      // 停止/停止生成 match the Chinese stop button — keep for matching.
      return visible(n) && /停止|停止生成|Stop/i.test(text);
    });
  if (byText) byText.click();
})()
"#;

/// Turn "deep thinking" on (only done once, right after starting a new chat).
///
/// **Check the marker before clicking**: the "on" sign is the `--selected`
/// class (measured with the --toggle probe). Clicking blindly on an already-on
/// toggle would turn it off — worse than not turning it on. If the toggle
/// can't be found (site redesigned), no harm: at worst this round has no chain
/// of thought, the body works as usual.
///
/// Report with the `think` key, **never `error`** — on the Rust side `error`
/// aborts the whole round; a toggle that can't be clicked shouldn't kill the
/// question.
const THINK_ON_JS: &str = r#"
(() => {
  const report = (state) => {
    document.title = '__TAG__' + JSON.stringify({ n: Date.now(), think: state });
  };
  try {
    const visible = (n) => n.getBoundingClientRect().width > 0;
    // 深度思考 matches the Chinese toggle label — keep for matching.
    const btn = [...document.querySelectorAll('__TOGGLE__')]
      .find((n) => visible(n) && (n.innerText || '').includes('深度思考'));
    if (!btn) { report('missing'); return; }
    if (String(btn.className).includes('__MARKER__')) { report('already'); return; }
    btn.click();
    report('on');
  } catch (e) {
    report('exception: ' + ((e && e.message) || e));
  }
})()
"#;

/// What one Q&A turn hands to the page.
pub struct Turn {
    pub prompt: String,
    /// Images to paste this round (base64 JPEG, no data: prefix), possibly
    /// several. Empty on follow-ups (no new screenshot).
    pub images: Vec<String>,
    /// Start a new conversation, or continue the current one
    pub new_chat: bool,
    /// The site conversation URL to **resume** (`.../a/chat/s/<id>`).
    ///
    /// The record stores the site conversation for the chat (written after
    /// each round, see `run_turn`'s return value); follow-ups deep-link
    /// straight there — the site keeps the context itself, so there is **no
    /// need to re-send the previous turns one by one** (slow, and it litters
    /// the site's history with duplicate conversations). When None, `new_chat`
    /// decides between a new chat and continuing on the current page.
    pub resume: Option<String>,
}

impl Turn {
    fn args(&self) -> serde_json::Value {
        json!({
            "prompt": self.prompt,
            "images": self
                .images
                .iter()
                .map(|b| format!("data:image/jpeg;base64,{b}"))
                .collect::<Vec<_>>(),
        })
    }
}

/// What a finished turn brings back: the answer, and the conversation URL the
/// site is on now.
pub struct TurnOutcome {
    pub answer: String,
    /// A `/a/chat/s/<id>` URL; empty when the site never entered a
    /// conversation (no answer, still on the home page)
    pub site_url: String,
}

fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .fold(template.to_string(), |acc, (from, to)| acc.replace(from, to))
}

/// Site selection for the probe: deepseek by default, `--site chatgpt` etc. to switch.
static PROBE_SITE: std::sync::OnceLock<&'static Site> = std::sync::OnceLock::new();
pub fn probe_select_site(id: &str) {
    let _ = PROBE_SITE.set(site_by_id(id));
}
fn current_site() -> &'static Site {
    PROBE_SITE.get().copied().unwrap_or(&DEEPSEEK)
}
/// For the probe: the site the probe selected (--site decides, deepseek default)
pub fn probe_site() -> &'static Site {
    current_site()
}

/// No-arg version for the probe (site chosen by `--site`); the driver itself
/// uses the site-carrying variant.
pub fn collect_js() -> String {
    collect_js_for(current_site())
}

/// Used for the final re-read: have the page compare "the copy the other side
/// holds" against the page's length, and re-send the whole body on mismatch.
///
/// Why it is needed: the title channel is not reliable (the browser may merge
/// two adjacent title changes into one), and once an increment is lost it is a
/// permanent gap. One re-read is the only way to align "the answer in the
/// panel" with "the passage on the page" — the latter being the only copy the
/// user can see.
pub fn collect_js_flush(site: &Site, have: usize, have_reasoning: usize, base: Baseline) -> String {
    collect_js_for_with(site, true, have, have_reasoning, base)
}

pub fn collect_js_for(site: &Site) -> String {
    collect_js_for_with(site, false, 0, 0, Baseline::default())
}

fn collect_js_for_with(site: &Site, force: bool, have: usize, have_reasoning: usize, base: Baseline) -> String {
    // The converter must be injected along: every eval is a clean run, nothing
    // defined in a previous step survives
    format!(
        "{}\n{}",
        markdown_js(),
        fill(
            COLLECT_JS,
            &[
                ("__COMPOSER__", site.composer),
                // For sites without a thinking container, a selector that never
                // matches; querySelectorAll won't blow up
                ("__ANSWER__", site.answer),
                ("__THINK__", site.think.unwrap_or(".snapi-no-think")),
                ("__THINK_FALLBACK__", if site.think.is_some() { "true" } else { "false" }),
                ("__CHUNK__", &CHUNK.to_string()),
                ("__TAG__", TAG),
                ("__FORCE__", if force { "true" } else { "false" }),
                ("__HAVE__", &have.to_string()),
                ("__HAVER__", &have_reasoning.to_string()),
                ("__BASE_A__", &base.answers.to_string()),
                ("__BASE_T__", &base.thinks.to_string()),
                ("__STATE_SHAPE__", STATE_SHAPE_JS),
                ("__PAGE_DIAG__", PAGE_DIAG_JS),
            ],
        )
    )
}

/// The in-page HTML->Markdown converter (the probe uses the same copy)
pub fn markdown_js() -> String {
    MARKDOWN_JS.to_string()
}

pub fn compose_js(site: &Site, turn: &Turn) -> String {
    fill(
        COMPOSE_JS,
        &[
            ("__COMPOSER__", site.composer),
            ("__ANSWER__", site.answer),
            ("__THINK__", site.think.unwrap_or(".snapi-no-think")),
            ("__TAG__", TAG),
            ("__ARGS__", &turn.args().to_string()),
            ("__STATE_SHAPE__", STATE_SHAPE_JS),
            (
                "__COMPOSER_IS_TA__",
                if site.composer_is_textarea { "true" } else { "false" },
            ),
        ],
    )
}

pub fn send_js(site: &Site) -> String {
    fill(SEND_JS, &[("__SEND__", site.send), ("__TAG__", TAG)])
}

/// Start a new conversation: prefer clicking the sidebar button; fall back to
/// reloading the home page when it can't be clicked (or the site has none).
pub fn new_chat_js(site: &Site) -> String {
    if site.new_chat_text.is_empty() {
        // This site has no "new chat" button recognizable by text: go back to
        // the home page (page reload; slower but always correct)
        return format!(
            "document.title = '{}' + JSON.stringify({{ newchat: 'fallback' }}); location.href = '{}';",
            TAG,
            site.home.replace(char::from(92), "\\").replace(char::from(39), "\'"),
        );
    }
    fill(
        NEW_CHAT_JS,
        &[
            ("__TEXT__", site.new_chat_text),
            ("__TAG__", TAG),
            ("__HOME__", site.home),
        ],
    )
}

/// Have the page stop generating (used when the user presses cancel)
pub fn stop_js() -> String {
    STOP_JS.to_string()
}

/// Turn "deep thinking" on (used when starting a new chat). Reports
/// `{ think: on|already|missing }`. Only sites with a configured toggle
/// (deepseek) have this step.
pub fn think_on_js(site: &Site) -> String {
    let (toggle, marker) = site.think_toggle.unwrap_or((".snapi-no-toggle", "snapi-none"));
    fill(
        THINK_ON_JS,
        &[("__TOGGLE__", toggle), ("__MARKER__", marker), ("__TAG__", TAG)],
    )
}

/// Extract the payload from a title. Anything without the prefix is noise
/// (the site changes the title too) and gets dropped.
pub fn parse_signal(title: &str) -> Option<serde_json::Value> {
    serde_json::from_str(title.strip_prefix(TAG)?).ok()
}

/// What one title payload contains.
///
/// These field names are the contract between Rust and the in-page
/// `COLLECT_JS` — if either side gets one wrong, the symptom is "stuck at
/// analyzing forever, not a word comes out". So they get their own struct,
/// watched by tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Signal {
    /// Body increment (or the first chunk of a full re-send, see `rst`)
    pub text: String,
    /// Chain-of-thought increment (a separate stream from the body; mixed
    /// together the panel can't display them apart)
    pub reasoning: String,
    /// Body not fully pushed yet; receive another round immediately
    pub more: bool,
    /// Chain of thought not fully pushed
    pub rmore: bool,
    /// This chunk is the head of a "full re-send": open a new buffer on
    /// receipt, don't append to the old content
    pub rst: bool,
    /// Full re-send of the chain of thought
    pub rrst: bool,
}

/// Read a `Signal` out of a title payload, surfacing "the page reported an error".
pub fn read_signal(value: &serde_json::Value) -> Result<Signal, String> {
    if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
        return Err(format!("The web UI reported an error: {err}"));
    }
    let text = value.get("d").and_then(|v| v.as_str()).unwrap_or("");
    let reasoning = value.get("dr").and_then(|v| v.as_str()).unwrap_or("");
    let flag = |key: &str| value.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(Signal {
        text: text.to_string(),
        reasoning: reasoning.to_string(),
        more: flag("more"),
        rmore: flag("rmore"),
        rst: flag("rst"),
        rrst: flag("rrst"),
    })
}

/// Assemble one stream (body or chain of thought).
///
/// The page can push either increments or full re-sends (the site redraws the
/// span it just output, see `COLLECT_JS`), so this must accept "start over
/// from the top": on `rst` open a new buffer, and only when `more` turns false
/// treat it as the stream's new content.
///
/// It is a separate little structure because "when to append, when to replace"
/// is the easiest thing to get wrong and can't be auto-tested in a real
/// window — here it can be watched by plain unit tests.
#[derive(Default)]
struct Assembler {
    /// Content confirmed so far
    done: String,
    /// Full re-send currently being received
    snap: Option<String>,
}

impl Assembler {
    /// Receive one payload. Returns "the increment to append" and "the content
    /// to replace everything with" (mutually exclusive, both may be empty).
    fn push(&mut self, delta: &str, rst: bool, more: bool) -> (String, Option<String>) {
        if rst {
            // The previous span was cut off mid-way: a full re-send means
            // "latest wins"
            self.snap = Some(String::new());
        }
        if let Some(buf) = self.snap.as_mut() {
            buf.push_str(delta);
            if !more {
                let text = self.snap.take().unwrap_or_default();
                self.done = text.clone();
                return (String::new(), Some(text));
            }
            return (String::new(), None);
        }
        self.done.push_str(delta);
        (delta.to_string(), None)
    }

    fn text(&self) -> &str {
        &self.done
    }
}

/// What to report outward after receiving a payload.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// Body increment, append
    Delta(String),
    /// Chain-of-thought increment, append
    Reasoning(String),
    /// Full replacement: the site redrew part of the body mid-stream, this is
    /// the complete re-sent content
    Replace {
        answer: Option<String>,
        reasoning: Option<String>,
    },
}


/// Turn "deep thinking" on and wait for the page's report (up to 1.2s; treat
/// nothing received as failure).
///
/// The wait matters: moving on right after the click means React's state
/// hasn't landed yet, and the immediately following compose/send has a small
/// chance of racing ahead of the switch. The report also goes to the log, so
/// when a site redesigns you can see at a glance whether it is "missing"
/// (the toggle is gone).
async fn enable_thinking(
    site: &Site,
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> String {
    if win.eval(think_on_js(site)).is_err() {
        return "eval failed".into();
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(1200);
    let mut state = String::from("no report");
    while std::time::Instant::now() < deadline {
        match signal.try_recv() {
            Ok(payload) => {
                if let Some(value) = parse_signal(&payload) {
                    if let Some(s) = value.get("think").and_then(|v| v.as_str()) {
                        state = s.to_string();
                        break;
                    }
                }
                // Other payloads (e.g. a leftover ready) can just be dropped
            }
            Err(_) => sleep(Duration::from_millis(80)).await,
        }
    }
    if state.starts_with("on") {
        // There is a React state update between the click and the site actually
        // switching; give it a bit of time
        sleep(Duration::from_millis(300)).await;
    }
    state
}


/// Run one turn: paste images + ask + read the answer back, handing increments
/// out through `on_event`.
///
/// Returns the full answer and the conversation URL the site is on now (the
/// caller stores it in the record and deep-links straight there next time).
/// Same contract as `api::stream_chat_with`; the caller writes it into history
/// directly.
pub async fn run_turn<F>(
    app: &AppHandle,
    timeout_secs: u64,
    site: &'static Site,
    account: &crate::config::WebchatAccount,
    turn: Turn,
    cancel: &crate::api::Cancel,
    mut on_event: F,
) -> Result<TurnOutcome, String>
where
    F: FnMut(Out) + Send,
{
    let win = crate::ensure_webchat(app, false, site, account)?;
    let mut signal = take_signal(app);

    // 1. Wait for the page to be ready (the window may have just been created,
    //    the page still loading)
    wait_ready(&win, &mut signal, site, timeout_secs, cancel).await?;

    // 2. The record holds the site conversation for this chat: deep-link back
    //    and continue. The site keeps the context itself; previous turns don't
    //    need re-sending, and the "deep thinking" toggle is left alone — the
    //    conversation keeps its own mode.
    if let Some(url) = &turn.resume {
        let _ = win.eval(format!(
            "location.href = '{}'",
            url.replace('\\', "\\\\").replace('\'', "\\'")
        ));
        // The page reloads: moving on immediately means the old document races
        // to report "ready" (measured: as early as 0.6s), and compose/send all
        // land on a page that is reloading. Let the old document die first.
        if nap(SETTLE_AFTER_NEW_CHAT, cancel).await {
            drop_signal(app);
            return Err(CANCELLED_MSG.into());
        }
        wait_ready(&win, &mut signal, site, timeout_secs, cancel).await?;
        if cancel.is_cancelled() {
            drop_signal(app);
            return Err(CANCELLED_MSG.into());
        }
        // Whether old messages render stably and old content drains cleanly is
        // left to page_baseline before asking (it first waits for the counts to
        // settle, then drains the old content).
    }

    // 3. If a new conversation is requested, start one first
    //    Before clicking, record which conversation the site is on: partly to
    //    confirm "it really switched away", partly as a safety net at the end —
    //    if the button was clicked but the site didn't move, this round was
    //    sent into the old conversation, and the old URL must never be stored
    //    for the new one (otherwise this chat permanently deep-links into the
    //    old conversation, mixing all context).
    let mut url_before = String::new();
    if turn.new_chat {
        url_before = current_chat_url(&win, &mut signal, site).await;
        crate::trace(&format!("webchat: starting a new chat, site URL before asking = {url_before:?}"));
        let _ = win.eval(new_chat_js(site));
        if nap(Duration::from_millis(600), cancel).await {
            drop_signal(app);
            return Err(CANCELLED_MSG.into());
        }

        // When the button can't be clicked the fallback goes to the home page,
        // which reloads — readiness has to be awaited again before moving on.
        // The report may arrive later than the 600ms nap, and a one-shot drain
        // would miss it (and then compose/send land on the reloading page), so
        // wait for it with a deadline.
        let mut reloaded = false;
        let fallback_deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while std::time::Instant::now() < fallback_deadline {
            match signal.try_recv() {
                Ok(payload) => {
                    if let Some(value) = parse_signal(&payload) {
                        if value.get("newchat").and_then(|v| v.as_str()) == Some("fallback") {
                            reloaded = true;
                            break;
                        }
                    }
                }
                Err(_) => {
                    if nap(POLL, cancel).await {
                        drop_signal(app);
                        return Err(CANCELLED_MSG.into());
                    }
                }
            }
        }
        // Don't fully trust a clicked button either: confirm the URL actually
        // left the old conversation (into a new one or back home). If it can't
        // be confirmed, treat it as not switched and go back to the home page —
        // slower but always correct; never send the question into the old
        // conversation.
        if !reloaded && !confirm_left_old_chat(&win, &mut signal, site, &url_before, cancel).await {
            crate::trace("webchat: clicked new chat but the URL didn't change, treating as failed; reopening from the home page");
            let home = site.home.replace('\\', "\\\\").replace('\'', "\'");
            let _ = win.eval(format!("location.href = '{home}'"));
            reloaded = true;
        }
        if reloaded {
            // Going home is a real page reload. Waiting for readiness
            // immediately means the **old document** races to report "ready"
            // before it dies (measured: as early as 0.6s), and then compose/send
            // land on the reloading page — whether anything was sent is up to
            // luck. Let the old document die first.
            if nap(SETTLE_AFTER_NEW_CHAT, cancel).await {
                drop_signal(app);
                return Err(CANCELLED_MSG.into());
            }
            wait_ready(&win, &mut signal, site, timeout_secs, cancel).await?;
            if cancel.is_cancelled() {
                drop_signal(app);
                return Err(CANCELLED_MSG.into());
            }
        }

        // 2.5 Turn "deep thinking" on. In conversations opened by the driver
        // this toggle defaults to off (measured: manually-started chats have
        // thinking blocks, driver-opened ones don't), and without it the model
        // won't think at all. Only sites with a configured toggle (deepseek)
        // do this step; the others keep the site's own default. Continuation
        // turns skip it: that conversation keeps its own mode — don't override
        // the user's choice.
        if site.think_toggle.is_some() {
            let mut think_state = enable_thinking(site, &win, &mut signal).await;
            if !think_state.starts_with("on") && !think_state.starts_with("already") {
                // Right after switching to a new conversation the toggle often
                // hasn't rendered yet (true story: one regenerated conversation
                // had no thinking block at all). Let the page catch its breath,
                // then click again instead of letting it slip.
                crate::trace(&format!("webchat: deep-thinking toggle -> {think_state}, retrying once after the page settles"));
                if nap(SETTLE_AFTER_NEW_CHAT, cancel).await {
                    drop_signal(app);
                    return Err(CANCELLED_MSG.into());
                }
                think_state = enable_thinking(site, &win, &mut signal).await;
            }
            crate::trace(&format!("webchat: deep-thinking toggle -> {think_state}"));
        }
    }

    // 4. For this final round (the question shown to the user): paste images +
    //    ask + send. The baseline is recorded before asking (same reason as in
    //    the replay section above).
    let mut base = page_baseline(&win, &mut signal, site, cancel).await;
    if cancel.is_cancelled() {
        drop_signal(app);
        return Err(CANCELLED_MSG.into());
    }
    // The last gate (DOM-level, independent of the URL): if this round asked
    // for a new conversation, no old answer should remain on the page. If some
    // remain, the whole "new chat" sequence (button click, URL confirm, home
    // fallback) failed to take effect. Self-heal first: a full reload via the
    // home page (a reloaded DOM is guaranteed fresh), wait for readiness, click
    // "new chat" again, re-record the baseline — at most twice; once clean,
    // ask as usual without disturbing the user. Only report an error if it
    // never gets clean (sending would cross-contaminate conversations; better
    // not to send).
    let mut selfheal = 0;
    while turn.new_chat && (base.answers > 0 || base.thinks > 0) {
        if selfheal >= 2 {
            drop_signal(app);
            crate::trace(&format!(
                "webchat: new chat requested but the page keeps old content ({} answers, {} thinking blocks); refusing to ask",
                base.answers, base.thinks
            ));
            return Err(
                "The web UI could not start a new conversation (old answers kept staying on the page), so this round was not sent. \
Click \"Show page\" on the panel to check manually, then re-select the region and ask again."
                    .into(),
            );
        }
        selfheal += 1;
        crate::trace(&format!(
            "webchat: new-chat page still holds old content ({} answers, {} thinking blocks), full reload via the home page (self-heal #{selfheal})",
            base.answers, base.thinks
        ));
        let home = site.home.replace('\\', "\\\\").replace('\'', "\'");
        let _ = win.eval(format!("location.href = '{home}'"));
        if nap(SETTLE_AFTER_NEW_CHAT, cancel).await {
            drop_signal(app);
            return Err(CANCELLED_MSG.into());
        }
        wait_ready(&win, &mut signal, site, timeout_secs, cancel).await?;
        if cancel.is_cancelled() {
            drop_signal(app);
            return Err(CANCELLED_MSG.into());
        }
        // After returning home, click "new chat" once more: some sites auto-restore
        // the last conversation on the home page — don't rely on their default page.
        let _ = win.eval(new_chat_js(site));
        if nap(Duration::from_millis(600), cancel).await {
            drop_signal(app);
            return Err(CANCELLED_MSG.into());
        }
        base = page_baseline(&win, &mut signal, site, cancel).await;
        if cancel.is_cancelled() {
            drop_signal(app);
            return Err(CANCELLED_MSG.into());
        }
    }
    ask_page(site, &win, &turn, crate::cdp::cdp_port(&account.name), cancel).await?;
    if cancel.is_cancelled() {
        drop_signal(app);
        return Err(CANCELLED_MSG.into());
    }

    // 5. Poll and push the answer to the panel as it streams
    let (mut answer, mut thinking) = read_stream(
        &win,
        &mut signal,
        cancel,
        timeout_secs,
        true,
        base,
        site,
        &mut on_event,
    )
    .await?;

    // 6. Final re-read: compare the body on the page against the copy in hand
    //    by length; on mismatch re-collect the whole thing. The title channel
    //    can drop a payload (adjacent title changes get merged), and one loss
    //    on the increment path is a permanent gap — the re-read guarantees what
    //    is stored matches the page exactly.
    if !cancel.is_cancelled() {
        flush_from_page(
            site,
            &win,
            &mut signal,
            cancel,
            &mut answer,
            &mut thinking,
            base,
            &mut on_event,
        )
        .await?;
    }

    if answer.text().trim().is_empty() {
        drop_signal(app);
        if cancel.is_cancelled() {
            return Err("Cancelled; no content was received this time.".into());
        }
        return Err("The web UI produced no answer text. Click \"Show page\" on the panel to see what is going on over there.".into());
    }
    // 7. Ask which conversation the site is on now. Once stored in the record,
    //    follow-ups, regeneration and "continue from context" can all deep-link
    //    straight there without re-sending previous turns. Must be asked
    //    **before** drop_signal: once the channel is closed nobody receives the
    //    page's reports (measured: with the order flipped, webchat_url was
    //    always empty and "resume" never worked).
    let mut site_url = current_chat_url(&win, &mut signal, site).await;
    // Safety net: if this round opened a "new chat" but the URL at the end
    // still sits on the pre-click conversation, the site never moved and the
    // round was actually sent into the old conversation. The old URL must never
    // be stored for the new chat (otherwise every later round deep-links into
    // the old conversation and the two merge permanently); better to leave the
    // record without a deep link.
    if turn.new_chat && !url_before.is_empty() && site_url == url_before {
        crate::trace("webchat: the URL still sits on the old conversation at the end of the round; not storing it for the new chat");
        site_url.clear();
    }
    drop_signal(app);
    Ok(TurnOutcome {
        answer: answer.text().to_string(),
        site_url,
    })
}

/// The conversation URL the site currently has open (`/a/chat/s/<id>`).
/// Empty when not in any conversation.
async fn current_chat_url(
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    site: &Site,
) -> String {
    const URL_JS: &str = r#"document.title = '__TAG__' + JSON.stringify({ url: location.href });"#;
    if win.eval(URL_JS.replace("__TAG__", TAG).as_str()).is_err() {
        return String::new();
    }
    sleep(POLL).await;
    while let Ok(payload) = signal.try_recv() {
        if let Some(value) = parse_signal(&payload) {
            if let Some(url) = value.get("url").and_then(|v| v.as_str()) {
                if url.contains(site.chat_url_marker) {
                    return url.to_string();
                }
            }
        }
    }
    String::new()
}

/// After clicking "new chat", confirm the old conversation was really left:
/// poll the URL until it differs from `url_before` (a new conversation, or back
/// home) or time out. When `url_before` is empty (previously in no
/// conversation) there is nothing to compare; treat as success. On cancel,
/// return as "not switched" — the caller runs its own cancel branch.
async fn confirm_left_old_chat(
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    site: &Site,
    url_before: &str,
    cancel: &crate::api::Cancel,
) -> bool {
    if url_before.is_empty() {
        return true;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let url = current_chat_url(win, signal, site).await;
        if url != url_before {
            return true;
        }
        if nap(POLL, cancel).await {
            return false;
        }
    }
    false
}

/// Synthetic-input fallback when CDP is unavailable: focus + selection +
/// execCommand, then paste, then textContent.
fn typing_js(site: &Site, prompt: &str) -> String {
    let text = serde_json::to_string(prompt).unwrap_or_else(|_| String::from("\"\""));
    format!(
        r#"(function() {{
  var composer = document.querySelector("__COMPOSER__");
  if (!composer) return;
  composer.focus();
  try {{
    var range = document.createRange();
    range.selectNodeContents(composer);
    range.collapse(false);
    var sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }} catch (e) {{}}
  var typed = false;
  try {{ typed = document.execCommand('insertText', false, {text}); }} catch (e) {{}}
  if (!typed && !(composer.textContent || '').trim() && !composer.value) {{
    try {{
      var dt = new DataTransfer();
      dt.setData('text/plain', {text});
      composer.dispatchEvent(new ClipboardEvent('paste', {{ clipboardData: dt, bubbles: true, cancelable: true }}));
      typed = (composer.textContent || '').trim().length > 0 || !!composer.value;
    }} catch (e) {{}}
  }}
  if (!typed && !(composer.textContent || '').trim() && !composer.value) {{
    composer.textContent = {text};
    composer.dispatchEvent(new Event('input', {{ bubbles: true }}));
  }}
}})()"#
    )
    .replace("__COMPOSER__", site.composer)
}

/// "Typing + sending" for contenteditable sites: CDP real input (focus ->
/// insertText -> Enter). Enter-to-send is the shared interaction of all three.
/// On failure the caller falls back to the synthetic-event path.
/// A cancel-friendly sleep: sleep in small steps and wake up early once the
/// cancel flag is set. Returns true = cancelled.
/// Cancel used to be checked only in the "read the answer" section — the
/// pre-ask preparation (wait ready / wait page settle / image settle) can
/// block up to 10 seconds, so pressing cancel looked like nothing happened
/// (measured). Every section now checks the flag.
async fn nap(dur: Duration, cancel: &crate::api::Cancel) -> bool {
    const STEP: Duration = Duration::from_millis(60);
    let mut left = dur;
    while left > Duration::ZERO {
        if cancel.is_cancelled() {
            return true;
        }
        let d = left.min(STEP);
        sleep(d).await;
        left -= d;
    }
    cancel.is_cancelled()
}

/// The standard reply to the panel when cancelled
const CANCELLED_MSG: &str = "Cancelled; no content was received this time.";

pub fn cdp_type_and_send(
    site: &Site,
    win: &tauri::WebviewWindow,
    port: u16,
    prompt: &str,
) -> Result<(), String> {
    let escaped = site
        .composer
        .replace(char::from(92), "\\")
        .replace('"', "\"");
    let focus = "(function(){var c=document.querySelector(\"@@\");if(c)c.focus();})()"
        .replace("@@", &escaped);
    win.eval(&focus).map_err(|e| format!("Failed to focus the composer: {e}"))?;
    // Host fragment: carve xxx out of https://xxx/... to avoid hitting the
    // wrong page when the port is taken by another browser process
    let host = site
        .home
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    let mut cdp = crate::cdp::Cdp::connect(port, host)?;
    cdp.insert_text(prompt)
        .map_err(|e| format!("CDP typing failed: {e}"))?;
    cdp.press_enter()
        .map_err(|e| format!("CDP Enter failed: {e}"))
}

/// Write this turn's images to local temp files (CDP native file setting only
/// accepts **file paths**, not bytes). Images in Turn are base64 (possibly
/// with a data: prefix); strip the prefix and decode. The extension follows the
/// real format: the screenshot pipeline encodes JPEG, and hardcoding .png
/// makes the browser upload with an image/png MIME based on the extension —
/// the site fails to decode and drops the image (measured).
fn write_temp_images(images: &[String]) -> Result<Vec<String>, String> {
    use base64::Engine as _;
    let mut paths = Vec::new();
    for (i, img) in images.iter().enumerate() {
        let b64 = img
            .split_once("base64,")
            .map(|(_, rest)| rest)
            .unwrap_or(img);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| format!("Failed to decode the screenshot base64: {e}"))?;
        let is_png = bytes.starts_with(&[0x89, b'P', b'N', b'G']);
        let path = std::env::temp_dir().join(format!(
            "askwithsnip-img-{}-{i}.{}",
            std::process::id(),
            if is_png { "png" } else { "jpg" }
        ));
        std::fs::write(&path, &bytes).map_err(|e| format!("Failed to write the temp file: {e}"))?;
        paths.push(path.to_string_lossy().into_owned());
    }
    Ok(paths)
}

/// The CDP pipeline: connect to the debug port -> if images weren't pasted,
/// set them **natively** as files -> real typing -> Enter to send.
///
/// Both JS image paths (synthetic paste / file-input assignment) are ignored by
/// chatgpt — measured: its React layer only accepts its own upload pipeline.
/// CDP's DOM.setFileInputFiles sets files natively in the browser, the same
/// path as a real user upload, which the site cannot block. Paste (measured
/// working on gemini/claude) has priority; CDP file setting kicks in only when
/// imgPath in the page state isn't 'paste'.
pub fn cdp_deliver(
    site: &Site,
    win: &tauri::WebviewWindow,
    port: u16,
    turn: &Turn,
    cancel: &crate::api::Cancel,
) -> Result<(), String> {
    /// Synchronous small-step sleep: returns true early once cancel is set
    fn nap_sync(dur: Duration, cancel: &crate::api::Cancel) -> bool {
        let step = Duration::from_millis(80);
        let mut left = dur;
        while left > Duration::ZERO {
            if cancel.is_cancelled() {
                return true;
            }
            let d = left.min(step);
            std::thread::sleep(d);
            left -= d;
        }
        cancel.is_cancelled()
    }
    let escaped = site
        .composer
        .replace(char::from(92), "\\")
        .replace('"', "\"");
    let focus = "(function(){var c=document.querySelector(\"@@\");if(c)c.focus();})()"
        .replace("@@", &escaped);
    win.eval(&focus).map_err(|e| format!("Failed to focus the composer: {e}"))?;
    // Host fragment: carve xxx out of https://xxx/... to avoid hitting the
    // wrong page when the port is taken by another browser process
    let host = site
        .home
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    let mut cdp = crate::cdp::Cdp::connect(port, host)?;
    if !turn.images.is_empty() {
        let state = cdp
            .evaluate("(function(){return (window.__snapi && window.__snapi.imgPath) || 'none';})()")
            .map_err(|e| format!("Failed to recheck the image-paste state: {e}"))?;
        let pasted = state.get("value").and_then(|v| v.as_str()) == Some("paste");
        if !pasted {
            // Route 1: the real upload flow (file chooser interception +
            // input.click() + native file setting). chatgpt's composer is now a
            // TEXTAREA (it doesn't accept pasted images); its own upload
            // pipeline is the only reliable entry. Fall back to clipboard
            // Ctrl+V if that fails.
            use base64::Engine as _;
            let files = write_temp_images(&turn.images)?;
            // Multi-step real clicks: first click the "add files" button
            // (chatgpt: composer-plus-btn); when the final popup is the file
            // chooser it gets intercepted and the files are set natively.
            // Fallback step: click the hidden file input itself directly
            // (some sites need no menu).
            const RECT: &str = "(function(sel){var el=document.querySelector(sel);if(!el)return '';var r=el.getBoundingClientRect();if(r.width<2||r.height<2)return '';return JSON.stringify([r.x+r.width/2,r.y+r.height/2]);})";
            // Menu items found by text (上传/Upload/附件...), take the last
            // visible one (menus render top-down).
            // (上传/附件/照片/文件/图片 match the Chinese menu labels — keep.)
            const RECT_ANY: &str = "(function(sel){var el=document.querySelector(sel);if(!el)return '';var r=el.getBoundingClientRect();return JSON.stringify([r.x+r.width/2,r.y+r.height/2]);})";
            const MENU: &str = "(function(){var els=[...document.querySelectorAll('[role=menuitem],[role=option],button,li,div')].filter(function(e){var r=e.getBoundingClientRect();if(r.width<2)return false;var t=(e.innerText||'').trim();return t.length<20&&(t.indexOf('上传')>=0||t.indexOf('附件')>=0||t.indexOf('照片')>=0||t.indexOf('文件')>=0||t.indexOf('图片')>=0||/^(upload|attach|add\\s*(photos|files|media))/i.test(t)||(t.indexOf('file')>=0&&/^add/i.test(t)));});if(!els.length)return '';var r=els[els.length-1].getBoundingClientRect();return JSON.stringify([r.x+r.width/2,r.y+r.height/2]);})";
            let steps = vec![
                // chatgpt's add-button aria-label varies by language/version
                // ("添加文件", "Add files and more"...), so keep a data-testid
                // too (composer-plus-btn is its own).
                // The Rect must filter out invisible ones: with the composer
                // collapsed the button isn't in the render tree at all.
                format!("{}(concat('button#composer-plus-btn', ', ', 'button[data-testid=\"composer-plus-btn\"]', ', ', 'button[aria-label*=\"添加文件\"]', ', ', 'button[aria-label*=\"Add files\"]', ', ', 'button[aria-label*=\"Upload\"]', ', ', 'button[aria-label*=\"Attach\"]'))", RECT),
                MENU.to_string(),
                format!("{}('input[type=file][accept*=image]')", RECT_ANY),
            ];
            match cdp.upload_via_file_chooser(&steps, &files) {
                Ok(()) => crate::trace("webchat: image attached via native file-chooser upload"),
                Err(chooser_err) => {
                    crate::trace(&format!("webchat: file-chooser upload failed ({chooser_err}), falling back to clipboard Ctrl+V"));
                    // Route 2: clipboard + real Ctrl+V, verify one image at a
                    // time (blob: preview), at most 3 attempts
                    const HAS_BLOB: &str =
                        "(function(){return !!document.querySelector('[src^=\"blob:\"],[style*=\"blob:\"]');})()";
                    for img in &turn.images {
                        let b64 = img
                            .split_once("base64,")
                            .map(|(_, rest)| rest)
                            .unwrap_or(img);
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b64.trim())
                            .map_err(|e| format!("Failed to decode the screenshot base64: {e}"))?;
                        let mut attached = false;
                        for _ in 0..3 {
                            crate::cdp::Cdp::clipboard_set_image(&bytes)
                                .map_err(|e| format!("Failed to put the image on the clipboard: {e}"))?;
                            if let Err(e) = cdp.press_ctrl_v() {
                                return Err(format!("Ctrl+V failed: {e}"));
                            }
                            if nap_sync(Duration::from_millis(1500), cancel) {
                                return Err("Cancelled".into());
                            }
                            match cdp.evaluate(HAS_BLOB) {
                                Ok(v) if v.get("value").and_then(|x| x.as_bool()) == Some(true) => {
                                    attached = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        if !attached {
                            // If the image won't attach, don't force-send —
                            // sending produces a message without the image,
                            // more misleading than not sending
                            return Err(format!("Could not paste the image into the web UI (selector upload failed: {chooser_err}; native paste also failed after 3 attempts)"));
                        }
                    }
                }
            }
            // Give the page time to finish uploading the attachments (sending
            // before the upload completes drops the images)
            if nap_sync(Duration::from_secs(3), cancel) {
                return Err("Cancelled".into());
            }
        }
    }
    cdp.insert_text(&turn.prompt)
        .map_err(|e| format!("CDP typing failed: {e}"))?;
    cdp.press_enter()
        .map_err(|e| format!("CDP Enter failed: {e}"))
}

/// Hand one round to the page: paste images + type (not sending yet), wait for
/// the images to be accepted, then click send.
async fn ask_page(
    site: &Site,
    win: &tauri::WebviewWindow,
    turn: &Turn,
    port: u16,
    cancel: &crate::api::Cancel,
) -> Result<(), String> {
    win.eval(compose_js(site, turn))
        .map_err(|e| format!("Failed to hand the question to the web UI: {e}"))?;
    // With images, wait a bit longer: the page must accept them first so the
    // send carries the attachments
    if nap(
        if !turn.images.is_empty() { ATTACH_SETTLE } else { Duration::from_millis(250) },
        cancel,
    )
    .await
    {
        return Err("Cancelled".into());
    }
    if site.composer_is_textarea {
        // deepseek: text already typed by compose; click the send button
        win.eval(send_js(site))
            .map_err(|e| format!("Failed to click send: {e}"))?;
        return Ok(());
    }
    // The contenteditable three (chatgpt/gemini/claude): the CDP real pipeline
    // (paste + type + Enter to send). Synthetic events are rolled back/ignored
    // by their editors (measured); only this pipeline gets in.
    if let Err(err) = cdp_deliver(site, win, port, turn, cancel) {
        crate::trace(&format!("webchat: CDP input failed, falling back to synthetic events: {err}"));
        win.eval(typing_js(site, &turn.prompt))
            .map_err(|e| format!("Fallback typing failed: {e}"))?;
        sleep(Duration::from_millis(200)).await;
        win.eval(send_js(site))
            .map_err(|e| format!("Failed to click send: {e}"))?;
    }
    Ok(())
}

/// What the page reported during this round. Written into the error message on
/// timeout — last time "no answer ever came" while the site had long finished
/// generating, and the panel only said "you may need to log in", with no clue
/// at all. These few numbers next time explain immediately: did the page not
/// render, or did we not receive.
#[derive(Default)]
struct PageReport {
    /// How many payloads were received
    payloads: u32,
    /// Reported by the page: answer-node count, the body length it read, and
    /// whether the composer is present
    answers: u64,
    thinks: u64,
    chars: u64,
    ready: bool,
    /// The failure reason reported by the collection script (the `err` field,
    /// not the round-failing `error`). This really happened: the converter
    /// threw on a piece of content, the whole collect died before reading the
    /// body every time, no payload got out, and the panel timed out with no
    /// idea why.
    err: String,
    /// How many times the converter itself fell back to plain text (`mderr`):
    /// not fatal, but worth counting
    md_errors: u32,
    /// The page's "still generating" report (whether a "stop" button exists).
    /// The basis for judging "answered": deep thinking can pause over 1.5s
    /// between breaths, and judging by "no change" alone would declare a
    /// still-thinking round finished (measured: "no body at all" reported while
    /// not a word had arrived).
    generating: bool,
    /// The page's own title and the start of its body: on timeout, tell the
    /// user directly what the page shows (login page / Cloudflare challenge /
    /// blank) instead of letting them guess.
    page_title: String,
    page_body: String,
}

impl PageReport {
    fn note(&mut self, value: &serde_json::Value) {
        self.payloads += 1;
        self.answers = value.get("answers").and_then(|v| v.as_u64()).unwrap_or(self.answers);
        self.thinks = value.get("thinks").and_then(|v| v.as_u64()).unwrap_or(self.thinks);
        self.chars = value.get("chars").and_then(|v| v.as_u64()).unwrap_or(self.chars);
        self.ready = value.get("ready").and_then(|v| v.as_bool()).unwrap_or(self.ready);
        if let Some(err) = value.get("err").and_then(|v| v.as_str()) {
            self.err = err.to_string();
        }
        if value.get("mderr").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
            self.md_errors += 1;
        }
        if let Some(gen) = value.get("gen").and_then(|v| v.as_bool()) {
            self.generating = gen;
        }
        if let Some(pt) = value.get("pt").and_then(|v| v.as_str()) {
            if !pt.is_empty() {
                self.page_title = pt.to_string();
            }
        }
        if let Some(bx) = value.get("bx").and_then(|v| v.as_str()) {
            if !bx.is_empty() {
                self.page_body = bx.to_string();
            }
        }
    }

    fn baseline(&self) -> Baseline {
        Baseline {
            answers: self.answers,
            thinks: self.thinks,
        }
    }

    fn summary(&self) -> String {
        let mut text = format!(
            "received {} payloads; the page last said: composer {}, {} answers, {} chars of body",
            self.payloads,
            if self.ready { "present" } else { "absent" },
            self.answers,
            self.chars
        );
        if !self.err.is_empty() {
            text += &format!("; collection script error: {}", self.err);
        }
        if self.generating {
            text += "; the page is still generating (a stop button is present)";
        }
        if self.md_errors > 0 {
            text += &format!("; body conversion fell back to plain text {} time(s)", self.md_errors);
        }
        if !self.page_title.is_empty() || !self.page_body.is_empty() {
            text += &format!(
                "; the page shows \"{}\" starting with: {}",
                self.page_title, self.page_body
            );
        }
        text
    }
}

/// How many answers / thinking blocks were on the page before asking.
///
/// The "full re-send" fallback needs this number: when no new answer node has
/// appeared, the body in hand is likely **the previous round's** leftover, and
/// pushing it wholesale creates "new question with old answer" — harder to
/// notice than not reading anything.
#[derive(Clone, Copy, Default)]
pub struct Baseline {
    answers: u64,
    thinks: u64,
}

/// Record the baseline before asking.
///
/// Two steps, both indispensable:
///
/// 1. **Wait for the page to settle first**: answer/thinking counts must agree
///    on two consecutive polls. During page switches and re-render gaps the
///    counts transiently drop to zero — asking before it settles records a
///    baseline of 0, and old answers rendering a beat later get collected as
///    this round's answer (measured: three rounds of records all stored the
///    same old answer).
/// 2. **Then drain the old content already accumulated on the page** (a long
///    answer takes several evals to finish pushing; collecting only one round
///    leaks the rest into read_stream). What is drained goes into a
///    discard-purpose assembler.
///
/// If it doesn't settle within 10s, carry on anyway — compose's baseline gate
/// downstream still guards.
async fn page_baseline(
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    site: &Site,
    cancel: &crate::api::Cancel,
) -> Baseline {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut prev: Option<(u64, u64)> = None;
    let mut counts = (0u64, 0u64);
    while std::time::Instant::now() < deadline {
        if cancel.is_cancelled() {
            return Baseline::default();
        }
        let _ = win.eval(collect_js_for(site));
        sleep(POLL).await;
        while let Ok(payload) = signal.try_recv() {
            if let Some(value) = parse_signal(&payload) {
                let answers = value.get("answers").and_then(|v| v.as_u64());
                let thinks = value.get("thinks").and_then(|v| v.as_u64());
                if let (Some(a), Some(t)) = (answers, thinks) {
                    counts = (a, t);
                }
            }
        }
        if prev == Some(counts) {
            break;
        }
        prev = Some(counts);
    }
    if prev.is_none() {
        crate::trace("webchat: page never settled within 10s; asking anyway");
    }

    let mut report = PageReport::default();
    if win.eval(collect_js_for(site)).is_err() {
        return Baseline::default();
    }
    sleep(POLL).await;
    let mut discard_answer = Assembler::default();
    let mut discard_thinking = Assembler::default();
    let mut sink = |_: Out| {};
    let mut got_any = false;
    let _ = drain_signals(
        site,
        win,
        signal,
        &mut discard_answer,
        &mut discard_thinking,
        &mut sink,
        &mut got_any,
        &mut report,
    );
    crate::trace(&format!(
        "webchat: baseline before asking — {} answers, {} thinking blocks (old page content drained)",
        report.answers, report.thinks
    ));
    report.baseline()
}

/// How long without any increment before proactively re-checking (see the
/// "fallback re-send" section below)
const QUIET_BEFORE_RETRY: Duration = Duration::from_secs(10);

/// The "no change" threshold while the body hasn't started. Deep thinking
/// pauses over 1.5s between breaths all the time (measured: after the chain of
/// thought ran for a while it paused 2s and the panel reported "no body at
/// all" while the page was writing away), so when not a word of body has
/// arrived, don't give up after 1.5s — wait this long, or until the page
/// reports "no longer generating".
const LONG_STABLE_POLLS: u32 = 120;

/// Wait for the page to finish answering this round, handing increments to
/// `on_event` while waiting (when `keep` is false, only wait, don't report).
///
/// Completion relies on "no change for several consecutive rounds", not on any
/// end marker — the page has no reliable end signal. `base` is the
/// answer/thinking count on the page before asking, used only by the fallback
/// re-send (see `Baseline`).
async fn read_stream<F>(
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    cancel: &crate::api::Cancel,
    timeout_secs: u64,
    keep: bool,
    base: Baseline,
    site: &Site,
    on_event: &mut F,
) -> Result<(Assembler, Assembler), String>
where
    F: FnMut(Out),
{
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs.clamp(10, 600));
    let mut answer = Assembler::default();
    let mut thinking = Assembler::default();
    let mut stable = 0u32;
    let mut started = false;
    let mut report = PageReport::default();
    let mut last_change = std::time::Instant::now();
    // When the body last "moved". The fallback re-send uses this, not
    // last_change: the chain of thought flowing while the body never comes is
    // normal (the model thinks before speaking), and the chain's activity must
    // not stand in for the body's.
    let mut last_answer_change = std::time::Instant::now();
    let mut retried = 0u32;
    // The non-reporting rounds: hold a discarding callback; everything else is identical
    let mut sink = |_: Out| {};

    loop {
        // The user pressed cancel: stop the page too, then return what was
        // already received as usual
        if cancel.is_cancelled() {
            let _ = win.eval(stop_js());
            break;
        }

        if std::time::Instant::now() > deadline {
            if answer.text().trim().is_empty() {
                let _ = win.eval(stop_js());
                return Err(format!(
                    "Waited {timeout_secs} seconds and the web UI never started answering ({}). \
Click \"Show page\" on the panel to see what is going on over there.",
                    report.summary()
                ));
            }
            break;
        }

        win.eval(collect_js_for(site))
            .map_err(|e| format!("Failed to read the web UI's answer: {e}"))?;

        let answer_len_before = answer.text().len();
        let mut got_any = false;
        let out: &mut dyn FnMut(Out) = if keep { on_event } else { &mut sink };
        drain_signals(site, win, signal, &mut answer, &mut thinking, out, &mut got_any, &mut report)?;

        if answer.text().len() != answer_len_before {
            last_answer_change = std::time::Instant::now();
        }

        if got_any {
            started = true;
            stable = 0;
            last_change = std::time::Instant::now();
        } else if started {
            stable += 1;
            // Whether the body has arrived decides how long "no change" can be
            // trusted: the model thinks before speaking, and a pause in the
            // chain of thought doesn't mean it finished. When the page reports
            // "still generating" (stop button present), don't finish early either.
            let cap = if answer.text().trim().is_empty() {
                LONG_STABLE_POLLS
            } else {
                STABLE_POLLS
            };
            if stable >= cap && (!report.generating || stable >= LONG_STABLE_POLLS) {
                break;
            }
        }

        // When nothing at all arrives for a long time, note what the page says
        // so the user isn't left guessing against "analyzing"
        if last_change.elapsed() >= Duration::from_secs(5) && !started {
            crate::trace(&format!("webchat still waiting: {}", report.summary()));
        }

        // Fallback: the body never got pushed — some step in the middle doesn't
        // match (the site redrew the span it just output wholesale, this round's
        // answer is character-identical to the last one, the title channel
        // dropped a run of payloads, or the collection script itself threw).
        // Don't just wait for the timeout: have the page re-send the whole
        // thing and be received as a "replace". Once per ten seconds, at most
        // three times; if even that doesn't hold, it still times out, but at
        // least it isn't a blind two-minute wait.
        //
        // What counts is the **body's** quiet time, not "any payload": while
        // the chain of thought keeps coming, the old condition never triggers
        // and a lost body is really lost. Don't re-send while the page reports
        // "still generating" (the answer isn't written yet; there's nothing on
        // the page to send) — wait for it to stop. Safety is left to the
        // page-side baseline guard.
        if answer.text().trim().is_empty()
            && !report.generating
            && last_answer_change.elapsed() >= QUIET_BEFORE_RETRY
            && retried < 3
        {
            retried += 1;
            crate::trace(&format!(
                "webchat: no increment for {} seconds ({}), asking the page to re-send everything (attempt #{retried})",
                QUIET_BEFORE_RETRY.as_secs(),
                report.summary()
            ));
            let have = answer.text().encode_utf16().count();
            let have_thinking = thinking.text().encode_utf16().count();
            let _ = win.eval(collect_js_flush(site, have, have_thinking, base));
            last_change = std::time::Instant::now();
            // Reset the body's quiet timer: without the reset, the next loop
            // iteration immediately satisfies the condition again and all three
            // re-sends burn off in the same instant (measured: three log lines
            // crammed into the same millisecond).
            last_answer_change = std::time::Instant::now();
            continue;
        }

        sleep(POLL).await;
    }

    Ok((answer, thinking))
}

/// Final re-read: have the page compare "the length of the copy in hand"
/// against the body on the page, and re-collect the whole thing on mismatch.
async fn flush_from_page<F>(
    site: &Site,
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    cancel: &crate::api::Cancel,
    answer: &mut Assembler,
    thinking: &mut Assembler,
    base: Baseline,
    on_event: &mut F,
) -> Result<(), String>
where
    F: FnMut(Out),
{
    for _ in 0..3 {
        if cancel.is_cancelled() {
            break;
        }
        // Align with the page's `String.length`: that side counts UTF-16 code
        // units, not characters
        let have = answer.text().encode_utf16().count();
        let have_thinking = thinking.text().encode_utf16().count();
        if win.eval(collect_js_flush(site, have, have_thinking, base)).is_err() {
            break;
        }
        // eval is asynchronous: collecting immediately means the page's report
        // hasn't entered the channel yet, and "received nothing" becomes the
        // norm — this check never actually worked (the re-read step in trace
        // carried zero payloads; that was it). Wait one polling interval, then collect.
        sleep(POLL).await;
        let mut got_any = false;
        drain_signals(site, win, signal, answer, thinking, on_event, &mut got_any, &mut PageReport::default())?;
        if !got_any {
            break; // Lengths agree; the page has nothing more to give
        }
    }
    Ok(())
}

/// Drain the title payloads currently queued in the channel, reporting to the
/// panel while draining.
///
/// When the page says "more remains unpushed", collect another round
/// immediately instead of waiting for the next polling interval — an answer of
/// several hundred characters is pushed in many chunks, and waiting for the
/// interval makes it look stuttery.
fn drain_signals(
    site: &Site,
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    answer: &mut Assembler,
    thinking: &mut Assembler,
    on_event: &mut dyn FnMut(Out),
    got_any: &mut bool,
    report: &mut PageReport,
) -> Result<(), String> {
    let mut more = true;
    while more {
        more = false;
        while let Ok(payload) = signal.try_recv() {
            let value = match parse_signal(&payload) {
                Some(value) => value,
                None => continue,
            };
            report.note(&value);
            let sig = read_signal(&value)?;
            crate::trace(&format!(
                "webchat<payload: body +{} chars, chain of thought +{} chars, rst={} rmore={} ({} received in total)",
                sig.text.chars().count(),
                sig.reasoning.chars().count(),
                sig.rst,
                sig.rmore,
                report.payloads
            ));

            let (delta, replace) = answer.push(&sig.text, sig.rst, sig.more);
            let (rdelta, rreplace) = thinking.push(&sig.reasoning, sig.rrst, sig.rmore);
            if !delta.is_empty() {
                on_event(Out::Delta(delta));
                *got_any = true;
            }
            if !rdelta.is_empty() {
                on_event(Out::Reasoning(rdelta));
                *got_any = true;
            }
            if replace.is_some() || rreplace.is_some() {
                // The site redrew a span already pushed out; this is the
                // complete re-sent content: have the panel replace wholesale
                // instead of appending (appending repeats the same passage twice)
                on_event(Out::Replace {
                    answer: replace,
                    reasoning: rreplace,
                });
                *got_any = true;
            }
            if sig.more || sig.rmore {
                more = true;
            }
        }
        if more {
            win.eval(collect_js_for(site)).ok();
        }
    }
    Ok(())
}

/// Wait for the page to render the composer.
async fn wait_ready(
    win: &tauri::WebviewWindow,
    signal: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    site: &Site,
    timeout_secs: u64,
    cancel: &crate::api::Cancel,
) -> Result<(), String> {
    let deadline =
        std::time::Instant::now() + Duration::from_secs(timeout_secs.clamp(5, 30));
    let mut report = PageReport::default();
    while std::time::Instant::now() < deadline {
        // Cancel must win over "waiting for the composer": this loop used to
        // ignore the flag entirely, so pressing cancel during a slow page load
        // (proxied sites, a stuck Cloudflare check) did nothing until the
        // full 30s readiness timeout burned down.
        if cancel.is_cancelled() {
            return Err(CANCELLED_MSG.into());
        }
        win.eval(collect_js_for(site))
            .map_err(|e| format!("Failed to drive the web UI: {e}"))?;
        while let Ok(payload) = signal.try_recv() {
            if let Some(value) = parse_signal(&payload) {
                if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
                    return Err(format!("The web UI reported an error: {err}"));
                }
                if value.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
                    return Ok(());
                }
                report.note(&value);
            }
        }
        if cancel.is_cancelled() {
            return Err(CANCELLED_MSG.into());
        }
        sleep(POLL).await;
    }
    // On timeout, don't make the user guess "maybe a login page": report what
    // the page actually shows, verbatim.
    let what = report.summary();
    crate::trace(&format!("webchat: composer never appeared — {what}"));
    Err(format!(
        "The web UI's composer never appeared. {what}. \
Click \"Show page\" on the panel to take a look: log in first if it asks, \
and if there is a Cloudflare/captcha challenge, pass it manually once, \
then ask again."
    ))
}

/// Bring the window up: the user can see for themselves what is happening over
/// there (login, verification, site redesign).
pub fn reveal(app: &AppHandle, account: &crate::config::WebchatAccount) -> Result<(), String> {
    let site = site_by_id(&account.site);
    let win = crate::ensure_webchat(app, true, site, account)?;
    let _ = win.show();
    let _ = win.set_focus();
    Ok(())
}

/// Swap in a fresh signal channel. Swapped every round: the previous round's
/// leftovers must not leak into this one.
fn take_signal(app: &AppHandle) -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let state = app.state::<crate::AppState>();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *lock(&state.webchat_signal) = Some(tx);
    rx
}

fn drop_signal(app: &AppHandle) {
    let state = app.state::<crate::AppState>();
    *lock(&state.webchat_signal) = None;
}

/// Where title signals land: the `on_document_title_changed` registered at
/// window creation calls in here, which forwards to the current round. With no
/// receiver (no driver running) the payload is dropped.
pub fn on_title(app: &AppHandle, title: &str) {
    if !title.starts_with(TAG) {
        return;
    }
    let state = app.state::<crate::AppState>();
    let guard = lock(&state.webchat_signal);
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(title.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_tagged_signals() {
        let ok = parse_signal(r#"SNAPI|{"i":1,"d":"hello"}"#).expect("should parse");
        assert_eq!(ok["d"], "hello");

        // The site changes the title itself; don't treat it as a signal
        assert!(parse_signal("DeepSeek - exploring the unknown").is_none());
        assert!(parse_signal("SNAPI|not json").is_none());
        assert!(parse_signal("").is_none());
    }

    #[test]
    fn reads_text_and_reasoning_apart() {
        let value = parse_signal(
            r#"SNAPI|{"i":9,"d":"the answer is 4","more":true,"rst":false,"dr":"look at the problem first…","rmore":false,"rrst":true}"#,
        )
        .expect("should parse");
        let sig = read_signal(&value).expect("must not error");
        assert_eq!(
            sig,
            Signal {
                text: "the answer is 4".into(),
                reasoning: "look at the problem first…".into(),
                more: true,
                rmore: false,
                rst: false,
                rrst: true,
            }
        );

        // Empty polls (the page added nothing) must still read; missing fields
        // mean empty/false
        let quiet = read_signal(&parse_signal(r#"SNAPI|{"i":10,"d":"","dr":""}"#).unwrap()).unwrap();
        assert_eq!(quiet, Signal::default());
        assert!(quiet.text.is_empty() && !quiet.more);

        // Page errors must surface, not be treated as "no content" until timeout
        let bad = parse_signal(r#"SNAPI|{"error":"the composer vanished"}"#).unwrap();
        assert!(read_signal(&bad).is_err());
    }

    #[test]
    fn compose_and_collect_agree_on_the_state_shape() {
        // True story: compose built state with the old shape (last/pending...)
        // while collect was rewritten to the new shape (a/r); every collect
        // threw a TypeError on feed(st.a, …) and not a single payload got out —
        // the web page finished generating the answer while the panel stayed on
        // "analyzing" forever, with no clue at all. The shape now has exactly
        // one copy (STATE_SHAPE_JS), referenced by both sides; this test guards
        // against a relapse.
        let site = site_by_id("deepseek");
        let compose = compose_js(site, &Turn {
            prompt: "q".into(),
            images: Vec::new(),
            new_chat: true,
            resume: None,
        });
        let collect = collect_js();
        assert!(!compose.contains("__STATE_SHAPE__"), "compose's shape placeholder must be replaced");
        assert!(!collect.contains("__STATE_SHAPE__"), "collect's shape placeholder must be replaced");
        // Both sides must build the same set of fields
        for marker in ["nodes:", "thinks:", "a: { read: ''", "r: { read: ''"] {
            assert!(STATE_SHAPE_JS.contains(marker), "STATE_SHAPE_JS is missing {marker}");
        }
        // collect only rebuilds when "there is no state or the shape is old";
        // compose rebuilds every round
        assert!(collect.contains("!window.__snapi || !window.__snapi.a"));
        assert!(compose.contains(STATE_SHAPE_JS.trim()));
        // compose must not carry old-shape fields of its own
        assert!(!compose.contains("rlast"), "compose still has the old-shape rlast");
        assert!(!compose.contains("rpending"), "compose still has the old-shape rpending");
    }

    #[test]
    fn page_report_says_what_the_page_saw() {
        // The timeout message must say something diagnosable: last time "no
        // answer ever came" while the site had long finished answering, and the
        // panel only said "you may need to log in" — effectively nothing.
        let mut report = PageReport::default();
        report.note(&json!({"answers": 1, "chars": 602, "ready": true}));
        // Later payloads missing fields (empty polls lack them) must not clear
        // what is already known
        report.note(&json!({"d": "x"}));
        assert_eq!(report.payloads, 2);

        let line = report.summary();
        assert!(line.contains("2 payloads"), "{line}");
        assert!(line.contains("composer present"), "{line}");
        assert!(line.contains("1 answers"), "{line}");
        assert!(line.contains("602 chars of body"), "{line}");
    }

    #[test]
    fn assembler_appends_plain_deltas() {
        let mut a = Assembler::default();
        assert_eq!(a.push("first span.", false, false), ("first span.".into(), None));
        assert_eq!(a.push("second span.", false, false), ("second span.".into(), None));
        assert_eq!(a.text(), "first span.second span.");
    }

    #[test]
    fn assembler_replaces_the_whole_text_on_a_redraw() {
        // The site redrew a span already pushed out: this copy is the complete
        // body and must **replace**, not append. Appending here used to make
        // the same answer repeat two or three times in the panel.
        let mut a = Assembler::default();
        a.push("the old body.", false, false);
        let (delta, replace) = a.push("the complete body after the redraw.", true, false);
        assert_eq!(delta, "", "a full re-send must not also go out as an increment");
        assert_eq!(replace.as_deref(), Some("the complete body after the redraw."));
        assert_eq!(a.text(), "the complete body after the redraw.");
    }

    #[test]
    fn assembler_waits_for_the_whole_resent_text() {
        // Re-sends are pushed in chunks (the title has a length limit):
        // replacing before it finishes makes the panel flash a half-finished body
        let mut a = Assembler::default();
        a.push("the old one.", false, false);
        assert_eq!(a.push("chunk one", true, true), ("".into(), None));
        assert_eq!(a.push("chunk two", false, true), ("".into(), None));
        assert_eq!(
            a.push("chunk three", false, false),
            ("".into(), Some("chunk onechunk twochunk three".into()))
        );
        assert_eq!(a.text(), "chunk onechunk twochunk three");

        // A new re-send arrived while the previous one was unfinished: latest
        // wins; don't glue the two halves together
        a.push("a new copy", true, true);
        assert_eq!(a.push(", done.", false, false), ("".into(), Some("a new copy, done.".into())));
    }

    #[test]
    fn collect_js_and_rust_agree_on_field_names() {
        // This is a cross-language contract: whatever keys the in-page object
        // writes, Rust must read. Changing only one side yields "stuck at
        // analyzing forever, not a word comes out" — the hardest kind of bug to hunt.
        let js = collect_js();
        for key in ["d:", "dr:", "more:", "rmore:", "rst:", "rrst:"] {
            assert!(js.contains(key), "COLLECT_JS is missing the {key} field");
        }
        let value = parse_signal(
            r#"SNAPI|{"i":1,"d":"body","dr":"thinking","more":false,"rmore":true,"rst":false,"rrst":true,"ready":true,"answers":1}"#,
        )
        .unwrap();
        let sig = read_signal(&value).unwrap();
        assert_eq!(sig.text, "body");
        assert_eq!(sig.reasoning, "thinking");
        assert!(sig.rmore && !sig.more);
        assert!(sig.rrst && !sig.rst);
    }

    #[test]
    fn compose_js_carries_prompt_and_image() {
        let turn = Turn {
            prompt: "how do I fix this error".into(),
            images: vec!["QUJD".into()],
            new_chat: true,
            resume: None,
        };
        let js = compose_js(site_by_id("deepseek"), &turn);
        assert!(js.contains("how do I fix this error"), "the prompt must be carried in");
        assert!(
            js.contains("data:image/jpeg;base64,QUJD"),
            "the image must be carried in, as a data url"
        );
        assert!(!js.contains("__ARGS__"), "all placeholders must be replaced");
        assert!(!js.contains("__TAG__"));
    }

    #[test]
    fn resume_url_gets_quoted_into_the_navigation_js() {
        // When deep-linking back to a site conversation, the URL goes into a
        // location.href assignment: a quoting mistake makes the page jump
        // somewhere else (or the whole eval throws)
        const BS: char = '\\';
        let url = "https://chat.deepseek.com/a/chat/s/abc-123";
        let escaped = url
            .replace(BS, "\\")
            .replace('\'', "\\'");
        assert_eq!(escaped, url, "a plain URL is preserved as-is");
        let nasty = "https://x/a/chat/s/it's";
        assert_eq!(nasty.replace('\'', "\\'"), "https://x/a/chat/s/it\\'s");
    }

    #[test]
    fn compose_js_without_image_leaves_it_null() {
        let turn = Turn {
            prompt: "follow-up".into(),
            images: Vec::new(),
            new_chat: false,
            resume: None,
        };
        let js = compose_js(site_by_id("deepseek"), &turn);
        assert!(js.contains(r#""images":[]"#), "a follow-up must not carry images");
    }

    #[test]
    fn all_js_is_fully_filled() {
        // One unreplaced placeholder throws in the page and is very hard to
        // hunt. Don't use "contains a double underscore" as the criterion:
        // window.__snapi is itself legitimate.
        const PLACEHOLDERS: [&str; 18] = [
            "__COMPOSER__",
            "__ANSWER__",
            "__THINK__",
            "__SEND__",
            "__CHUNK__",
            "__TAG__",
            "__ARGS__",
            "__TEXT__",
            "__FORCE__",
            "__HAVE__",
            "__HAVER__",
            "__BASE_A__",
            "__BASE_T__",
            "__TOGGLE__",
            "__MARKER__",
            "__STATE_SHAPE__",
            "__PAGE_DIAG__",
            "__THINK_FALLBACK__",
        ];
        for (name, js) in [
            ("collect", collect_js()),
            (
                "collect_flush",
                collect_js_flush(site_by_id("deepseek"), 12, 34, Baseline { answers: 1, thinks: 0 }),
            ),
            ("compose", compose_js(site_by_id("deepseek"), &Turn {
                prompt: "q".into(),
                images: vec!["QUJD".into()],
                new_chat: true,
                resume: None,
            })),
            ("send", send_js(site_by_id("deepseek"))),
            ("new_chat", new_chat_js(site_by_id("deepseek"))),
            ("think_on", think_on_js(site_by_id("deepseek"))),
        ] {
            for placeholder in PLACEHOLDERS {
                assert!(
                    !js.contains(placeholder),
                    "{name} still contains the unreplaced placeholder {placeholder}"
                );
            }
        }
    }

    #[test]
    fn collect_js_reads_the_answer_in_both_ways() {
        // Body: normally push increments; when the site redrew a span already
        // pushed out, re-send the whole thing. Both must be present — missing
        // one produces "the answer repeats several times" or "the answer
        // differs from the page".
        let js = collect_js();
        assert!(js.contains("const feed = (s, text)"), "both streams share this");
        assert!(js.contains("s.rstNext = true"), "a mismatch must trigger a full re-send");
        assert!(js.contains("rst: aRst"), "the flag must ship with the payload");
        assert!(js.contains("rrst: rRst"), "the chain of thought must also support full re-sends");
    }

    #[test]
    fn think_on_js_checks_the_marker_before_clicking() {
        // Clicking blindly on an already-on toggle turns it off — worse than
        // not turning it on. So the --selected marker must be checked first;
        // if already on, report as-is and don't touch it.
        let js = think_on_js(site_by_id("deepseek"));
        assert!(
            js.contains("ds-toggle-button--selected"),
            "check the on-marker before deciding to click"
        );
        // None of these placeholders may remain: one left behind throws in the page
        assert!(!js.contains("__MARKER__"), "the placeholder must be replaced");
        assert!(
            js.find("includes('ds-toggle-button--selected')").unwrap()
                < js.find("btn.click()").unwrap(),
            "the marker check must come before the click"
        );
        // The report key must be think: on the Rust side error aborts the whole round
        assert!(js.contains("think:"), "must report with the think key");
        assert!(!js.contains("error:"), "must not touch the error key — that aborts the whole question");
    }

    #[test]
    fn compose_js_leaves_the_image_to_cdp_when_both_js_paths_fail() {
        // After chatgpt's redesign both JS paths (synthetic paste, file input)
    // went dead: compose raising an error here aborts the whole round, and
    // the CDP native upload fallback (file chooser / clipboard) never gets its
    // turn. So contenteditable sites must pass through (imgPath='none' is
    // left for cdp_deliver to recheck); only textarea sites (deepseek, no
    // third path) truly report the error.
        let turn = Turn {
            prompt: "q".into(),
            images: vec!["QUJD".into()],
            new_chat: true,
            resume: None,
        };
        let gpt = compose_js(site_by_id("chatgpt"), &turn);
        let ds = compose_js(site_by_id("deepseek"), &turn);
        for (name, js) in [("chatgpt", &gpt), ("deepseek", &ds)] {
            assert!(
                js.contains("neither the paste path nor the file input path"),
                "{name}'s compose must keep this error message (at least textarea sites need it)"
            );
        }
        let guard_pos = gpt.find("if (false) {").expect("chatgpt's error must be blocked by false");
        let err_pos = gpt
            .find("neither the paste path nor the file input path")
            .expect("the error message is still there");
        assert!(guard_pos < err_pos, "the error must sit inside the if (false) block");
        assert!(
            ds.find("if (true) {").is_some(),
            "deepseek's error must really be reachable"
        );
    }

    #[test]
    fn collect_js_flush_asks_the_page_to_compare_lengths() {
        // Final re-read: report the length held on this side to the page; only
        // re-send the whole thing on mismatch — re-sending unconditionally
        // every time would make the final loop never stop
        let js = collect_js_flush(site_by_id("deepseek"), 123, 456, Baseline { answers: 2, thinks: 1 });
        assert!(js.contains("if (true)"), "the re-read section must be switched on");
        assert!(js.contains("123") && js.contains("456"), "both lengths must be reported to the page");
        assert!(!collect_js().contains("if (true)"), "normal polling must not enter the re-read section");
        // Baseline guard: when no new answer node appeared and the content
        // didn't change, no full re-send is allowed (it would push the previous
        // round's answer up). The baseline is the one compose recorded in the
        // page state.
        assert!(
            js.contains("st.baseA") && js.contains("st.baseT"),
            "the baseline must be the one recorded page-side by compose"
        );
        assert!(
            js.contains("st.baseLen") && js.contains("st.baseLenR"),
            "a content change must also pass (counts are unreliable across page switches)"
        );
        assert!(
            js.contains("freshA") && js.contains("freshR"),
            "both streams must pass the baseline gate"
        );
    }

    #[test]
    fn baseline_zero_means_no_limit() {
        // When no baseline was read (new conversation, the page hasn't replied
        // yet), 0 is passed down and page-side 0 = no limit. This must be
        // watched: if 0 were treated as "the page has 0 answers in total,
        // re-sending forbidden", the first round's fallback would never fire —
        // effectively blocking the normal path.
        let js = collect_js_flush(site_by_id("deepseek"), 0, 0, Baseline::default());
        assert!(!js.contains("__BASE_A__"), "the placeholder must be replaced");
        assert!(
            js.contains("(st.baseA || 0) === 0"),
            "baseline 0 must let the page recognize the 'no limit' path"
        );
    }

    #[test]
    fn new_chat_falls_back_to_the_home_page() {
        // With the sidebar collapsed, "开启新对话" isn't rendered; the home-page
        // fallback is mandatory or the context gets mixed
        let js = new_chat_js(site_by_id("deepseek"));
        assert!(js.contains("开启新对话"));
        assert!(js.contains("https://chat.deepseek.com/"), "the fallback must return to the home page");
        assert!(js.contains("newchat"), "which route was taken must be reported back to Rust");
    }

    #[test]
    fn collect_js_ships_the_markdown_converter() {
        // The converter must be injected together with the polling script;
        // otherwise window.__snapiMD is empty page-side and every round
        // silently reads an empty string
        let js = collect_js();
        assert!(js.contains("window.__snapiMD ="), "the converter must be included");
        assert!(js.contains("window.__snapiMD("), "the polling script must use it");
        assert!(js.contains("application/x-tex"), "formulas must come from KaTeX's annotation");
    }

    #[test]
    fn driver_uses_the_verified_selectors() {
        // The deepseek set was probed from the real page (see the README's web
        // UI troubleshooting section); run the probe before changing it. The
        // other sites are "first-guess values" and need calibrating under a
        // real login.
        let ds = site_by_id("deepseek");
        assert!(ds.composer.contains("发送消息"));
        assert_eq!(ds.send, ".ds-button--primary");
        assert_eq!(ds.answer, ".ds-assistant-message-main-content");
        // Read out by the --think probe in a real session: the hash-free class name is this one
        assert_eq!(ds.think, Some(".ds-think-content"));
        assert!(ds.think_toggle.is_some(), "deepseek must have a \"deep thinking\" toggle configured");
        // All four sites must be registered, ids unique
        let ids: Vec<_> = SITES.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), 4);
        // Selectors must **not contain single quotes**: the driver JS wraps
        // them in querySelector('...'), and a stray single quote is a syntax
        // error for the whole script (measured: chatgpt/claude returned zero
        // payloads because of this)
        for site in SITES.iter() {
            for (field, value) in [
                ("composer", site.composer),
                ("send", site.send),
                ("answer", site.answer),
            ] {
                assert!(
                    !value.contains(char::from(39)),
                    "site {}'s {} selector contains a single quote: {}",
                    site.id,
                    field,
                    value
                );
            }
        }
        assert_eq!(site_by_id("chatgpt").label, "ChatGPT");
        assert_eq!(site_by_id("gemini").label, "Gemini");
        assert_eq!(site_by_id("claude").label, "Claude");
        assert_eq!(site_by_id("unrecognized").id, "deepseek", "unrecognized ids fall back to deepseek");
    }

    #[test]
    fn window_label_sanitizes_account_names() {
        assert_eq!(window_label("A-1_b"), "webchat-A-1_b");
        // "工作" is deliberate non-ASCII test data (kept as pure data).
        assert!(window_label("工作").starts_with("webchat-"));
        assert!(
            window_label("工作").chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "non-ASCII characters in the label must be folded to '-'"
        );
    }

    #[test]
    fn collect_js_reads_reasoning_before_the_answer_starts() {
        // The model may think for a long time before speaking. The chain of
        // thought must be read off **its own** stream, not keyed to the answer
        // node count — keyed to that, the chain is unreadable during that
        // whole period, which is exactly the "stuck at analyzing, no chain of
        // thought visible" symptom.
        let js = collect_js();
        assert!(js.contains("st.r"), "the chain of thought has its own state");
        assert!(
            !js.contains("thinks.length > st.thinks"),
            "reading the chain of thought must not first require a new answer node"
        );
        // Readable while collapsed: innerText returns an empty string for
        // hidden content, textContent doesn't
        assert!(js.contains(".textContent"), "thinking content must be readable while collapsed");
    }

    #[test]
    fn chunk_fits_in_a_window_title() {
        // Measured: titles can take several KB; this leaves a 10x margin
        assert!(CHUNK <= 400);
    }
}
