/* Ask panel: conversation history list on the left, preview / ask / streaming answers on the right. */
(function () {
  const {
    call, listen, renderMarkdown, createStreamRenderer,
    handleExternalLinks, installErrorReporting, toast,
    t, setLanguage, getLanguage, applyTheme, applyI18n,
  } = window.AskWithSnip;
  installErrorReporting("panel");

  const thread = document.getElementById("thread");
  const turnRail = document.getElementById("turn-rail");
  const film = document.getElementById("film");
  const legacyThumb = document.getElementById("thumb");
  const promptEl = document.getElementById("prompt");
  const sendBtn = document.getElementById("btn-send");
  const statusEl = document.getElementById("status");
  const presetsEl = document.getElementById("presets");
  const historyList = document.getElementById("history-list");
  const continueEl = document.getElementById("btn-continue");
  const backendEl = document.getElementById("btn-backend");
  const accountBox = document.getElementById("account-box");
  const accountBtn = document.getElementById("account-btn");
  const accountMenu = document.getElementById("account-menu");
  const loginBtn = document.getElementById("btn-login");
  const siteSelect = document.getElementById("site-select");
  const revealEl = document.getElementById("btn-reveal");
  const reloadEl = document.getElementById("btn-reload");
  const lightbox = document.getElementById("lightbox");
  const lightboxImg = document.getElementById("lightbox-img");
  const lightboxSelect = document.getElementById("lightbox-select");
  const lightboxDelete = document.getElementById("lightbox-delete");
  const lightboxClose = document.getElementById("lightbox-close");

  // Quick-ask buttons: they live in named lists (see presetLists) so you can
  // keep several sets and switch between them; the picker above the buttons
  // switches / creates / renames / deletes lists and persists the selection.
  // Each entry = { name, prompt }; empty prompt = send the name itself.
  let presetLists = [];
  let activePresetList = "";
  // What actually gets sent: when the prompt is empty, use the name as the prompt.
  // Quick prompts are user content — never run them through t(), they must not be translated.
  const presetPrompt = (p) => p.prompt || p.name;

  /** The list whose presets are shown / edited right now (falls back to the first list). */
  function currentPresetList() {
    return presetLists.find((l) => l.name === activePresetList) || presetLists[0];
  }

  /** The currently open preset-list menu, closed by any outside click (like the account menu). */
  let openPresetMenu = null;

  /** The current conversation (mirror of the one on the Rust side). null means no screenshot has been taken yet. */
  let conversation = null;
  let busy = false;
  /** The turn currently streaming out */
  let current = null;
  /** The DOM elements of the last turn on screen (edit question / regenerate only touch those) */
  let lastTurn = null;

  // ---------------------------------------------------------- Small utilities

  function setStatus(text) {
    statusEl.textContent = text || "";
  }

  /**
   * While busy, the send button becomes "Cancel".
   *
   * Previously the button was disabled and labeled "Analyzing…" — the user couldn't tell
   * it was still alive and had no way to abort, just waiting (or assuming it hung).
   * Now it stays clickable: clicking cancels.
   */
  function setBusy(value) {
    busy = value;
    sendBtn.textContent = value ? t("取消") : t("提问");
    // The button stays clickable: when busy it is t("取消"), when idle it sends plain text too (you can ask without a screenshot)
    sendBtn.disabled = false;
    promptEl.disabled = value;
    // While running, hide "Regenerate / Edit question": the turn hasn't landed yet, editing it is pointless
    thread.classList.toggle("busy", value);
  }

  /** While analyzing, refresh elapsed time every half second so the user knows it's alive */
  let statusTimer = null;
  let statusStartedAt = 0;
  /** An extra line to say during this turn (e.g. the webchat has to re-ask the previous turns) */
  let statusNote = "";

  /** The status bar line: the longer the wait, the more clearly it should say what's happening and how long.
   *  Once text starts streaming, also include progress (characters output so far, rough speed) —
   *  with only a seconds counter the user can't tell slow streaming from a hang. */
  function refreshStatus() {
    const secs = Math.floor((Date.now() - statusStartedAt) / 1000);
    const thinking = current && current.reasoningText;
    let line = `${thinking ? t("思考中") : t("分析中")}… ${secs}s`;
    if (current && current.answerChars > 0) {
      line += ` · ${t("已输出 {n} 字").replace("{n}", current.answerChars)}`;
      if (secs > 0) line += ` · ${Math.round(current.answerChars / secs)}${t("字/秒")}`;
    }
    if (statusNote) line += ` · ${statusNote}`;
    setStatus(line);
  }

  function startStatusTimer(note) {
    clearInterval(statusTimer);
    statusStartedAt = Date.now();
    statusNote = note || "";
    // Write a line right away so the previous turn's error message doesn't linger in the status bar for another half second
    refreshStatus();
    statusTimer = setInterval(refreshStatus, 500);
  }

  function stopStatusTimer() {
    clearInterval(statusTimer);
    statusTimer = null;
    statusNote = "";
  }

  function relativeTime(ms) {
    const diff = Date.now() - ms;
    if (diff < 60_000) return t("刚刚");
    if (diff < 3_600_000) return t("{n} 分钟前").replace("{n}", Math.floor(diff / 60_000));
    const date = new Date(ms);
    const pad = (n) => String(n).padStart(2, "0");
    const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
    if (date.toDateString() === new Date().toDateString()) return clock;
    return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${clock}`;
  }

  // ---------------------------------------------------------- Rendering

  function renderHistory(items) {
    historyList.textContent = "";

    if (!items.length) {
      const empty = document.createElement("div");
      empty.className = "side-empty";
      empty.textContent = t("还没有记录。框选一张图并提问，就会出现在这里。");
      historyList.appendChild(empty);
      return;
    }

    const activeId = conversation && conversation.id;
    for (const item of items) {
      const row = document.createElement("div");
      row.className = "history-item" + (item.id === activeId ? " active" : "");
      row.dataset.id = item.id;
      row.title = item.title;

      const title = document.createElement("div");
      title.className = "h-title";
      title.textContent = item.title;

      const meta = document.createElement("div");
      meta.className = "h-meta";
      meta.textContent = `${relativeTime(item.created_at)} · ${item.turn_count} ${t("轮")}`;

      const del = document.createElement("button");
      del.className = "h-del";
      del.textContent = "✕";
      del.title = t("删除这条记录");
      del.addEventListener("click", async (event) => {
        // Don't let the click bubble to the whole row, or it would open the very item being deleted
        event.stopPropagation();
        try {
          await call("delete_conversation", { id: item.id });
          if (conversation && conversation.id === item.id) {
            renderConversation(null);
          }
          await reloadHistory();
        } catch (err) {
          toast(String(err), "err");
        }
      });

      row.append(title, meta, del);
      row.addEventListener("click", () => openHistoryItem(item.id));
      historyList.appendChild(row);
    }
  }

  function markActiveHistory() {
    const activeId = conversation && conversation.id;
    for (const row of historyList.children) {
      row.classList.toggle("active", Boolean(activeId) && row.dataset.id === activeId);
    }
  }

  function showHint(lines) {
    const box = document.createElement("div");
    box.id = "empty-hint";
    for (const line of lines) {
      const div = document.createElement("div");
      div.textContent = line;
      box.appendChild(div);
    }
    thread.textContent = "";
    thread.appendChild(box);
  }

  function appendTurn(turn, isError) {
    const box = document.createElement("div");
    box.className = "turn" + (isError ? " error" : "");

    // The screenshots sent with this turn: they travel with the question, no need to dig through the film strip when reviewing
    const images = turn.images || [];
    if (images.length) {
      const shots = document.createElement("div");
      shots.className = "shots";
      for (const url of images) {
        const img = document.createElement("img");
        img.src = url;
        img.loading = "lazy";
        img.alt = "Screenshots sent with this turn";
        shots.appendChild(img);
      }
      box.appendChild(shots);
    }

    const q = document.createElement("div");
    q.className = "q";
    // A conversation may include several images ("continue after the last snippet", multi-select in the film strip), so mark
    // which turn brought newly captured shots and how many, so the user isn't puzzled by sentences next to one thumbnail
    if (turn.imageCount > 0) {
      const badge = document.createElement("span");
      badge.className = "from-shot";
      badge.textContent = turn.imageCount > 1 ? t("新截图 ×{n}").replace("{n}", turn.imageCount) : t("新截图");
      q.appendChild(badge);
    }
    q.appendChild(document.createTextNode(turn.question));

    const a = document.createElement("div");
    a.className = "a";
    if (turn.answer) a.innerHTML = renderMarkdown(turn.answer);

    box.append(q, a);
    thread.appendChild(box);
    thread.scrollTop = thread.scrollHeight;
    refreshTurnRail();
    return { turn: box, questionEl: q, answerEl: a, reasoningEl: null, reasoningText: "" };
  }

  // ---------------------------------------------------------- Tick rail (conversation progress navigator)

  /** One tick per turn, click to jump to it; on scroll, highlight the turn closest to the top. */
  function refreshTurnRail() {
    turnRail.textContent = "";
    const turns = thread.querySelectorAll(".turn");
    // NB: the callback param must not be named `t` — that would shadow the translate function
    turns.forEach((node, i) => {
      const tick = document.createElement("button");
      tick.className = "tick";
      tick.title = t("第 {n} 轮").replace("{n}", i + 1);
      tick.addEventListener("click", () => {
        node.scrollIntoView({ block: "start" });
        setActiveTick(tick);
      });
      turnRail.appendChild(tick);
    });
    updateActiveTick();
  }

  function setActiveTick(tick) {
    for (const el of turnRail.children) el.classList.toggle("active", el === tick);
  }

  function updateActiveTick() {
    // The test harness's DOM stubs lack getBoundingClientRect; skip if it can't be measured (only affects highlighting)
    if (typeof thread.getBoundingClientRect !== "function") return;
    const turns = [...thread.querySelectorAll(".turn")];
    if (!turns.length) return;
    const top = thread.getBoundingClientRect().top + 40;
    let best = 0;
    turns.forEach((node, i) => {
      if (typeof node.getBoundingClientRect === "function" && node.getBoundingClientRect().top <= top) best = i;
    });
    const tick = turnRail.children[best];
    if (tick) setActiveTick(tick);
  }

  thread.addEventListener("scroll", () => requestAnimationFrame(updateActiveTick));

  /**
   * The two buttons under the last turn: Regenerate / Edit question.
   *
   * Only for the last turn — the Rust side only supports touching the last turn too (editing an earlier
   * turn invalidates all later ones; the site's own "edit" has the same semantics, but the panel
   * doesn't expose that yet).
   */
  function refreshTurnActions() {
    for (const row of thread.querySelectorAll(".turn-actions")) row.remove();
    if (!lastTurn || !conversation || !conversation.turns.length) return;

    const row = document.createElement("div");
    row.className = "turn-actions";

    const again = document.createElement("button");
    again.type = "button";
    again.textContent = t("重新生成");
    again.title = t("这个问题不动，让模型重答一次");
    again.addEventListener("click", regenerateLast);

    const edit = document.createElement("button");
    edit.type = "button";
    edit.textContent = t("修改提问");
    edit.title = t("改掉这一轮的问题，然后重问");
    edit.addEventListener("click", () => editLast());

    row.append(again, edit);
    lastTurn.turn.appendChild(row);
  }

  /**
   * "Regenerate / Edit question" on failed turns.
   *
   * When a new question fails, Rust reverts that turn along with its images (no empty turn is kept in
   * history), so the regular last-turn buttons can't reach the failed question — therefore the
   * original question and screenshots are kept in retry, and before resending, restage_images puts
   * the images back into staging, then the normal ask path runs.
   * Uses a separate fail-actions class: refreshTurnActions only clears turn-actions, so it won't wipe these.
   */
  function showFailedActions(turn, retry) {
    for (const row of thread.querySelectorAll(".fail-actions")) row.remove();

    const row = document.createElement("div");
    row.className = "fail-actions";

    const restage = async (keptImages) => {
      if (keptImages.length) {
        try {
          await call("restage_images", { images: keptImages });
        } catch (err) {
          toast(String(err), "err");
          return false;
        }
      }
      return true;
    };

    const again = document.createElement("button");
    again.type = "button";
    again.textContent = t("重新生成");
    again.title = t("原样重发这个问题（连截图一起）");
    again.addEventListener("click", async () => {
      if (busy) return;
      row.remove();
      if (await restage(retry.images)) await dispatchAsk(retry.question, retry.images);
    });

    const edit = document.createElement("button");
    edit.type = "button";
    edit.textContent = t("修改提问");
    edit.title = t("改掉这个问题（截图可增删），然后重发");
    edit.addEventListener("click", () => {
      if (busy) return;
      row.remove();
      const editor = document.createElement("div");
      editor.className = "q-edit";
      const area = document.createElement("textarea");
      area.value = retry.question;
      area.rows = 2;

      // The images this question was sent with: click one to drop it from the re-ask
      // (click again to keep); "＋" adds local images, and dropping/pasting onto the
      // panel joins here while the editor is open (editSession). Dropped images aren't
      // restaged, so the re-ask really goes out without them.
      const turnImages = (retry.images || []).slice();
      const keptImages = turnImages.slice();
      const strip = document.createElement("div");
      strip.className = "q-edit-strip";
      const makeThumb = (src, title, onClick) => {
        const img = document.createElement("img");
        img.src = src;
        img.className = "q-edit-thumb";
        img.title = title;
        img.addEventListener("click", onClick);
        return img;
      };
      turnImages.forEach((src, i) => {
        const thumb = makeThumb(src, t("点一下剔除这张，重发时不带；再点一下保留"), () => {
          const at = keptImages.indexOf(turnImages[i]);
          if (at >= 0) {
            keptImages.splice(at, 1);
            thumb.classList.add("dropped");
          } else {
            keptImages.push(turnImages[i]);
            thumb.classList.remove("dropped");
          }
        });
        strip.appendChild(thumb);
      });
      const addBtn = document.createElement("button");
      addBtn.type = "button";
      addBtn.className = "q-edit-add";
      addBtn.textContent = "＋";
      addBtn.title = t("添加本地图片（也可以直接把图片拖进来，或截图后 Ctrl+V 粘贴）");
      const fileInput = document.createElement("input");
      fileInput.type = "file";
      fileInput.accept = "image/*";
      fileInput.multiple = true;
      fileInput.hidden = true;
      addBtn.addEventListener("click", () => fileInput.click());
      fileInput.addEventListener("change", async () => {
        const files = Array.from(fileInput.files || []);
        fileInput.value = "";
        for (const file of files) {
          try {
            if (isImageFile(file)) await addImageFile(file);
          } catch (err) {
            toast(String(err), "err");
          }
        }
      });
      strip.append(addBtn, fileInput);
      const addFailedEditImage = (staged, name) => {
        keptImages.push(staged.imageB64);
        const src = `data:image/jpeg;base64,${staged.imageB64}`;
        const thumb = makeThumb(
          src,
          t("新加的图片{t}：点一下移除").replace("{t}", name ? `（${name}）` : ""),
          () => {
            const at = keptImages.indexOf(staged.imageB64);
            if (at >= 0) keptImages.splice(at, 1);
            thumb.remove();
          }
        );
        thumb.classList.add("added");
        strip.insertBefore(thumb, addBtn);
      };

      const okBtn = document.createElement("button");
      okBtn.type = "button";
      okBtn.textContent = t("重问");
      const cancelBtn = document.createElement("button");
      cancelBtn.type = "button";
      cancelBtn.textContent = t("取消");
      const buttons = document.createElement("div");
      buttons.className = "q-edit-buttons";
      buttons.append(okBtn, cancelBtn);
      editor.append(strip, area, buttons);
      turn.questionEl.classList.add("editing");
      turn.questionEl.appendChild(editor);
      // While open, files dropped/pasted anywhere on the panel join this editor
      editSession = { area, addImage: addFailedEditImage };
      area.focus();
      area.select();
      okBtn.addEventListener("click", async () => {
        const question = area.value.trim();
        if (!question) return;
        editor.remove();
        editSession = null;
        turn.questionEl.classList.remove("editing");
        // Original order restored, editor-added images last — same rule as editLast
        const originalAt = (src) => {
          const at = turnImages.indexOf(src);
          return at === -1 ? turnImages.length : at;
        };
        keptImages.sort((a, b) => originalAt(a) - originalAt(b));
        if (await restage(keptImages)) await dispatchAsk(question, keptImages);
      });
      cancelBtn.addEventListener("click", () => {
        editSession = null;
        editor.remove();
        turn.questionEl.classList.remove("editing");
        turn.turn.appendChild(row);
      });
    });

    row.append(again, edit);
    turn.turn.appendChild(row);
  }

  /**
   * The reasoning process (the "thinking" part of reasoning models) goes in its own collapsible block above the answer.
   *
   * Created only when the first chain-of-thought arrives — for non-reasoning (or ordinary) models
   * nothing shows here, so no empty collapsible appears.
   */
  function ensureReasoning(turn) {
    if (turn.reasoningEl) return turn.reasoningEl;
    const box = document.createElement("details");
    box.className = "reasoning";
    box.open = true; // Expanded by default: this is exactly what the user wants to read
    const summary = document.createElement("summary");
    summary.textContent = t("思考过程");
    const body = document.createElement("div");
    body.className = "reasoning-body";
    box.append(summary, body);
    turn.turn.insertBefore(box, turn.answerEl);
    turn.reasoningEl = body;
    return body;
  }

  /**
   * Lay out the film strip: "+" captures another shot, shots taken but not yet sent sit on the right,
   * click one to select/deselect (highlighted = will be sent).
   *
   * When nothing is staged, fall back to the old display: a single static preview (the image used by
   * this conversation's last turn). An <img> without src must be hidden — the browser draws a broken-image placeholder.
   */
  function renderFilm(conv) {
    film.textContent = "";
    const before = filmCount;
    const sent = (conv && conv.sent) || [];
    const staged = (conv && conv.staged) || [];
    // Wrap each image in a cell: the image + the ✕ in the top-right corner (appears on hover, same gesture as the history list).
    // After a deletion, redraw everything through refreshStaging — removing one image shifts the numbering of the rest.
    const cell = (img, remove, tip) => {
      const box = document.createElement("div");
      box.className = "film-cell";
      const del = document.createElement("button");
      del.className = "film-del";
      del.textContent = "✕";
      del.title = tip;
      del.addEventListener("click", async (event) => {
        event.stopPropagation(); // Don't let it also count as "select/deselect"
        try {
          await remove();
          await refreshStaging();
        } catch (err) {
          toast(String(err), "err");
        }
      });
      box.append(img, del);
      film.appendChild(box);
    };
    // Lay out the already-sent ones first: dimmed, just to show "these are what was just sent".
    // Without this, once staging clears after sending, only the single preview remains, making several shots look like one.
    for (const item of sent) {
      const done = document.createElement("img");
      done.className = "film-item sent";
      done.src = item.dataUrl;
      done.title = t("已经发出去的图。点一下放大预览");
      done.addEventListener("click", () => {
        openLightbox({
          src: item.dataUrl,
          onDelete: () => call("remove_turn_image", { turn: item.turnIndex, image: item.imageIndex }),
        });
      });
      cell(
        done,
        () => call("remove_turn_image", { turn: item.turnIndex, image: item.imageIndex }),
        t("把这张从这一轮的记录里删掉（之后追问、重新生成都不再带上它）")
      );
    }
    for (const [index, item] of staged.entries()) {
      const img = document.createElement("img");
      img.className = "film-item" + (item.selected ? " selected" : "");
      img.src = item.dataUrl;
      img.title = item.selected
        ? t("已选中，会跟着下一次提问发出去。点一下放大预览")
        : t("没选中。点一下放大预览，在预览里选中");
      // Click opens the full-size preview; select/deselect moved into the
      // lightbox bar (a mis-toggle on a 50px thumbnail was too easy to make
      // and hard to notice).
      img.addEventListener("click", () => {
        lightbox._thumbEl = img;
        openLightbox({
          src: item.dataUrl,
          selected: item.selected,
          onToggle: async () => {
            const selected = await call("toggle_staged", { index });
            // Sync the local copy too: the pre-ask "at least one selected" check reads
            // conversation.staged, and images moved here by "New" start unselected
            if (conversation && conversation.staged && conversation.staged[index]) {
              conversation.staged[index].selected = selected;
            }
            return selected;
          },
          onDelete: () => call("remove_staged", { index }),
        });
      });
      cell(img, () => call("remove_staged", { index }), t("删掉这张（不发给 AI）"));
    }
    // The newly captured shot sticks to the far right: when the count grows, scroll it into view, otherwise
    // "I captured a new one" looks like nothing happened. Beyond the max, the browser clamps to the right on its own.
    if (sent.length + staged.length > before) film.scrollLeft = film.scrollWidth;
    filmCount = sent.length + staged.length;

    film.hidden = sent.length === 0 && staged.length === 0;
    // Only fall back to the single static preview when the film strip is empty (old records, or images that failed to load)
    legacyThumb.hidden = !(film.hidden && conv && conv.dataUrl);
    if (legacyThumb.hidden) {
      legacyThumb.removeAttribute("src");
    } else {
      legacyThumb.src = conv.dataUrl;
      legacyThumb.title = t("点一下放大预览");
      legacyThumb.onclick = () => openLightbox({ src: conv.dataUrl });
    }
  }

  /** How many images the film strip had last: scroll only when a new one is added, don't disturb someone browsing back and forth */
  let filmCount = 0;

  // ---------------------------------------------------------- Image lightbox

  /**
   * Full-size preview of a film-strip image. Clicking a thumbnail opens this
   * instead of toggling selection (a mis-click on a 50px thumbnail silently
   * changed what would be sent — hard to notice and hard to undo); selection
   * and removal moved into the bar here, where the image is big enough to see
   * what is being toggled.
   *
   * `onToggle` / `onDelete` are async; after they run the film strip redraws
   * (renderFilm / refreshStaging), so the lightbox is closed first.
   */
  let lightboxOpen = false;
  function openLightbox({ src, selected, onToggle, onDelete }) {
    lightboxImg.src = src;
    lightboxSelect.hidden = !onToggle;
    lightboxDelete.hidden = !onDelete;
    lightboxDelete.textContent = t("删除");
    const renderSelect = (sel) => {
      lightboxSelect.textContent = sel ? t("取消选中") : t("选中发送");
      lightboxSelect.classList.toggle("primary", !sel);
    };
    if (onToggle) {
      renderSelect(selected);
      lightboxSelect.onclick = async () => {
        try {
          const sel = await onToggle();
          renderSelect(sel);
          // The film strip doesn't redraw on toggle; sync the border here.
          lightbox._thumbEl?.classList.toggle("selected", sel);
        } catch (err) {
          toast(String(err), "err");
        }
      };
    } else {
      lightboxSelect.onclick = null;
    }
    if (onDelete) {
      lightboxDelete.onclick = async () => {
        closeLightbox();
        try {
          await onDelete();
          // Same as the ✕ in the strip's corner: the Rust side is the source of
          // truth, and without this redraw the deleted image stays on screen
          // ("clicked delete, nothing changed").
          await refreshStaging();
        } catch (err) {
          toast(String(err), "err");
        }
      };
    } else {
      lightboxDelete.onclick = null;
    }
    lightbox.hidden = false;
    lightboxOpen = true;
  }

  function closeLightbox() {
    if (!lightboxOpen) return;
    lightbox.hidden = true;
    lightboxImg.removeAttribute("src");
    lightbox._thumbEl = null;
    lightboxOpen = false;
  }

  // Backdrop click closes; clicks on the image / the button bar must not.
  lightbox.addEventListener("click", (e) => {
    if (e.target === lightbox) closeLightbox();
  });
  lightboxClose.addEventListener("click", closeLightbox);

  function renderConversation(conv) {
    conversation = conv;
    current = null;
    lastTurn = null;
    thread.textContent = "";

    if (!conv) {
      renderFilm(null);
      promptEl.value = "";
      promptEl.disabled = false;
      promptEl.placeholder = t("想问什么？回车发送，Shift+回车换行");
      sendBtn.disabled = busy;
      setStatus("");
      showHint([
        t("还没有截图。"),
        t("可以直接在这里打字提问；要带图的话按 Ctrl+Shift+A，或者点托盘图标框选区域。"),
        t("左边的列表是以前的对话，点一下就能打开，也可以接着追问。"),
      ]);
      markActiveHistory();
      return;
    }

    promptEl.disabled = false;
    promptEl.placeholder = t("想问什么？回车发送，Shift+回车换行");
    sendBtn.disabled = busy;

    renderFilm(conv);

    for (const turn of conv.turns) lastTurn = appendTurn(turn, false);

    refreshTurnActions();
    refreshTurnRail();
    markActiveHistory();
    if (!busy) promptEl.focus();
  }

  // ---------------------------------------------------------- Data

  async function reloadHistory() {
    try {
      renderHistory(await call("list_history"));
    } catch (err) {
      toast(String(err), "err");
    }
  }

  async function reloadConversation() {
    try {
      conversation = await call("get_conversation");
    } catch (err) {
      toast(String(err), "err");
      conversation = null;
    }
    renderConversation(conversation);
  }

  async function openHistoryItem(id) {
    if (busy) {
      toast(t("等这次回答结束再切换"));
      return;
    }
    try {
      renderConversation(await call("load_conversation", { id }));
    } catch (err) {
      toast(String(err), "err");
    }
  }

  // ---------------------------------------------------------- Continue from context

  /**
   * Whether the next snip starts a new conversation or continues this one.
   *
   * Stored in the config (same switch as the checkbox on the settings page), so changes here show up
   * there too, and survive a restart. Saved immediately on change, no "Save settings" click needed.
   */
  let continueNext = false;

  function renderContinueToggle() {
    continueEl.classList.toggle("on", continueNext);
    // The label's **length must stay constant**: previously the "on" state read "Continue here ✓", and the
    // extra character widened the column, squeezing the input box — clicking the button made the whole
    // row jump. Use the two equal-width symbols ● / ○ for on and off.
    continueEl.textContent = (continueNext ? "● " : "○ ") + t("接着上文");
    continueEl.title = continueNext
      ? t("下次框选会把新截图接到当前这段对话后面（点一下改回另起一段）")
      : t("当前：每次框选都另起一段对话。点一下改成接在当前这段后面");
  }

  async function loadContinueFlag() {
    try {
      const cfg = await call("get_config");
      continueNext = Boolean(cfg && cfg.continue_conversation);
    } catch (_) {
      /* If it can't be read, show the default (start a new one); nothing else is affected */
    }
    renderContinueToggle();
  }

  async function toggleContinue() {
    const wanted = !continueNext;
    try {
      const cfg = await call("get_config");
      cfg.continue_conversation = wanted;
      await call("save_config", { cfg });
      continueNext = wanted;
      renderContinueToggle();
      toast(
        wanted
          ? t("下次框选会接到这一段后面")
          : t("下次框选会另起一段对话"),
        "ok"
      );
    } catch (err) {
      toast(String(err), "err");
    }
  }

  // ---------------------------------------------------------- Sending

  /** Whether the running turn was cancelled by the user (decides how failure is displayed) */
  let cancelled = false;
  /** The conversation was swapped to another one while busy: do one full refresh at cleanup (see the conversation-changed listener) */
  let deferredConversation = false;

  async function cancelCurrent() {
    cancelled = true;
    setStatus(t("正在取消…"));
    try {
      await call("cancel_ask");
    } catch (err) {
      toast(String(err), "err");
    }
  }

  /**
   * Run a turn: hand the question to the backend, stream the answer into the turn's answerEl.
   *
   * New questions, regenerations, and edited questions all go through here; only "which command to
   * call" and "which turn to write into" differ. Cleanup for failure/cancellation lives here too,
   * so all three paths behave the same.
   *
   * The `turn` passed in is the object returned by `appendTurn`, and `current` points directly at it —
   * the chain-of-thought and whole-segment replacement listeners both write onto this object. **It must
   * be the same object**: if, as before, a new `{...}` is used as current, a regeneration can't find the
   * previous turn's thinking block and the new reasoning hangs under the old one (found by testing).
   */
  async function runTurn({ turn, invoke, onDone, note, retry }) {
    cancelled = false;
    setBusy(true);
    startStatusTimer(note);

    turn.answerEl.classList.add("pending");
    turn.renderer = createStreamRenderer(turn.answerEl);
    turn.reasoningText = "";
    turn.answerChars = 0;
    current = turn;

    let answer = null;
    // Set when the failed/cancelled turn got the retry row (showFailedActions): the turn was
    // retracted by the backend, so the regular last-turn buttons must NOT be added on top of
    // it — otherwise the same turn ends up with both rows, i.e. two 重新生成 and two 修改提问.
    let failedActionsShown = false;
    try {
      answer = await invoke();
      turn.renderer.finish(answer);
      turn.answerEl.classList.remove("pending");
      if (cancelled) {
        if (!answer || !answer.trim()) turn.answerEl.textContent = t("（已取消）");
        toast(t("已取消"));
      }
      // Don't just clear the status bar: give this turn's closing summary (character count, duration),
      // otherwise the previous line of numbers vanishes the moment streaming ends and the user can't
      // tell finished from broken
      if (cancelled) {
        setStatus("");
      } else {
        const secs = Math.max(1, Math.floor((Date.now() - statusStartedAt) / 1000));
        setStatus(
          t("完成 · {n} 字 · {s} 秒")
            .replace("{n}", (answer || "").length)
            .replace("{s}", secs)
        );
      }
      if (onDone) onDone(answer);
    } catch (err) {
      turn.answerEl.classList.remove("pending");
      if (cancelled) {
        // Cancellation isn't an error: keep whatever already came out; say so if not a single character arrived
        if (!turn.answerEl.textContent.trim()) turn.answerEl.textContent = t("（已取消）");
        setStatus(t("已取消"));
      } else if (!turn.turn.classList.contains("error")) {
        // On failure Rust has already broadcast ai-error (status bar and this answer are both written), so this
        // is just a fallback cleanup, to avoid showing the same message twice. Careful not to clear the status
        // bar here — that's the very line the user needs.
        turn.turn.classList.add("error");
        turn.answerEl.textContent = String(err);
        setStatus(String(err));
      }
      // Failed turns get "Regenerate / Edit question": resend as-is (with the screenshots), or edit the question and resend.
      // Cancelled turns need the same when nothing arrived: the backend retracted that turn (no empty
      // question kept in history), so without these buttons the question and its snips would be stuck
      // with no way to re-ask. Cancels with partial output are excluded: that turn was kept normally
      // and already has the regular last-turn buttons.
      // Failures on the regenerate/edit path don't use this — Rust puts the original turn back as it was,
      // so the buttons the last turn already has are directly usable.
      if (retry && (!cancelled || !answer || !answer.trim())) {
        showFailedActions(turn, retry);
        failedActionsShown = true;
      }
    } finally {
      current = null;
      cancelled = false;
      stopStatusTimer();
      setBusy(false);
      // Rust only persists after the Q&A finishes, so refresh the list (title and turn count change).
      // On success, refresh the **film strip** too, treating Rust as the source of truth: the selected
      // images are now part of this turn and staging should be cleared.
      // Note: only refresh the film strip, **do not redraw the conversation area** — the reasoning and
      // the already-displayed answer exist only in the UI, not in the stored history; a full redraw
      // would wipe them (found by testing).
      // On failure/cancellation refresh nothing: Rust has reverted the unfinished turn, but the error
      // message (or partial answer) just shown on the panel must stay.
      if (answer) {
        await refreshStaging();
      }
      await reloadHistory();
      // The conversation was swapped while the answer was running (e.g. a screenshot was taken then, starting a new one): do a full refresh now
      if (deferredConversation) {
        deferredConversation = false;
        await reloadConversation();
      }
      // The retry row (fail-actions) replaces the regular buttons on a retracted turn;
      // adding the regular ones too is how the turn ended up with four buttons
      if (!failedActionsShown) refreshTurnActions();
      promptEl.focus();
    }
    return answer;
  }

  /** After a question, refresh only the film strip and its status line (leave the conversation area alone). */
  async function refreshStaging() {
    try {
      const fresh = await call("get_conversation");
      if (!fresh) return;
      if (conversation && fresh.id === conversation.id) {
        conversation.staged = fresh.staged;
        conversation.sent = fresh.sent;
        // dataUrl must follow too: after deleting an image the backend turns it into an empty string (that
        // "fallback preview"); updating only staged/sent would leave an empty film strip still showing the
        // deleted screenshot as the old main image.
        conversation.dataUrl = fresh.dataUrl;
      } else {
        conversation = fresh;
      }
      renderFilm(conversation);
    } catch (err) {
      toast(String(err), "err");
    }
  }

  // ---------------------------------------------------------- Drop / paste files

  /**
   * Drop (or paste) files onto the panel: images join the film strip exactly like a
   * snip; text/code files are appended to the prompt as a fenced block. While the
   * "edit question" editor is open (editLast), images and text are routed into the
   * editor instead — that's how a re-ask gains new images.
   */

  /** The open question editor, if any; set by editLast. { area, addImage } */
  let editSession = null;

  const IMAGE_FILE_RE = /\.(png|jpe?g|webp|gif|bmp|avif)$/i;
  const isImageFile = (file) => file.type.startsWith("image/") || IMAGE_FILE_RE.test(file.name);
  const TEXT_FILE_RE = /\.(txt|md|markdown|log|json|csv|tsv|ya?ml|toml|ini|cfg|conf|env|html?|css|scss|less|js|jsx|ts|tsx|mjs|cjs|py|rb|php|rs|go|java|kt|swift|c|h|cpp|hpp|cc|cs|m|sql|sh|bat|cmd|ps1|xml|svg|diff|patch)$/i;

  /** Limits for dropped images: the same downscale and JPEG quality as a snip (read
   * from settings), so a dropped file costs the model no more than a screenshot. */
  async function imageLimits() {
    try {
      const cfg = await call("get_config");
      return { maxEdge: cfg.max_image_edge || 1568, quality: (cfg.jpeg_quality || 85) / 100 };
    } catch {
      return { maxEdge: 1568, quality: 0.85 };
    }
  }

  /**
   * Decode an image file and re-encode it to JPEG exactly like the snip path does:
   * long edge capped at maxEdge, quality from settings. Returns the pieces
   * stage_uploaded_image takes (bare base64 — the Rust side stores no prefix — plus
   * shown and sent sizes).
   */
  async function imageFileToStaged(file) {
    const { maxEdge, quality } = await imageLimits();
    const url = URL.createObjectURL(file);
    try {
      const img = await new Promise((resolve, reject) => {
        const el = new Image();
        el.onload = () => resolve(el);
        el.onerror = () => reject(new Error(`${t("图片读不出来")}: ${file.name}`));
        el.src = url;
      });
      const cropW = img.naturalWidth;
      const cropH = img.naturalHeight;
      const ratio = Math.min(1, maxEdge / Math.max(cropW, cropH, 1));
      const sentW = Math.max(1, Math.round(cropW * ratio));
      const sentH = Math.max(1, Math.round(cropH * ratio));
      const canvas = document.createElement("canvas");
      canvas.width = sentW;
      canvas.height = sentH;
      canvas.getContext("2d").drawImage(img, 0, 0, sentW, sentH);
      const dataUrl = canvas.toDataURL("image/jpeg", quality);
      return { imageB64: dataUrl.slice(dataUrl.indexOf(",") + 1), cropW, cropH, sentW, sentH };
    } finally {
      URL.revokeObjectURL(url);
    }
  }

  const MAX_TEXT_CHARS = 100_000;

  /** Read a text/code file to append to the question. Oversized files get truncated. */
  async function fileToText(file) {
    let text = await file.text();
    if (text.length > MAX_TEXT_CHARS) {
      text = text.slice(0, MAX_TEXT_CHARS) + "\n…" + t("（文件太长，已截断）");
    }
    return text;
  }

  /** Stage a dropped image — into the open editor when there is one, else the film strip. */
  async function addImageFile(file) {
    const staged = await imageFileToStaged(file);
    if (editSession) {
      editSession.addImage(staged, file.name);
      return;
    }
    await call("stage_uploaded_image", staged);
    if (conversation) await refreshStaging();
    else await reloadConversation();
  }

  /** Route a dropped text file: into the editor textarea when editing, else the prompt. */
  async function addTextFile(file) {
    const text = await fileToText(file);
    const block = `\n\n--- ${file.name} ---\n${text}\n`;
    if (editSession) {
      editSession.area.value += block;
      editSession.area.focus();
    } else {
      promptEl.value += block;
      promptEl.focus();
    }
  }

  async function handleFiles(files) {
    let handled = 0;
    for (const file of files) {
      try {
        if (isImageFile(file)) {
          await addImageFile(file);
          handled++;
        } else if (file.type.startsWith("text/") || TEXT_FILE_RE.test(file.name)) {
          await addTextFile(file);
          handled++;
        } else {
          toast(t("暂不支持这种文件：{n}").replace("{n}", file.name), "err");
        }
      } catch (err) {
        toast(String(err), "err");
      }
    }
    if (handled) toast(t("已添加 {n} 个文件").replace("{n}", handled), "ok");
  }

  // dragenter/leave are counted (they fire for every element crossed, not once per
  // drag) so the highlight toggles once per drag, not once per element.
  let dragDepth = 0;
  document.addEventListener("dragenter", (event) => {
    event.preventDefault();
    dragDepth++;
    // The ::after overlay reads its text from here (see style.css body.dropping)
    document.body.dataset.dropHint = t("松开，添加文件或图片");
    document.body.classList.add("dropping");
  });
  document.addEventListener("dragover", (event) => event.preventDefault());
  document.addEventListener("dragleave", () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (!dragDepth) document.body.classList.remove("dropping");
  });
  document.addEventListener("drop", (event) => {
    event.preventDefault();
    dragDepth = 0;
    document.body.classList.remove("dropping");
    handleFiles(Array.from(event.dataTransfer?.files || []));
  });

  // Paste an image straight from the clipboard (screenshot tool, browser right-click
  // copy): same as dropping it. Text pastes stay native — only files are intercepted.
  document.addEventListener("paste", (event) => {
    const files = Array.from(event.clipboardData?.files || []);
    if (!files.length) return;
    event.preventDefault();
    handleFiles(files);
  });

  /** Clear a turn's answer (including the reasoning block); used before regenerating / editing a question */
  function clearAnswer(turn) {
    turn.answerEl.textContent = "";
    turn.answerEl.classList.remove("error");
    turn.turn.classList.remove("error");
    turn.reasoningText = "";
    if (turn.reasoningEl) {
      const box = turn.reasoningEl.closest(".reasoning");
      if (box) box.remove();
      turn.reasoningEl = null;
    }
  }

  /** The next question waiting in line (when busy, clicking "Ask" / pressing Enter queues it; it sends automatically when this turn finishes) */
  let queuedQuestion = null;

  async function send(viaButton) {
    // When busy: text in the input = queue (auto-sends when this turn finishes, once);
    // empty input + button click = cancel the current turn. Previously this was always t("取消") and the
    // user had to click once more to send; a rapid "cancel + resend" combo sent the same image twice —
    // now it queues, eliminating that combination.
    if (busy) {
      const pending = promptEl.value.trim();
      if (!pending) {
        if (viaButton) await cancelCurrent();
        return;
      }
      queuedQuestion = pending;
      promptEl.value = "";
      // Don't use setStatus: the running turn rewrites the status bar every 500ms, so a queued
      // message would be covered within half a second (as good as never shown). Put it into
      // statusNote so the timer refreshes it too, keeping it visible until this turn finishes.
      statusNote = t("已排队：等这轮答完就发");
      refreshStatus();
      return;
    }

    const question = promptEl.value.trim();
    if (!question) return;
    await dispatchAsk(question);
  }

  /** Validate + lay out the turn + run it. The queued question also goes through here, keeping both paths consistent. */
  async function dispatchAsk(question, presetImages) {
    // It can send without a conversation: Rust opens a new one on the fly (plain text, no images)

    // Sending with zero images selected would make the first question a bare text prompt (the model sees no image)
    // Check staged first: with no conversation yet (null), this must short-circuit and let the
    // text-only question through — Rust opens a fresh conversation on the fly.
    const stagedAll = (conversation && conversation.staged) || [];
    if (stagedAll.length > 0 && conversation.turns.length === 0 && !stagedAll.some((s) => s.selected)) {
      toast(t("先点选至少一张截图（高亮的才会发送）"), "err");
      return;
    }

    promptEl.value = "";
    // Stage the images for this turn into the conversation right here (Rust only now takes them out of staging);
    // the retry path uses presetImages (restage_images puts them back into staging, and here they're collected as usual)
    const shotUrls =
      presetImages ||
      ((conversation && conversation.staged) || [])
        .filter((s2) => s2.selected)
        .map((s2) => s2.dataUrl);
    lastTurn = appendTurn(
      { question, answer: "", imageCount: shotUrls.length, images: shotUrls },
      false
    );
    refreshTurnActions();

    await runTurn({
      turn: lastTurn,
      invoke: () => call("ask", { prompt: question }),
      onDone: (answer) => {
        // Record this turn locally too (counting the selected staged images). images
        // must come along: "edit question" on this turn reads conversation.turns' last
        // entry, and without it the editor would show no thumbnails to keep/drop.
        if (conversation) {
          conversation.turns = conversation.turns.concat([
            { question, answer, imageCount: shotUrls.length, images: shotUrls },
          ]);
        }
      },
      retry: { question, images: shotUrls },
    });

    // There's a queued question: send it next (runTurn's cleanup already cleared the status; go straight to the next one)
    if (queuedQuestion) {
      const next = queuedQuestion;
      queuedQuestion = null;
      setStatus("");
      await dispatchAsk(next);
    }
  }

  /** Regenerate: keep the question, redo the answer */
  async function regenerateLast() {
    if (busy || !lastTurn || !conversation || !conversation.turns.length) return;
    const index = conversation.turns.length - 1;
    const question = conversation.turns[index].question;
    clearAnswer(lastTurn);

    await runTurn({
      turn: lastTurn,
      invoke: () => call("regenerate"),
      onDone: (answer) => {
        conversation.turns[index] = { ...conversation.turns[index], question, answer };
      },
    });
  }

  /** Edit question: replace this turn's question (and optionally drop screenshots), then ask again */
  async function editLast() {
    if (busy || !lastTurn || !conversation || !conversation.turns.length) return;
    const index = conversation.turns.length - 1;
    const original = conversation.turns[index].question;
    const questionEl = lastTurn.questionEl;

    // Swap in an inline editor: the original line is tucked away first (restored on cancel)
    const editor = document.createElement("div");
    editor.className = "q-edit";
    const area = document.createElement("textarea");
    area.value = original;
    area.rows = 2;

    // This turn's screenshots: click one to drop it from the re-ask (click again to keep).
    // Kept = what gets sent; dropping all re-asks as plain text. The "＋" button adds
    // local images; dropping/pasting images onto the panel joins here too (editSession).
    const turnImages = conversation.turns[index].images || [];
    const keptImages = turnImages.slice();
    const strip = document.createElement("div");
    strip.className = "q-edit-strip";
    const makeThumb = (src, title, onClick) => {
      const img = document.createElement("img");
      img.src = src;
      img.className = "q-edit-thumb";
      img.title = title;
      img.addEventListener("click", onClick);
      return img;
    };
    turnImages.forEach((src, i) => {
      const thumb = makeThumb(src, t("点一下剔除这张，重发时不带；再点一下保留"), () => {
        const at = keptImages.indexOf(turnImages[i]);
        if (at >= 0) {
          keptImages.splice(at, 1);
          thumb.classList.add("dropped");
        } else {
          keptImages.push(turnImages[i]);
          thumb.classList.remove("dropped");
        }
      });
      strip.appendChild(thumb);
    });

    // "＋": pick local images to send along with the re-ask. The same happens for
    // images dropped onto the panel or pasted while this editor is open (editSession).
    const addBtn = document.createElement("button");
    addBtn.type = "button";
    addBtn.className = "q-edit-add";
    addBtn.textContent = "＋";
    addBtn.title = t("添加本地图片（也可以直接把图片拖进来，或截图后 Ctrl+V 粘贴）");
    const fileInput = document.createElement("input");
    fileInput.type = "file";
    fileInput.accept = "image/*";
    fileInput.multiple = true;
    fileInput.hidden = true;
    addBtn.addEventListener("click", () => fileInput.click());
    fileInput.addEventListener("change", async () => {
      const files = Array.from(fileInput.files || []);
      fileInput.value = "";
      for (const file of files) {
        try {
          if (isImageFile(file)) await addImageFile(file);
        } catch (err) {
          toast(String(err), "err");
        }
      }
    });
    strip.append(addBtn, fileInput);

    const okBtn = document.createElement("button");
    okBtn.type = "button";
    okBtn.textContent = t("重问");
    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.textContent = t("取消");
    const buttons = document.createElement("div");
    buttons.className = "q-edit-buttons";
    buttons.append(okBtn, cancelBtn);
    editor.append(strip, area, buttons);

    questionEl.classList.add("editing");
    questionEl.appendChild(editor);
    area.focus();
    area.select();

    /** Send one image (picked via ＋, dropped or pasted onto the panel) with the re-ask.
     * The thumb click removes it again; original thumbs toggle, added ones just go. */
    const addEditImage = (staged, name) => {
      keptImages.push(staged.imageB64);
      const src = `data:image/jpeg;base64,${staged.imageB64}`;
      const thumb = makeThumb(
        src,
        t("新加的图片{t}：点一下移除").replace("{t}", name ? `（${name}）` : ""),
        () => {
          const at = keptImages.indexOf(staged.imageB64);
          if (at >= 0) keptImages.splice(at, 1);
          thumb.remove();
        }
      );
      thumb.classList.add("added");
      strip.insertBefore(thumb, addBtn);
    };
    // While open, files dropped/pasted anywhere on the panel join this editor
    editSession = { area, addImage: addEditImage };

    const close = () => {
      editSession = null;
      editor.remove();
      questionEl.classList.remove("editing");
    };
    const submit = async () => {
      const text = area.value.trim();
      if (!text) {
        toast(t("问题不能是空的"), "err");
        return;
      }
      close();
      clearAnswer(lastTurn);
      // Images that survived: restore their original order (dropping + re-keeping shuffles);
      // images added in the editor aren't part of that original order and go last
      const originalAt = (src) => {
        const at = turnImages.indexOf(src);
        return at === -1 ? turnImages.length : at;
      };
      keptImages.sort((a, b) => originalAt(a) - originalAt(b));
      const imageCount = keptImages.length;
      // Replace that line with the new question: re-lay the badge and text (same as in appendTurn)
      const parts = [];
      if (imageCount > 0) {
        const badge = document.createElement("span");
        badge.className = "from-shot";
        badge.textContent = imageCount > 1 ? t("新截图 ×{n}").replace("{n}", imageCount) : t("新截图");
        parts.push(badge);
      }
      parts.push(document.createTextNode(text));
      questionEl.replaceChildren(...parts);
      // Lay in the new question first; on failure Rust puts the original turn back, then we re-sync here
      conversation.turns[index] = { ...conversation.turns[index], question: text, answer: "" };

      const answer = await runTurn({
        turn: lastTurn,
        invoke: () => call("edit_question", { prompt: text, images: keptImages }),
        onDone: (answer) => {
          conversation.turns[index] = {
            ...conversation.turns[index],
            question: text,
            answer,
            imageCount,
            images: keptImages,
          };
        },
      });
      if (answer === null) await reloadConversation();
    };
    okBtn.addEventListener("click", submit);
    cancelBtn.addEventListener("click", () => {
      close();
      promptEl.focus();
    });
    area.addEventListener("keydown", (event) => {
      if (event.isComposing || event.keyCode === 229) return;
      if (event.key === "Enter" && !event.shiftKey) {
        event.preventDefault();
        submit();
      }
      if (event.key === "Escape") {
        event.preventDefault();
        close();
      }
    });
  }

  /** A list name that doesn't collide yet: "新列表", "新列表 2", "新列表 3"... */
  function uniqueListName(base) {
    let n = 1;
    let name = base;
    while (presetLists.some((l) => l.name === name)) {
      n += 1;
      name = `${base} ${n}`;
    }
    return name;
  }

  /**
   * The list picker above the quick-ask buttons: click it to open a menu, pick
   * another list to switch (persisted immediately), or create / rename / delete
   * lists. Menu rows are plain buttons; the active list is highlighted.
   */
  /** List names are user data stored in the config, so t() doesn't touch them;
     translate only the built-in "默认" so the English UI shows "Default". */
  const displayListName = (name) => (name === "默认" ? t("默认") : name);

  function buildPresetPicker() {
    const box = document.createElement("div");
    box.className = "preset-picker-box";
    const btn = document.createElement("button");
    btn.className = "preset-picker";
    const current = currentPresetList();
    btn.textContent = (current ? displayListName(current.name) : "") + " ▾";
    btn.title = t("快捷用语列表（点开切换；也可以新建 / 重命名 / 删除）");

    const menu = document.createElement("div");
    menu.className = "preset-menu";
    menu.hidden = true;

    for (const list of presetLists) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = "menu-row preset-list-option" + (list.name === activePresetList ? " active" : "");
      row.textContent = displayListName(list.name);
      row.addEventListener("click", () => switchPresetList(list.name));
      menu.appendChild(row);
    }

    const sep = document.createElement("div");
    sep.className = "account-menu-sep";
    menu.appendChild(sep);

    const addRow = document.createElement("button");
    addRow.type = "button";
    addRow.className = "menu-row";
    addRow.textContent = t("新建列表…");
    addRow.addEventListener("click", () => createPresetList());
    menu.appendChild(addRow);

    const renameRow = document.createElement("button");
    renameRow.type = "button";
    renameRow.className = "menu-row";
    renameRow.textContent = t("重命名当前列表…");
    renameRow.addEventListener("click", () => renamePresetList());
    menu.appendChild(renameRow);

    const delRow = document.createElement("button");
    delRow.type = "button";
    delRow.className = "menu-row danger";
    delRow.textContent = t("删除当前列表…");
    delRow.addEventListener("click", () => deletePresetList());
    menu.appendChild(delRow);

    btn.addEventListener("click", (event) => {
      event.stopPropagation();
      menu.hidden = !menu.hidden;
    });
    box.append(btn, menu);
    openPresetMenu = menu;
    return box;
  }

  /** Native prompt for the list name (WebView2 supports it); no-prompt environments give null. */
  function askListName() {
    if (typeof window.prompt !== "function") return null;
    const current = currentPresetList();
    const entered = window.prompt(t("列表名字"), current ? current.name : "");
    return entered == null ? null : entered.trim();
  }

  /** Persist the whole list set + the active selection so the panel reopens on it. */
  async function persistPresetLists() {
    try {
      await call("set_preset_lists", { lists: presetLists, active: activePresetList });
    } catch (err) {
      toast(String(err), "err");
    }
  }

  async function switchPresetList(name) {
    if (name === activePresetList) return;
    activePresetList = name;
    await persistPresetLists();
    buildPresets();
  }

  async function createPresetList() {
    const name = askListName();
    if (name === null) return; // cancelled
    const finalName = name || uniqueListName(t("新列表"));
    if (presetLists.some((l) => l.name === finalName)) {
      toast(t("已经叫「{n}」了").replace("{n}", finalName), "err");
      return;
    }
    presetLists.push({ name: finalName, presets: [] });
    activePresetList = finalName;
    await persistPresetLists();
    buildPresets();
    toast(t("已新建列表「{n}」").replace("{n}", finalName), "ok");
  }

  async function renamePresetList() {
    const list = currentPresetList();
    if (!list) return;
    const name = askListName();
    if (name == null || !name || name === list.name) return;
    if (presetLists.some((l) => l.name === name)) {
      toast(t("已经叫「{n}」了").replace("{n}", name), "err");
      return;
    }
    list.name = name;
    activePresetList = name;
    await persistPresetLists();
    buildPresets();
  }

  async function deletePresetList() {
    const list = currentPresetList();
    if (!list || presetLists.length <= 1) return; // always keep at least one list
    const msg = t("删除列表「{n}」？它里面的快捷用语也会一起删掉。").replace("{n}", list.name);
    if (typeof window.confirm === "function" && !window.confirm(msg)) return;
    presetLists = presetLists.filter((l) => l !== list);
    activePresetList = presetLists[0].name;
    await persistPresetLists();
    buildPresets();
  }

  function buildPresets() {
    presetsEl.textContent = "";
    presetsEl.appendChild(buildPresetPicker());
    const list = currentPresetList();
    (list ? list.presets : []).forEach((preset, index) => {
      const button = document.createElement("button");
      button.className = "preset";
      button.textContent = preset.name;
      button.title = t("点击发送；右键编辑");
      button.addEventListener("click", () => {
        const text = presetPrompt(preset);
        promptEl.value = promptEl.value ? `${promptEl.value}\n${text}` : text;
        send();
      });
      button.addEventListener("contextmenu", (event) => {
        event.preventDefault();
        openPresetEditor(index);
      });
      presetsEl.appendChild(button);
    });

    // "＋": add a new quick prompt
    const add = document.createElement("button");
    add.className = "preset-add";
    add.textContent = "＋";
    add.title = t("新增快捷用语");
    add.addEventListener("click", () => openPresetEditor(-1));
    presetsEl.appendChild(add);
  }

  /**
   * Quick prompt editor. index = array index (right-click to edit); -1 = add new.
   * The name is what shows on the button; the prompt is what actually gets sent, empty sends the name itself.
   */
  function openPresetEditor(index) {
    const existing = document.getElementById("preset-editor");
    if (existing) existing.remove();
    const isNew = index < 0;
    // The list refactoring split presets into named lists; the editor edits the
    // **current** list's entry (the index buildPresets passed in is into its array)
    const list = currentPresetList();
    const source = isNew ? { name: "", prompt: "" } : list.presets[index];

    const editor = document.createElement("div");
    editor.id = "preset-editor";
    editor.className = "preset-editor";

    const nameLabel = document.createElement("div");
    nameLabel.className = "preset-editor-label";
    nameLabel.textContent = t("名称（按钮上显示的）");
    const nameInput = document.createElement("input");
    nameInput.type = "text";
    nameInput.value = source.name;
    nameInput.placeholder = t("例如：翻译成中文");

    const promptLabel = document.createElement("div");
    promptLabel.className = "preset-editor-label";
    promptLabel.textContent = t("提示词（实际发送的内容，留空 = 用名称）");
    const area = document.createElement("textarea");
    area.value = source.prompt;
    area.rows = 3;
    area.placeholder = t("可引用截图让模型干活；留空则把名称当提示词发出去");

    const buttons = document.createElement("div");
    buttons.className = "preset-editor-buttons";
    const ok = document.createElement("button");
    ok.type = "button";
    ok.className = "primary";
    ok.textContent = t("保存");
    buttons.append(ok);
    if (!isNew) {
      const del = document.createElement("button");
      del.type = "button";
      del.textContent = t("删除");
      del.addEventListener("click", async () => {
        const list = currentPresetList();
        list.presets = list.presets.filter((_, i) => i !== index);
        try {
          await persistPresetLists();
          buildPresets();
        } catch (err) {
          toast(String(err), "err");
        }
      });
      buttons.append(del);
    }
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.textContent = t("取消");
    buttons.append(cancel);

    editor.append(nameLabel, nameInput, promptLabel, area, buttons);
    presetsEl.appendChild(editor);
    nameInput.focus();

    cancel.addEventListener("click", () => editor.remove());
    ok.addEventListener("click", async () => {
      const name = nameInput.value.trim();
      const prompt = area.value.trim();
      if (!name && !prompt) return;
      const entry = { name: name || prompt, prompt };
      const list = currentPresetList();
      const next = list.presets.slice();
      if (isNew) next.push(entry);
      else next[index] = entry;
      list.presets = next;
      try {
        await persistPresetLists();
        buildPresets();
      } catch (err) {
        toast(String(err), "err");
      }
    });
  }

  /** Frameless windows have no native resize border; hand dragging on the edges over to the system resize loop */
  function setupResizeHandles() {
    const tauriWindow = window.__TAURI__ && window.__TAURI__.window;
    if (!tauriWindow) return;
    const appWindow = tauriWindow.getCurrentWindow();
    for (const handle of document.querySelectorAll("[data-resize]")) {
      handle.addEventListener("mousedown", (event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        appWindow.startResizeDragging(handle.dataset.resize).catch(() => {});
      });
    }
  }

  // ---------------------------------------------------------- Startup

  async function boot() {
    // Quick-prompt lists are read from the config: each list = { name, presets }.
    // Backwards compatible with the old flat `presets` array (and plain-string entries).
    try {
      const cfg = await call("get_config");
      const norm = (entries) => (Array.isArray(entries) ? entries : [])
        .map((p) =>
          typeof p === "string" ? { name: p, prompt: "" } : { name: p.name || "", prompt: p.prompt || "" }
        ).filter((p) => p.name);
      const lists = (Array.isArray(cfg.preset_lists) ? cfg.preset_lists : [])
        .map((l) => ({ name: l.name || "", presets: norm(l.presets) }))
        .filter((l) => l.name);
      if (lists.length) {
        presetLists = lists;
        activePresetList = lists.some((l) => l.name === cfg.active_preset_list)
          ? cfg.active_preset_list : lists[0].name;
      } else {
        presetLists = [{ name: "默认", presets: norm(cfg.presets) }];
        activePresetList = "默认";
      }
    } catch (_) { /* If unreadable, keep the defaults */ }
    if (!presetLists.length) {
      presetLists = [{
        name: "默认",
        presets: [
          { name: "翻译成中文", prompt: "把截图里的所有内容翻译成中文，保持原有排版" },
          { name: "翻译成英文", prompt: "把截图里的所有内容翻译成英文，保持原有排版" },
        ],
      }];
      activePresetList = "默认";
    }
    buildPresets();
    setupResizeHandles();
    handleExternalLinks(thread);

    // Don't show a broken image even if the image data is corrupt (e.g. a damaged record); just say so
    legacyThumb.addEventListener("error", () => {
      if (!legacyThumb.getAttribute("src")) return;
      legacyThumb.hidden = true;
      toast(t("截图预览加载失败，图片数据可能已损坏"), "err");
    });

    await listen("ai-delta", (event) => {
      if (!current) return;
      current.answerEl.classList.add("pending");
      const chunk = String(event.payload ?? "");
      current.answerChars = (current.answerChars || 0) + chunk.length;
      current.renderer.push(chunk);
    });

    // Chain of thought: accumulated separately, placed into the collapsible block above. It doesn't render Markdown —
    // reasoning text often contains half-finished formulas and asterisks; plain text is least likely to look garbled.
    await listen("ai-reasoning", (event) => {
      if (!current) return;
      current.reasoningText += String(event.payload ?? "");
      const body = ensureReasoning(current);
      body.textContent = current.reasoningText;
      body.scrollTop = body.scrollHeight;
      // As soon as thinking content appears, the status bar switches from "analyzing" to "thinking" — these are two
      // different things; while watching a number grow, seeing that it's "thinking" beats only knowing it's waiting
      if (statusTimer) refreshStatus();
    });

    await listen("ai-replace", (event) => {
      // With the webchat, the site repaints the small segment it just output (half-finished table rows,
      // late footnote markers). The deltas then don't line up, and Rust resends the whole segment;
      // here we **replace** rather than append — appending would repeat the same passage several times.
      if (!current) return;
      const payload = event.payload || {};
      if (typeof payload.answer === "string") {
        current.renderer.replace(payload.answer);
        // On a full resend the deltas don't line up; take the character count from the resent full text
        current.answerChars = payload.answer.length;
      }
      if (typeof payload.reasoning === "string") {
        current.reasoningText = payload.reasoning;
        const body = ensureReasoning(current);
        body.textContent = current.reasoningText;
      }
    });

    await listen("ai-done", () => {
      if (current) current.answerEl.classList.remove("pending");
    });

    await listen("ai-error", (event) => {
      // A "failure" caused by cancellation isn't an error; send()'s side does the cleanup
      if (cancelled) return;
      const message = String(event.payload ?? t("请求失败"));
      setStatus(message);
      if (current) {
        current.turn.classList.add("error");
        current.answerEl.textContent = message;
        current.answerEl.classList.remove("pending");
      }
    });

    // The panel window is reused across snips (rebuilding it would hit the "window with same label already
    // exists" race), so new screenshots — or entering from the tray's "History" — refresh via this event.
    // Guard: **never redraw the conversation area while an answer is streaming** — redrawing rips out the
    // in-progress answer (the elements current points at disappear, later deltas are all dropped), showing
    // up as "took a new screenshot and the answer scrambled/vanished". In that case refresh only the film
    // strip (new shots enter it as usual) and the left list; if this snip swapped the conversation to
    // another one, do a full refresh at cleanup (see runTurn's finally).
    await listen("conversation-changed", async () => {
      if (busy) {
        const beforeId = conversation && conversation.id;
        await refreshStaging();
        if (conversation && beforeId !== conversation.id) deferredConversation = true;
        await reloadHistory();
        return;
      }
      await reloadConversation();
      await reloadHistory();
    });

    await reloadConversation();
    await reloadHistory();
    await loadContinueFlag();
    await loadBackend();
    await loadAccounts();

    // Language: apply as soon as the stored language is available (static copy + buttons) and wire up the toggle
    try {
      const cfg = await call("get_config");
      setLanguage(cfg.language);
      applyTheme(cfg.theme);
      listen("theme-changed", (e) => applyTheme(e.payload)).catch(() => {});
    } catch (_) { /* If unreadable, keep Chinese */ }
    setLanguage(getLanguage());
    buildPresets(); // quick-prompt buttons were built before the language was known; rebuild in the final one
    document.getElementById("btn-lang").addEventListener("click", async () => {
      const next = getLanguage() === "en" ? "zh" : "en";
      try {
        await call("set_language", { lang: next });
        setLanguage(next);
        buildPresets();
        renderBackend(); // its label is dynamic (API / Web ✓), applyI18n no longer touches it
        document.getElementById("btn-lang").textContent = next === "en" ? "中" : "EN";
        toast(next === "en" ? "Language: English" : t("语言：中文"), "ok");
      } catch (err) {
        toast(String(err), "err");
      }
    });

    await call("panel_ready");
    promptEl.focus();
  }

  // ---------------------------------------------------------- Event bindings

  sendBtn.addEventListener("click", () => send(true));
  continueEl.addEventListener("click", toggleContinue);

  // "+": hide the panel so you can browse normally; newly snipped images are added to the film strip automatically
  // (snip several in a row and send them together)
  promptEl.addEventListener("keydown", (event) => {
    // While an IME is composing (typing Chinese/Japanese), Enter confirms the candidate, it doesn't send.
    // Without this check, pressing Enter to pick a word would send half a sentence.
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      // While busy, Enter = queue (if the input has text); cancellation only via the button
      send(false);
    }
  });

  document.addEventListener("keydown", (event) => {
    // Esc during composition cancels the candidates; it shouldn't also close the panel
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "Escape") {
      // The lightbox sits above the panel: Esc closes the preview first, and
      // only a second press hides the panel.
      if (lightboxOpen) {
        event.preventDefault();
        closeLightbox();
        return;
      }
      event.preventDefault();
      call("hide_panel").catch(() => {});
    }
  });

  document.getElementById("btn-close").addEventListener("click", () => {
    call("hide_panel").catch(() => {});
  });

  document.getElementById("btn-settings").addEventListener("click", () => {
    // Hide the panel first: it's an always-on-top window that would sit over the settings window,
    // so settings would open yet be covered, looking like "settings won't open".
    call("hide_panel").catch(() => {});
    call("open_settings_window").catch((err) => toast(err, "err"));
  });

  document.getElementById("btn-again").addEventListener("click", async () => {
    try {
      await call("hide_panel");
      // Trigger a fresh snip right after hiding the panel; it feels like "snip another one"
      await call("trigger_snip");
    } catch (err) {
      toast(err, "err");
    }
  });

  document.getElementById("btn-clear-history").addEventListener("click", async () => {
    try {
      await call("clear_history");
      renderConversation(null);
      await reloadHistory();
      toast(t("记录已清空"), "ok");
    } catch (err) {
      toast(String(err), "err");
    }
  });

  // Sidebar "New": start a new conversation. The backend moves staged images into the new one and returns it;
  // the frontend redraws everything; the old conversation is already persisted if it had turns, so refresh
  // the list to put the highlight back in the right place.
  document.getElementById("btn-new-chat").addEventListener("click", async () => {
    try {
      conversation = await call("new_conversation");
      renderConversation(conversation);
      await reloadHistory();
    } catch (err) {
      toast(String(err), "err");
    }
  });

  document.getElementById("btn-copy").addEventListener("click", async () => {
    if (!conversation || !conversation.turns.length) {
      toast(t("还没有可以复制的内容"));
      return;
    }
    const text = conversation.turns
      .map((turn) => `${t("问：")}${turn.question}\n${t("答：")}${turn.answer}`)
      .join("\n\n");
    try {
      await call("copy_text", { text });
      toast(t("已复制整段对话"), "ok");
    } catch (err) {
      toast(String(err), "err");
    }
  });

  // ---------------------------------------------------------- Backend switch

  /**
   * Which path questions take: the API (consumes quota) or the built-in webchat (uses your own web accounts).
   *
   * This only swaps the backend; panel behavior and display are identical — snip as usual, type here as
   * usual, answers stream here as usual. Stored in the config; saved immediately on switch.
   */
  let useWebchat = false;

  function renderBackend() {
    backendEl.classList.toggle("on", useWebchat);
    backendEl.textContent = useWebchat ? t("网页版 ✓") : t("接口");
    backendEl.title = useWebchat
      ? t("当前走内嵌网页版：用你的网页账号提问，不消耗接口额度。点一下改回接口。")
      : t("当前走接口：消耗 API 额度。点一下改成内嵌网页版（不消耗额度）。");
    // "Show page" only makes sense on the webchat (signing in, or inspecting a problem over there)
    revealEl.hidden = !useWebchat;
    reloadEl.hidden = !useWebchat;
    // The sign-in button follows the account dropdown: click to pop up the current account's web window
    loginBtn.hidden = !useWebchat;
    renderAccountSelects();
  }

  // If the class flip (and its 0.13s color/background transition) happens while the
  // panel window is hidden, WebView2 can freeze the transition mid-flight: the button
  // comes back with white text and no surface — "the API button is white". Re-apply
  // the whole state the moment the window becomes visible again, which snaps the
  // transition to its real endpoint.
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) renderBackend();
  });

  // ---------------------------------------------------------- Web accounts (per-account isolation)

  /** Site registry (mirrors the registry in Rust's webchat.rs; used only for ids and display names) */
  const SITES = [
    ["deepseek", "DeepSeek"],
    ["chatgpt", "ChatGPT"],
    ["gemini", "Gemini"],
    ["claude", "Claude"],
  ];

  /** All web accounts + which one is active (by name). Logins are isolated per account: one data directory each. */
  let accounts = [];
  let activeAccount = "";

  async function loadAccounts() {
    try {
      const cfg = await call("get_config");
      accounts = (cfg && cfg.accounts) || [];
      activeAccount = (cfg && cfg.active_account) || (accounts[0] && accounts[0].name) || "";
    } catch (_) {
      accounts = [];
    }
    renderAccountSelects();
  }

  async function saveAccounts() {
    // Dedicated channel: only the account list and active account are sent (the full-config round-trip of
    // save_config is unreliable — account changes never reached disk through it). Surface errors as-is; don't swallow.
    try {
      await call("update_webchat_accounts", { accounts: accounts, active: activeAccount });
    } catch (err) {
      toast(t("保存账号失败: ") + String(err), "err");
      throw err;
    }
  }

  function renderAccountSelects() {
    accountBox.hidden = !useWebchat;
    siteSelect.hidden = !useWebchat;
    if (!useWebchat) return;
    // The site dropdown's options must be laid out every time (matches the registry in Rust's webchat.rs)
    siteSelect.textContent = "";
    for (const [id, name] of SITES) {
      const opt = document.createElement("option");
      opt.value = id;
      opt.textContent = name;
      siteSelect.appendChild(opt);
    }
    const current = accounts.find((a) => a.name === activeAccount);
    siteSelect.value = current ? current.site : "deepseek";

    // The account button = the current account's name tag
    const siteName = (SITES.find(([id]) => id === (current ? current.site : "")) || SITES[0])[1];
    accountBtn.textContent = siteName + " · " + (activeAccount || t("无账号")) + " ▾";

    // Menu: each row is a checkbox (use proxy) + the account name
    accountMenu.textContent = "";
    for (const account of accounts) {
      const site2 = SITES.find(([id]) => id === account.site) || SITES[0];
      const row = document.createElement("div");
      row.className = "account-row" + (account.name === activeAccount ? " active" : "");

      const proxy = document.createElement("input");
      proxy.type = "checkbox";
      // Proxy is checked per [account × site]: the same account goes direct on deepseek, proxy on chatgpt.
      // The old field (proxy_enabled) was migrated to proxy_sites once on the Rust side; only the new field counts here.
      const onThisSite = (account.proxy_sites || []).includes(account.site);
      proxy.checked = onThisSite;
      proxy.title = t("勾上 = 这个账号在这个站点走代理；不勾 = 直连。保存后窗口自动重建");
      proxy.addEventListener("change", async () => {
        const sites = new Set(account.proxy_sites || []);
        if (proxy.checked) sites.add(account.site);
        else sites.delete(account.site);
        account.proxy_sites = [...sites];
        try {
          await saveAccounts();
          toast(proxy.checked
            ? t("「{n}」在 {s} 上走代理（窗口已重建，立即生效）").replace("{n}", account.name).replace("{s}", account.site)
            : t("「{n}」在 {s} 上改为直连（窗口已重建，立即生效）").replace("{n}", account.name).replace("{s}", account.site), "ok");
        } catch (err) {
          toast(String(err), "err");
        }
      });

      const label = document.createElement("span");
      label.textContent = site2[1] + " · " + account.name + (onThisSite ? " ⚑" + t("代理") : "");
      label.title = t("点一下切换到这个账号");
      label.addEventListener("click", async () => {
        activeAccount = account.name;
        accountMenu.hidden = true;
        try {
          await saveAccounts();
        } catch (err) {
          toast(String(err), "err");
        }
        renderAccountSelects();
      });

      row.append(proxy, label);
      accountMenu.appendChild(row);
    }

    // Add/remove section
    const sep = document.createElement("div");
    sep.className = "account-menu-sep";
    const addRow = document.createElement("button");
    addRow.className = "account-row";
    addRow.textContent = t("+ 添加账号…");
    addRow.addEventListener("click", () => {
      accountMenu.hidden = true;
      addAccount();
    });
    accountMenu.append(sep, addRow);
    if (accounts.length > 1) {
      const delRow = document.createElement("button");
      delRow.className = "account-row danger";
      delRow.textContent = t("- 删除当前账号…");
      delRow.addEventListener("click", () => {
        accountMenu.hidden = true;
        deleteActiveAccount();
      });
      accountMenu.appendChild(delRow);
    }
  }

  accountBtn.addEventListener("click", () => {
    accountMenu.hidden = !accountMenu.hidden;
  });
  document.addEventListener("click", (event) => {
    if (!accountMenu.hidden && !accountBox.contains(event.target)) accountMenu.hidden = true;
  });

  // "More" menu: low-frequency actions (copy / show webchat / reload webchat) live here.
  // The buttons keep their ids; listeners and hidden toggling are the same as before.
  const moreBox = document.getElementById("more-box");
  const moreMenu = document.getElementById("more-menu");
  document.getElementById("btn-more").addEventListener("click", () => {
    moreMenu.hidden = !moreMenu.hidden;
  });
  document.addEventListener("click", (event) => {
    if (!moreMenu.hidden && !moreBox.contains(event.target)) moreMenu.hidden = true;
  });
  // Close the preset-list menu on any outside click (rows close it themselves via bubbling)
  document.addEventListener("click", () => {
    if (openPresetMenu) {
      openPresetMenu.hidden = true;
      openPresetMenu = null;
    }
  });

  async function addAccount() {
    // New account: completely separate login state (separate data directory); click "Sign in" once on first use
    let n = accounts.length + 1;
    while (accounts.some((a) => a.name === t("账号") + n)) n += 1;
    const current = accounts.find((a) => a.name === activeAccount);
    const site = current ? current.site : "deepseek";
    // Non-deepseek sites get the proxy pre-checked: direct connection almost certainly fails for these
    // (chatgpt.com direct was measured to time out); the window would white-screen, not even the login page
    // would appear, and the user would just think "nothing happened". deepseek is domestic — stay direct.
    // When no proxy address is set on the settings page, the Rust side treats it as direct anyway.
    const needsProxy = site !== "deepseek";
    accounts = accounts.concat([
      { name: t("账号") + n, site: site, proxy_enabled: false, proxy_sites: needsProxy ? [site] : [] },
    ]);
    activeAccount = t("账号") + n;
    try {
      await saveAccounts();
      toast(t(needsProxy
        ? "已添加「{n}」（已勾上代理，站点直连不通时必须走）：点「登录」登一次"
        : "已添加「{n}」：点「登录」登一次").replace("{n}", t("账号") + n), "ok");
    } catch (err) {
      toast(String(err), "err");
    }
    renderAccountSelects();
  }

  async function deleteActiveAccount() {
    const victim = activeAccount;
    if (!confirm(t("删除账号「{n}」？它保存的网页登录态也会一并删掉，撤销不了。").replace("{n}", victim))) return;
    accounts = accounts.filter((a) => a.name !== victim);
    activeAccount = (accounts[0] && accounts[0].name) || "";
    try {
      await saveAccounts();
      try { await call("delete_webchat_account", { name: victim }); } catch (_) {}
      toast(t("已删除「{n}」").replace("{n}", victim), "ok");
    } catch (err) {
      toast(String(err), "err");
    }
    renderAccountSelects();
  }

  // Sign in: pop up the current account's web window (logins are isolated per account; the panel header shows who is signed in)
  loginBtn.addEventListener("click", async () => {
    try {
      await call("reveal_webchat");
    } catch (err) {
      toast(String(err), "err");
    }
  });

  siteSelect.addEventListener("change", async () => {
    const current = accounts.find((a) => a.name === activeAccount);
    if (!current) return;
    current.site = siteSelect.value;
    // When switching to a non-deepseek site, check the proxy too (same reason as addAccount: direct
    // connection most likely fails; without it the window white-screens and looks broken). Don't touch
    // already-checked sites — don't wipe the user's manual choice.
    let proxyNote = "";
    if (siteSelect.value !== "deepseek" && !(current.proxy_sites || []).includes(siteSelect.value)) {
      current.proxy_sites = [...(current.proxy_sites || []), siteSelect.value];
      proxyNote = t("（该站点已自动勾上代理）");
    }
    try {
      await saveAccounts();
      toast(t("该账号改用 {n}，点「显示网页」登录").replace("{n}", siteSelect.options[siteSelect.selectedIndex].textContent + proxyNote), "ok");
    } catch (err) {
      toast(String(err), "err");
    }
    renderAccountSelects();
  });

  async function loadBackend() {
    try {
      const cfg = await call("get_config");
      useWebchat = Boolean(cfg && cfg.backend === "webchat");
    } catch (_) {
      /* If unreadable, show as API; nothing else is affected */
    }
    renderBackend();
  }

  async function toggleBackend() {
    const wanted = !useWebchat;
    try {
      const cfg = await call("get_config");
      cfg.backend = wanted ? "webchat" : "api";
      await call("save_config", { cfg });
      useWebchat = wanted;
      renderBackend();
      toast(
        wanted
          ? t("已切到网页版：用你的网页账号提问，不消耗接口额度。没登录过就先点「显示网页」登录一次")
          : t("已切回接口"),
        "ok"
      );
    } catch (err) {
      toast(String(err), "err");
    }
  }

  backendEl.addEventListener("click", toggleBackend);

  // The driver runs in the background with its window hidden; when something goes wrong (sign-in needed,
  // a captcha, a site redesign) the user must be able to see what's happening — this button is that doorway.
  revealEl.addEventListener("click", async () => {
    try {
      await call("reveal_webchat");
    } catch (err) {
      toast(String(err), "err");
    }
  });

  // Reload webchat: when the page white-screens, sticks on a captcha, or the proxy just changed, reload it.
  // "Show page" only shows, it can't fix; this button always works — no need to delete the account and start over.
  reloadEl.addEventListener("click", async () => {
    reloadEl.disabled = true;
    try {
      await call("reveal_webchat");
      await call("reload_webchat");
      toast(t("已让网页版重新加载"), "ok");
    } catch (err) {
      toast(String(err), "err");
    } finally {
      reloadEl.disabled = false;
    }
  });

  boot();
})();
