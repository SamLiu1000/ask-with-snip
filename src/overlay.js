/* Snip overlay.
   The window is fullscreen, borderless, always-on-top, and its background is the frozen
   frame captured at the moment the hotkey was pressed, so this is "drawing a rectangle on
   a static image" - the overlay can never end up inside the screenshot. */
(function () {
  const { call, listen, installErrorReporting, setLanguage, applyTheme, t } = window.AskWithSnip;
  installErrorReporting("overlay");

  listen("theme-changed", (e) => applyTheme(e.payload)).catch(() => {});

  const shot = document.getElementById("shot");
  const shade = document.getElementById("shade");
  const rectEl = document.getElementById("rect");
  const sizeEl = document.getElementById("size");
  const guideV = document.getElementById("guide-v");
  const guideH = document.getElementById("guide-h");
  const hint = document.getElementById("hint");
  const errorBox = document.getElementById("screen-error");

  /** Frozen frame info: width/height are physical pixels and the coordinate system used for the final crop */
  let frame = null;
  let dragging = false;
  let origin = null;
  let cursor = null;
  let submitted = false;
  /** Where the left button went down, and whether the pointer actually moved - decides confirm vs redraw on release */
  let pressAt = null;
  let moved = false;

  const MIN_SIZE = 6; // smaller than this is a stray click, not a snip
  const DRAG_SLOP = 3; // pointer must move this many pixels after mousedown to count as a drag, else a click
  // These must be t() calls, not constants: the module loads before boot() reads the
  // config and sets the language, so a constant would freeze the Chinese copy even
  // in the English UI.
  const hintIdleText = () => t("按住左键拖动框选；或者点一下定起点、移动鼠标、再点框内确认 · Esc 取消");
  const hintAnchoredText = () => t("移动鼠标定大小，点一下确认（按住拖可以重新框）· Esc 撤销这个框");

  function fail(message) {
    // Showing it on screen is not enough; also report it to stderr. Users rarely see the
    // console, and this is the only clue for a failed snip: without it, all that remains
    // afterwards is "nothing happened".
    call("report_js_error", { message: `[overlay] snip failed: ${message}` }).catch(() => {});

    // When capture fails this window covers the whole screen, so the user must be told how to
    // get out - otherwise they are left staring at a black fullscreen window. Everything is
    // written via textContent, never HTML concatenation.
    errorBox.textContent = "";

    const title = document.createElement("div");
    title.style.cssText = "font-weight:600;color:var(--danger);margin-bottom:8px";
    title.textContent = t("截图没能完成");

    const body = document.createElement("div");
    body.style.cssText = "white-space:pre-wrap;margin-bottom:12px";
    body.textContent = String(message);

    const tip = document.createElement("div");
    tip.className = "hint";
    tip.textContent = t("按 Esc 关闭这个窗口。");

    errorBox.append(title, body, tip);
    errorBox.hidden = false;
    hint.style.display = "none";
  }

  /** Back to the initial not-yet-snipping state */
  function resetUi() {
    submitted = false;
    dragging = false;
    origin = null;
    cursor = null;
    rectEl.style.display = "none";
    sizeEl.style.display = "none";
    shade.className = "full";
    guideV.style.display = "none";
    guideH.style.display = "none";
    hint.textContent = hintIdleText();
    hint.style.display = "";
    errorBox.hidden = true;
    errorBox.textContent = "";
  }

  /** Fetch the current frame and fill the window with it; only then may the window be shown */
  async function bindFrame() {
    try {
      frame = await call("get_frame");
    } catch (err) {
      // Capture failed (permissions, session isolation, ...). The window was created hidden, so it
      // must be shown here - otherwise pressing the hotkey looks like "nothing happened" with no hint why.
      frame = null;
      fail(err);
      await call("overlay_ready").catch(() => {});
      return;
    }
    if (!frame || !frame.width) {
      frame = null;
      fail(t("拿到的画面是空的，请重新按热键。"));
      await call("overlay_ready").catch(() => {});
      return;
    }

    shot.src = frame.dataUrl;
    // Wait for the image to actually decode before showing, avoiding a white flash
    if (shot.decode) {
      try {
        await shot.decode();
      } catch (_) {
        /* If decoding fails, carry on anyway - at least the window appears */
      }
    }
    await call("overlay_ready");
  }

  async function boot() {
  try { const c0 = await call("get_config"); setLanguage(c0 && c0.language); applyTheme(c0 && c0.theme); window.AskWithSnip.applyI18n(); } catch (_) {}
    listen("hotkey-error", (event) => fail(event.payload)).catch(() => {});

    // The overlay window is reused across snips: on a new snip Rust only resizes/repositions it and
    // broadcasts this event. We swap the frame and clear selection state here instead of reloading the page.
    await listen("frame-ready", () => {
      resetUi();
      bindFrame();
    });

    await bindFrame();
  }

  /** Viewport coordinates -> frozen-frame physical pixel coordinates */
  function toFrame(clientX, clientY) {
    const box = shot.getBoundingClientRect();
    return {
      x: (clientX - box.left) * (frame.width / box.width),
      y: (clientY - box.top) * (frame.height / box.height),
    };
  }

  /** The rectangle being dragged out (viewport coordinates) */
  function selection() {
    if (!origin || !cursor) return null;
    return {
      left: Math.min(origin.x, cursor.x),
      top: Math.min(origin.y, cursor.y),
      width: Math.abs(cursor.x - origin.x),
      height: Math.abs(cursor.y - origin.y),
    };
  }

  function draw() {
    const sel = selection();
    if (!sel) {
      rectEl.style.display = "none";
      sizeEl.style.display = "none";
      shade.className = "full";
      return;
    }

    rectEl.style.display = "block";
    rectEl.style.left = sel.left + "px";
    rectEl.style.top = sel.top + "px";
    rectEl.style.width = sel.width + "px";
    rectEl.style.height = sel.height + "px";
    // Once the selection takes shape, the dimming comes from the rect's box-shadow so the inside keeps full brightness
    shade.className = "hidden";

    const left = toFrame(sel.left, sel.top);
    const right = toFrame(sel.left + sel.width, sel.top + sel.height);
    const w = Math.round(Math.abs(right.x - left.x));
    const h = Math.round(Math.abs(right.y - left.y));

    sizeEl.style.display = "block";
    sizeEl.textContent = `${w} × ${h}`;
    // The size label sits just outside the top-left corner; flip it inside at a screen edge
    const labelTop = sel.top > 26 ? sel.top - 24 : sel.top + 4;
    sizeEl.style.left = Math.min(sel.left, window.innerWidth - 90) + "px";
    sizeEl.style.top = labelTop + "px";
  }

  function clearSelection() {
    dragging = false;
    pressAt = null;
    moved = false;
    origin = null;
    cursor = null;
    draw();
  }

  function moveGuides(clientX, clientY) {
    if (submitted) return;
    // Don't draw the crosshair while the left button is down (a rect exists); the rest of the time it
    // follows the pointer - including the anchored state, where the user is aiming for the second corner.
    const show = !dragging;
    guideV.style.display = show ? "block" : "none";
    guideH.style.display = show ? "block" : "none";
    guideV.style.left = clientX + "px";
    guideH.style.top = clientY + "px";
  }

  async function submit(sel) {
    submitted = true;
    hint.textContent = t("正在裁剪…");

    const box = shot.getBoundingClientRect();
    const scaleX = frame.width / box.width;
    const scaleY = frame.height / box.height;

    const args = {
      x: Math.round((sel.left - box.left) * scaleX),
      y: Math.round((sel.top - box.top) * scaleY),
      width: Math.max(1, Math.round(sel.width * scaleX)),
      height: Math.max(1, Math.round(sel.height * scaleY)),
    };

    try {
      await call("finish_selection", args);
    } catch (err) {
      // Submit failed: roll the state back so the user can draw again (Rust also releases the snip
      // flag, so re-pressing the hotkey works too)
      submitted = false;
      fail(err);
      return;
    }

    // The submit succeeded; what follows just hides the overlay. If this fails it must not be
    // reported as "snip failed" - the screenshot and the panel are already in place, only this
    // overlay didn't close. So the copy talks about exactly that, and submitted stays true:
    // redrawing on a stale overlay is pointless.
    try {
      await call("dismiss_overlay");
      // Hide first, then drop the background image: the window is reused, never destroyed, so
      // release the bitmap explicitly instead of keeping a multi-MB fullscreen image in the webview.
      shot.removeAttribute("src");
    } catch (err) {
      call("report_js_error", { message: `[overlay] failed to dismiss overlay: ${err}` }).catch(() => {});
      hint.textContent = t("截图已经收到了，但这个遮罩没能自动关掉，按 Esc 关掉就行。");
      hint.style.display = "";
    }
  }

  document.addEventListener("mousedown", (e) => {
    // A null frame means capture failed and only the error is showing; keep it out of the snip flow
    if (submitted || !frame || e.button !== 0) return;
    dragging = true;
    pressAt = { x: e.clientX, y: e.clientY };
    moved = false;
    // A rect already exists (anchored by the previous click): **leave it alone**. On release, a plain
    // click confirms it and a move redraws (see mouseup / mousemove).
    if (origin !== null) return;
    origin = { ...pressAt };
    cursor = { ...pressAt };
    guideV.style.display = "none";
    guideH.style.display = "none";
    hint.style.display = "none";
    draw();
  });

  document.addEventListener("mousemove", (e) => {
    if (submitted || !frame) return;
    moveGuides(e.clientX, e.clientY);
    if (origin === null) return;

    if (dragging) {
      // Only count as a drag once the pointer moved far enough: this redraws, with the origin reset to
      // the mousedown point. (Without this, a drag would be read as a confirming click and clobber the anchored rect.)
      if (!moved) {
        if (Math.abs(e.clientX - pressAt.x) <= DRAG_SLOP && Math.abs(e.clientY - pressAt.y) <= DRAG_SLOP) {
          return;
        }
        moved = true;
        origin = { ...pressAt };
      }
    }
    cursor = { x: e.clientX, y: e.clientY };
    draw();
  });

  document.addEventListener("mouseup", async (e) => {
    if (submitted || !frame || !dragging || e.button !== 0) return;
    dragging = false;

    // Click or drag, both count as long as the rect is big enough:
    //   click (anchored origin + current pointer position) -> confirm
    //   drag (mousedown point + release position) -> same as before, done on release
    const sel = selection();
    if (!sel || sel.width < MIN_SIZE || sel.height < MIN_SIZE) {
      // Rect still too small: keep this point **as the anchor**, let the rect follow the pointer, and
      // wait for the next click to confirm. This used to clear everything and start over, so a single
      // click accomplished nothing and the hotkey had to be pressed again - a click should make progress.
      hint.textContent = hintAnchoredText();
      hint.style.display = "";
      draw();
      return;
    }
    await submit(sel);
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      // First clear any drawn/anchored rect; a second Esc closes the whole overlay.
      // Cancelling everything at once is too harsh - most of the time the user just wants to redraw.
      if (origin !== null) {
        clearSelection();
        hint.textContent = hintIdleText();
        hint.style.display = "";
        return;
      }
      call("cancel_snip").catch(() => {});
    }
  });

  // Right-click: show a small "ask without a snip" menu, for asking a text-only question -
  // picking it opens the panel directly (no image) and Rust dismisses the overlay.
  function showDirectMenu(x, y) {
    hideDirectMenu();
    const menu = document.createElement("div");
    menu.id = "direct-menu";
    const item = document.createElement("button");
    item.type = "button";
    item.textContent = t("直接对话（不用截图）");
    item.addEventListener("click", async () => {
      submitted = true; // this turn has been handed off; no more drawing
      hideDirectMenu();
      try {
        await call("open_panel_direct");
      } catch (err) {
        submitted = false;
        fail(err);
      }
    });
    menu.appendChild(item);
    document.body.appendChild(menu);
    // Place next to the pointer without spilling off-screen
    menu.style.left = Math.min(x, window.innerWidth - 220) + "px";
    menu.style.top = Math.min(y, window.innerHeight - 60) + "px";
  }
  function hideDirectMenu() {
    const menu = document.getElementById("direct-menu");
    if (menu) menu.remove();
  }

  document.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (submitted) return;
    showDirectMenu(e.clientX, e.clientY);
  });
  // Click elsewhere / Esc: close the menu first; Esc with no menu falls through to cancel
  document.addEventListener("mousedown", (e) => {
    if (e.target.closest("#direct-menu")) return;
    hideDirectMenu();
  });
  const origKeydown = (e) => {
    if (e.key === "Escape" && document.getElementById("direct-menu")) {
      hideDirectMenu();
      return true; // while the menu is open, Esc only closes it
    }
    return false;
  };
  document.addEventListener("keydown", (e) => {
    if (origKeydown(e)) e.preventDefault();
  }, true); // capture phase runs before the Esc handler below, so an open menu doesn't cancel the snip

  boot();
})();
