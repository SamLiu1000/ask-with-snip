# ask-with-snip

A little desktop utility for snipping the screen anytime and asking questions anytime. Press a global hotkey, the screen freezes, drag a box, type your question, and the answer streams into a floating panel beside it.

Implemented following `AI截图分析工具_开发规划报告 (1).md`. <!-- AI Screenshot Analysis Tool - Development Planning Report -->

## Current Scope

This is a "basic, usable" version with the scope narrowed down per the planning report:

| Item | Approach in this version |
|---|---|
| Platform | **Windows only** |
| Monitors | **Single screen only** (multi-monitor and mixed DPI are left for later) |
| Model API | **OpenAI-compatible protocol** — any provider works (domestic direct-connect providers can be used as-is); multiple configurations can be stored and switched at any time |
| Frontend | Plain HTML/CSS/JS, **no build step**, no npm dependencies |
| Config storage | Plain-text JSON at `%APPDATA%\com.askwithsnip.desktop\config.json` |
| Conversation history | `%APPDATA%\com.askwithsnip.desktop\history\` (includes the screenshot from that session; keeps at most the 200 most recent entries) |
| Multi-image conversations | One image per conversation by default; enabling "continue from previous" accumulates multiple snips into the same conversation |
| Backend | OpenAI-compatible API, **or** the embedded web version (DeepSeek / ChatGPT / Gemini / Claude, with per-account isolated login states, consuming no API quota); switch with one click in the panel's top bar |
| Window geometry | `%APPDATA%\com.askwithsnip.desktop\windows.json` (stored separately from the config; see below) |

## Running It

### How to Start

A shortcut has already been created — just double-click **ask-with-snip** on the desktop (it can also be found in the Start menu).

You can also double-click the executable directly:

```
src-tauri\target\release\ask-with-snip.exe
```

No installer — this program is portable; a single exe runs as-is, no installation needed.

**It has no main window**, and that is intentional: a snipping tool only makes sense resident in the background; having to hunt for a window and click a button every time would be tedious. So after launch, snipping is invoked in three ways:

| Method | Description |
|---|---|
| The "Start Snipping" button on the console window | Pops up automatically on first launch (no longer auto-pops afterwards; reopen it from the tray menu) |
| Global hotkey `Ctrl+Shift+A` | Press anytime to enter snipping; configurable in Settings |
| Tray icon | Left-click to snip directly; right-click for the menu |

Not finding the tray icon is normal: **Windows 11 hides newly appearing tray icons in the overflow area by default**. Click the `^` arrow next to the taskbar clock to see it; to keep it permanently on the taskbar, enable ask-with-snip in "Settings → Personalization → Taskbar → Other system tray icons".

Closing any window leaves the program running in the background; **to exit completely, use "Exit" in the tray icon's right-click menu**.

### The Startup Toast

Because there is normally no main window, "nothing happens" on the desktop after launch, which easily makes people think the program didn't start (or has already exited). So a small toast pops up in the bottom-right corner of the screen at startup, telling you it's standing by in the background and which hotkey to use (if the settings page or console was opened automatically at startup, the toast is skipped — there is already visible feedback then). It **disappears by itself after 6 seconds**; click it to dismiss it early — it never steals focus, so typing in another window won't be interrupted.

(Technical reason: this window is created with `focusable(false)`, so it cannot receive mouse-hover or keyboard events — only clicks. Verified in practice, which is why the toast has no hover-pause and no Esc handling.)

### First Run

The settings page opens automatically on first launch. Fill in three things:

1. **API endpoint**: enter it down to the `/v1` level; the program appends `/chat/completions` automatically. If you only enter the domain, `/v1` is appended automatically. If the provider's path is not `v1` (e.g. Zhipu uses `/api/paas/v4`), enter the full path per its documentation.
2. **API Key**
3. **Model name**: must be a model the provider currently offers that **supports image input**. The dropdown has presets for a few common providers, but model names change fast — check the provider's current docs before use.

Click "Test Connection" to confirm it works, then click "Save".

### Multiple Configurations

The top of the settings page is the **API configuration** list. Clicking "Save as New Configuration" stores the currently entered endpoint/Key/model as "Configuration 2", "Configuration 3", and so on. Afterwards, **clicking a chip switches to it, and the active one is highlighted** (switching persists immediately — no need to click Save again). The endpoint, Key, and model you enter always belong to the currently highlighted chip, so:

> First click the chip you want to change, then edit the fields below, then click Save.

"Rename" renames the current chip; "✕" deletes it (you can't delete the last one). A typical setup is one direct-connect domestic provider and one relay, clicking whichever path currently works.

### One Image Per Conversation, or Continuing From Previous

By default, **every snip starts a new conversation**: contexts don't affect each other, each image is understood from scratch, which uses the fewest tokens and is least likely to get mixed up.

But for scenarios like reading a long document or reviewing long code — "snip a section, ask a question, snip another, keep asking" — the model needs to remember what it saw before. So there's a toggle ("Continue the current conversation after snipping" under "Usage" on the settings page, or the "Continue from previous" button next to the screenshot in the Q&A window — located above the Ask button; the settings page has the same toggle, and clicking it persists immediately):

| Toggle | New screenshot | Cost |
|---|---|---|
| Off (default) | Starts a new conversation | Each request carries only that one image |
| On | Appended to the current conversation | Every previous image is re-sent with each request; the longer the accumulation, the more expensive |

Continuing requires that **the current conversation has already had a question asked**. If it only holds a freshly snipped image with no question yet, the new image replaces it — with no exchange at all, accumulating is pointless. The rounds on the panel tagged with "New screenshot" are the ones carrying their own images.

### Building From Source

Requires Rust (MSVC toolchain), Node.js, and the WebView2 runtime. To build:

```bash
cd src-tauri
cargo build --release
```

Or use the Tauri CLI (use this when you need to generate an installer):

```bash
npm install
npm run tauri build        # output goes to src-tauri/target/release/bundle/
```

The frontend is pure static files; after editing `.html/.js/.css` you must re-run `cargo build` — the assets are embedded into the binary at compile time. For fast frontend iteration during development, temporarily change `frontendDist` in `tauri.conf.json` to a `devUrl`.

### Changing the Icon

The icon workflow is "one master image → CLI generates the full set of sizes"; no need to hand-edit `icon.ico` / `icon.icns`:

```bash
python tools/make_icon.py                                     # redraw the 1024 master image
npx tauri icon src-tauri/icons/source-icon.png -o src-tauri/icons
```

The master image is at `src-tauri/icons/source-icon.png`; the artwork comes from `tools/make_icon.py` (a selection marquee mid-drag + the letter A in a decorative face). To change colors or proportions, just edit the constants in that script.

`build.rs` also adds an extra `cargo:rerun-if-changed=icons` line: tauri-build only watches `tauri.conf.json` by itself, so without this declaration, after changing the icon cargo still considers the build script fresh and keeps reusing the cached Windows resources containing the old icon — the symptom being that the exe and tray icon never change no matter how many times you rebuild.

## How to Use

- **`Ctrl+Shift+A`** (configurable in Settings) brings up the snip overlay, or **left-click the tray icon**
- Select a region; both gestures work: **hold the left button and drag** (release to confirm), or **click once to set the start point, move the mouse, then click again to confirm** — use the latter when you don't want to keep holding. The clicked-out box follows the mouse, with its dimensions shown live in the top-left corner
- `Esc` **first cancels the box that has been drawn/clicked out**; if no box exists, pressing it again closes the whole overlay. When a box exists, holding the left button and dragging just draws a new one — no need to cancel first
- Type your question in the floating panel; Enter sends, `Shift+Enter` inserts a newline
- Four preset instruction buttons at the top (Translate / Explain / How to fix this error / Summarize)
- The same screenshot can be followed up with **follow-up questions**; the conversation context carries over
- **Send multiple images at once (film strip)**: snipped images line up in order on that strip of "film". **With "Continue from previous" on, snipping again automatically appends to the end**; with it off, ordinary snips always **start a new conversation**, and any unsent images on the old strip are **moved into the new conversation's film strip** (no image is lost; conversation boundaries follow the toggle). Clicking `+` explicitly appends regardless of the toggle (to discard, delete per-conversation or click the `✕` on an individual image — see below). The `+` to the left of the screenshot preview just **collapses the panel**, letting you browse as usual and press the hotkey (`Ctrl+Shift+A`) yourself when you want to add another image. The difference between "Re-snip" in the top bar and `+`: "Re-snip" immediately enters snipping and collapses the panel; `+` only collapses it.
- **Click a film frame to select/deselect** (only highlighted ones are sent); when you ask, all selected images are sent to the model **in one shot**. After the question is answered, **the sent frames stay on the strip** (dimmed, marked "sent"), with newly snipped ones lining up behind them; unselected, unsent ones stay on the strip waiting for next time.
- **Each image has a `✕` in its top-right corner** (appears on hover): on an unsent image it means "don't send this one"; on an already-sent image it **deletes it from that round's record** (subsequent follow-ups and regenerations won't include it; already persisted).
- The film strip **spans the full width and is only one row tall**: when there are many screenshots, a thin scrollbar appears below (if hidden, an image cut off at the edge looks like a rendering bug); the scroll wheel also works while hovering over it. Thumbnail height + scrollbar height must fit into the top bar's net height — if they don't, the bottom edge of images gets clipped; the tests pin this down. The top bar's height is fixed and does not grow with more screenshots (that would blow the layout apart). The old right-side text "x images will be sent · crop 1200 × 900" has been removed — whichever frames are highlighted on the strip are exactly the ones that will be sent, and you already saw the dimensions at snip time.
- Pressing the hotkey to snip again starts a **new conversation** by default (the previous one is stored in the history list on the left). To have the new screenshot **append to the current conversation**, click the "Continue from previous" button next to the screenshot (the settings page has the same toggle). The "Continue from previous" button sits **above the Ask button** (the same toggle also lives on the settings page).
- Right-clicking the tray icon shows a menu: Start Snipping / Conversation History / Open Console / Settings / Exit

### The Q&A Panel

- **Resizable by dragging**: move the mouse to any edge or corner of the window (the cursor becomes a double-headed arrow) and drag.
- **Both size and position are remembered**: they are written to `windows.json` as you move or resize, and recorded once more on exit. So the panel reappears where you put it on the next snip (including after restarting the program), rather than wandering with the selection. Only if it has never been placed (first use) does it auto-position next to the selection. Same for the settings page and console window. To return to auto-positioning, just delete `windows.json`.
- **The left side is the conversation history**: each snip is one entry. Click one to bring it back up, and **you can continue asking follow-ups directly** (that screenshot is carried along). Hovering shows a `✕` in the top-right to delete that entry; "Clear" at the top of the list deletes all.
- `✕` just hides the window (the program stays in the tray); `Esc` does the same.
- "Copy" in the top bar copies the whole Q&A exchange to the clipboard; "Re-snip" hides the window and immediately enters the next snip.
- **All buttons look the same**: size and appearance come from one base style (12px / 4×10 / border-radius 6); only a primary action like "Ask" is larger and uses the accent color, and toggles that are "on" (Continue from previous, backend switched to web version) share the same highlight. Add new buttons by reusing the existing ones — don't hand-roll sizes.
- **Toggle on/off states are indicated with monospace symbols**: "Continue from previous" is `● Continue from previous` / `○ Continue from previous`. This isn't decoration — **the text length of both states must be identical**: previously the on state was "Continue from previous ✓", and that one extra character widened the right-hand column, shrank the input box, and made the whole row jump every time you clicked. Likewise, the backend toggle in the top bar has a fixed minimum width ("API" and "Web ✓" differ in length).
- **While waiting, the button becomes "Cancel"**: clicking it while the input box is **empty** stops the run; whatever partial answer has already appeared stays on the panel and is not wiped by the cancellation. The status bar keeps reporting elapsed time (`Analyzing… 12s`), so "stuck" and "running" are distinguishable at a glance.
- **Want to keep asking while busy? Just type and send — it queues**: clicking "Ask" or pressing Enter with text in the input box puts that question in the queue (status bar shows "Queued"), and it is sent automatically once the current round finishes. Previously the button was "Cancel" at this point and you had to click again to send; that "cancel + resend" combo could send the same freshly snipped image out twice — queueing closes that hole.
- **Chain of thought gets its own section**: for reasoning models (e.g. `deepseek-reasoner`), the "thinking" part displays in a collapsible block called "Thinking Process" above the answer — expanded by default, collapsible. Non-reasoning models don't show this block. Once the chain of thought starts arriving, the status bar changes from "Analyzing" to "Thinking".
- **After an answer you can edit the question or regenerate**: two buttons below the last round — "Edit Question" rewrites that round's question in place and asks again; "Regenerate" keeps the question and redoes the answer. Both operate only on the **last round** (editing earlier ones would invalidate all later rounds — that door stays closed for now); while running they collapse, because that round hasn't landed yet. Both backends support them; for differences on the web-version path see the section below.
- That button in the top bar is the **backend toggle**: showing "API" means it uses the API; showing "Web ✓" means the embedded web version. One click switches it — no trip to the settings page needed. See the next section.

### When API Quota Runs Out: Use the Web Version (asking stays in this same panel)

Click "API" in the top bar → it becomes "Web ✓". Afterwards, **everything about asking and displaying is unchanged**: still snip, still type in the same panel, the answer still streams in here. There is only one difference — the question isn't sent to an API; instead, the program **operates the embedded `chat.deepseek.com` page on your behalf**: pastes the screenshot into the input box, types your question, presses send, then reads the answer back and displays it in the panel.

So it **consumes no API quota and needs no API Key** — it uses your own web account.

- First use requires login: click "Show Web" in the top bar to bring up that window and log in once (phone number + verification code). The program then uses that login state in the background; the window is normally hidden and won't pop up to interrupt you.
- **Only the backend changes, not the feel**: everything on the API path (including "Continue from previous", follow-ups, history) is untouched; both paths share the same panel and the same conversation history. Click the button again to switch back.
- "Show Web" is hidden in "API" mode, since it would be meaningless.
- On the web version, **context is maintained by the site itself**: the same conversation continues, and the site's memory is the context; with "Continue from previous" off, the program clicks "New Chat" once before sending, matching your habitual behavior.
- The login state is stored in the window's own data directory (`%LOCALAPPDATA%\com.askwithsnip.desktop\EBWebView\`), kept separate from your browser's, so you only log in once.
- When something goes wrong, click "Show Web" first: whether the site wants login, wants verification, or has been redesigned is immediately visible. The program's error messages will also point you there.
- **Multiple accounts, multiple sites**: the web version supports DeepSeek, ChatGPT, Gemini, and Claude (two dropdowns in the top bar: pick account, pick site). **Each account gets its own WebView2 data directory** with isolated login states — you can be logged into DeepSeek and ChatGPT simultaneously and switch accounts without re-logging-in. "＋ Add Account" adds one; after switching sites, click "Show Web" and log in once.
- **All site adaptations live in the site registry at the top of `src-tauri/src/webchat.rs`** (selectors, input box type, the "Deep Think" toggle, …). The DeepSeek set was calibrated against real sessions; the other three were written with best-guess values based on each site's known page structure and **have not been calibrated under a real login state** — run the probe (`--site chatgpt` etc.) to take a look, and if it doesn't match, fix the registry per the printed class names.

On this path, "Cancel" and "Thinking Process" both work too, each in its own way:

- Clicking "Cancel" also clicks the site's **stop** button on the page. Without that, the page would finish the whole answer in the background, burning your account's quota for nothing — so cancel is two actions, not just disconnecting the panel side.
- **Each web-version conversation records the corresponding session URL on the site** (after each round, `/a/chat/s/<id>` is fetched back from the page and stored in the record). Afterwards, opening an old record to continue, regenerating, editing the question, and "Continue from previous" all **deep-link straight back into that session and keep chatting there** — the site already holds the context, so prior rounds are not re-sent one by one. Conversations without a recorded URL (old records, or a question that never got an answer) start a fresh conversation — better to lose context than to cross into someone else's session. "Regenerate / Edit Question" is therefore as fast as chatting on the site yourself.
- **The program turns "Deep Think" on for you**: in site-driven new conversations this toggle defaults to off, the model doesn't think at all, and there's no chain of thought to read (confirmed with real sessions: manual chats have thinking blocks, driven ones didn't). So when starting a new conversation, the program checks the toggle first and **clicks it only if off** (judged by the `--selected` modifier class; if already on, it's left alone); continuing rounds are not touched and keep that conversation's own mode. If you don't want thinking, the follow-up rounds in the panel (with "Continue from previous" on) won't touch the toggle.
- The web version's chain of thought is identified by the site's `.ds-think-content` container (found with the probe in a real session; the class name carries no hash). It reads `textContent`, so **it reads even when the thinking block is collapsed**. If the class name ever changes, the program falls back to the old approach — treating "the message's text outside the answer" as the thinking process; that fallback has a filter word list that drops button labels like "Deep Think" and "Regenerate", and shows nothing if shorter than 40 characters.

## Cost and Privacy

- Screenshots are compressed before sending according to the "long-edge limit" in settings (default 1568px). Vision models compress to roughly that scale internally anyway; compressing first just avoids paying token cost for pixels you won't use. The floating panel shows "Crop W × H · Send W × H"; if the two differ, compression took effect.
- **Follow-ups don't re-send the image**. Each round carries only **its own** snipped image: a follow-up without re-snipping carries no image, since it's already in the context.
- **"Continue from previous" keeps accumulating images**. With it on, snipping five times in a row means the fifth request sends all five images (that's the price of "the model can see the earlier sections"); tokens and latency both rise accordingly. When done with a section, click back to start a new conversation — or just turn the toggle off.
- **Same for the film strip when sending multiple at once**: all selected frames go out as this round's images, each compressed to the long-edge limit. More images means more tokens and a greater chance of hitting the provider's per-request cap; deselect any you don't want.
- Screenshots are sent to **whichever provider you configured yourself**. The tool itself goes through no intermediary server.
- Same on the "web version": the screenshot is pasted **by the program into that page's input box on your behalf**, and from there it's sent to the site from your browser session — no server of ours in between. How the data is used is governed by the site's terms.
- The API Key is stored in plain text in the config file; mind that file's permissions.

## Troubleshooting

### UI issues (window should appear but doesn't, hangs)

**Process logs are written to file by default**: `%APPDATA%\com.askwithsnip.desktop\trace.log`, rewritten on each launch. The `[+ 12.345s]` at the start of each line is seconds since the first log line (for reading ordering and gaps — easier than absolute timestamps).

This file exists because problems like "the web version already answered but the program never received it" **only manifest on the user's side**: on the machine where it happens, the log is often the only clue available. After reproducing, just read this file.

To also see logs in the console, set the environment variable and restart:

```bat
set SNAPI_TRACE=1
ask-with-snip.exe
```

Logs then also go to stderr; uncaught frontend exceptions are printed with a `[webview]` prefix (the webview has no console — without this you'd never see frontend errors) — those go to stderr only, not to the file.

### Other Common Cases

| Symptom | Fix |
|---|---|
| Window off-screen / size recorded wrong | Delete `%APPDATA%\com.askwithsnip.desktop\windows.json`; next open returns to "auto-position next to the selection" (positions that no longer fit are ignored automatically when monitors or resolutions changed) |
| Unwanted entry in the history list | Hover it and click `✕`; to clear everything click "Clear" at the top of the list, or delete the `history\` directory directly |
| Config mangled (e.g. a corrupted Key) | Edit `config.json` directly, or delete it and redo first-time setup |

### Screenshot issues (snip comes out black, wrong dimensions)

First verify screen capture itself, decoupled from the UI:

```bash
cd src-tauri
cargo run --example probe
cargo run --example probe -- 1000 800 800 600    # crop another region to compare
```

Samples are written to `ask-with-snip-probe\full.jpg` in the temp directory; open and look. This step distinguishes a capture problem from a coordinate-conversion/window problem.

### API issues

You can exercise the whole API path without a real Key and without spending money:

```bash
python tools/mock_openai_server.py 8899      # terminal 1
cd src-tauri && cargo run --example api_probe # terminal 2
```

The mock server returns streaming answers in OpenAI format and writes a summary of each received request to `tools/mock_requests.jsonl`, focusing on "how many images this round carried and how large". Point the endpoint in settings at `http://127.0.0.1:8899/v1`, enter any Key, and you get the full experience in the UI.

### Web version issues (error on asking, or the answer can't be read)

On the web version the program works **on the page** (pasting images, typing, clicking send, reading answers), so it depends on the page's DOM structure. There are two probes here, neither requiring any UI clicks:

```bash
cd src-tauri
cargo run --features probe --bin webchat_probe          # recon: where the page landed, whether the input box / answer container still exist
cargo run --features probe --bin webchat_probe -- --dry # dry run: actually pastes image + types, but doesn't click send
cargo run --features probe --bin webchat_probe -- --think   # chain of thought: walks the sessions one by one, finds the thinking-process container
cargo run --features probe --bin webchat_probe -- --collect # reads a few rounds with the driver's own answer-reading JS, checking body and chain of thought come out
cargo run --features probe --bin webchat_probe -- --toggle  # what the "Deep Think" toggle looks like (clicks it on then back to restore)
cargo run --features probe --bin webchat_probe -- --actions # what the row of buttons under a message looks like (clicks "Edit" once, doesn't send)
```

- **Recon** opens a window, prints the start of the page body, the input box, the session list, and the answer container's class names, then closes itself. No input box visible = the site was redesigned or you're stuck on a verification page.
- **Dry run** uses **the driver's own JS** to walk "wait for ready → paste image → type question", only skipping the send click — because sending would push something to the site. It reports the character count in the input box and the image count in the container; both having values means the earlier steps hold on the real page. Whichever step breaks, fix the selector constants at the top of `src-tauri/src/webchat.rs`.
- **The chain-of-thought probe** opens the sidebar (in a narrow window it doesn't render at all), walks your historical sessions one by one, prints elements whose class names contain `think`/`reason` along with their lengths, and reports whether the thinking block and the answer node hang under the same `.ds-message` — that's exactly what the driver relies on to read the chain of thought. `SEL_THINK` was found this way (`.ds-think-content`, no hash).
- **`--collect`** is the only one that validates the "read" step: it uses the driver's own script to read ten rounds from an existing session, printing body and chain-of-thought deltas separately. Add `--here` to use the currently open page; without it, it clicks the first session first. It only reads — no pasting, no typing, no sending.
- **`--toggle`** prints the current class names of the "Deep Think" toggle, clicks it once and prints again (the on/off states differ by the `--selected` modifier class), then clicks back to restore. The driver's "only click when it should be on" relies on this marker. It briefly changes the toggle's state but always restores it when done.
- **`--actions`** inspects the row of action buttons under a message (copy / regenerate / edit): they **are icon buttons** with no text and no `aria-label`, so searching by text finds none of them. This mode prints each button's `<use href>` or path, and which message's row it sits on. **"Regenerate/Edit Question" deliberately avoids clicking them because this probe talked us out of it** — they can't be told apart reliably, and mis-clicking could hit read-aloud or share. It only clicks "Edit" once (the page's own edit state, not sent); it does not click "Regenerate".

These probes are **essentially read-only**: walking history and reading the DOM all fall within "nothing sent to the site". The only page-state changes are `--toggle` (restored after clicking), `--actions` (one "Edit" click), and the image pasting in the dry run; the only thing truly sent is the send click, and that step can only be pressed by the user in the panel.

All selectors are concentrated at the top of `webchat.rs` (`SEL_COMPOSER` / `SEL_SEND` / `SEL_ANSWER` / `SEL_THINK` / `NEW_CHAT_TEXT`); when the site changes, only these lines need touching, not the logic. These selectors were probed from the real page — run the recon above before changing them.

Answers are **converted to Markdown inside the page before being passed back** (not raw innerText): headings, lists, tables, code blocks, formulas, and reference links all survive thanks to this step, so the panel can render them. The converter is `MARKDOWN_JS` in `webchat.rs`; with `--convert` you can validate it against **real answers from your own history** (read-only, sends nothing):

```bash
cargo run --features probe --bin webchat_probe -- --convert
# the sidebar is collapsed by default and the probe may fail to click a history conversation; in that case open one directly by deep link:
cargo run --features probe --bin webchat_probe -- --convert \
  --url https://chat.deepseek.com/a/chat/s/<your session id>
```

Also: **when the sidebar is collapsed, the "New Chat" button doesn't render**, so if "start a new conversation" can't click it, it falls back to reopening one from the home page (one page reload — slower, but contexts don't cross). This fallback is also verified in the dry run.

### How Answers Travel Back From the Page (read this before touching it)

The page **pushes while streaming**: it writes the current body text into the window title, and the Rust side receives and stitches it. There are two pitfalls here, both found the hard way, documented in comments in `COLLECT_JS` in `webchat.rs`:

- The site **redraws while streaming**. With the cursor sitting inside a half-written table row, the row's tail first gets patched to `| game |`, then next round becomes `| game main executable |` — meaning the assumption "new text always appends after old text" **does not hold**. The earlier implementation re-pushed the entire document whenever the assumption broke, so a 600-character answer once landed on disk as four duplicates of 157+293+395+602. The current rule: if it connects, push the delta; if it doesn't, re-send the whole thing and the other side **replaces**.
- The title channel **is not reliable**: adjacent title changes get coalesced by the browser, and losing one payload means a permanent hole in the delta. So after the answer finishes (1.5 seconds with no change), there's also a **verification re-read**: the held version's length is reported to the page, and if it doesn't match, the whole thing is re-received. What this step guarantees: "the answer persisted in the panel is identical to the one on the web page".
- **There is exactly one state shape on the page side** (`STATE_SHAPE_JS`): both compose (which sets the baseline) and collect (which reads deltas) reference it. This was learned painfully — the two sides once had separate initializations, a change to collect's shape missed compose's, and collect threw a TypeError on every run, **not a single report getting through**: the web page finished generating the answer (chain of thought included) while the panel stayed stuck on "Analyzing" forever, and with zero reports coming through you couldn't even see where it broke. Timeout errors now include "how many reports were received and what body length the page last reported", making the next occurrence easier to diagnose; collect also auto-rebuilds when it finds a stale shape. `compose_and_collect_agree_on_the_state_shape` and `[6b]` in `webchat_collect_test.cjs` guard against a recurrence.

Both of these have tests watching them, and the tests run the **verbatim JS** from `webchat.rs` (extracted and fed to a stub page):

```bash
node tools/webchat_collect_test.cjs
```

It reproduces exactly that four-times-duplicate scenario and asserts the stitched body matches "the text on the page" character for character. If you're touching `COLLECT_JS`'s push logic, run it first.

The probe is deliberately a bin rather than an example: Tauri's Windows application manifest attaches to the bin target, and an example lacking it crashes at load time (`STATUS_ENTRYPOINT_NOT_FOUND`). It is gated with `required-features`, so `cargo build` / `tauri build` don't include it.

## Verify It Yourself

None of these need a real API Key, and none cost money:
```bash
cd src-tauri && cargo test          # Rust: URL assembly, SSE parsing, config normalization, history indexing, crop coordinates
npm run test:frontend               # frontend logic: history list, follow-ups, streaming render, resize handles, multi-config switching
python tools/mock_openai_server.py 8899   # local mock API (pair it with the "local mock API" configuration in settings)
```

The first two are pure logic tests running under Node and cargo — no windows, no WebView needed:

- `tools/panel_logic_test.cjs` runs the real `common.js` + `panel.js` against a minimal DOM stub, asserting things like "which command clicking this history entry should send", "where a streaming delta should land", and "whether the eight resize handles report the right directions".
- `tools/settings_logic_test.cjs` similarly validates the configuration list: chip switching, skipping occupied names when saving as new, blocking rename collisions, refusing to delete the last config, and not wiping form fields that weren't shown on save.
- `tools/overlay_logic_test.cjs` runs the real `src/overlay.js` on a minimal DOM stub, using mouse event sequences to assert the click → move → click box, the drag box, that a too-tiny click doesn't count, and that `Esc` cancels the box first and the overlay only on the second press. Every one of these came from a real user bug report ("clicking bounces me back", "endless snipping").
- `tools/webchat_collect_test.cjs` extracts the **verbatim JS** from `webchat.rs` that reads web-version answers, feeds it to a stub page, reproduces scenarios like "the site redraws a half-written table row", and asserts the stitched body matches the page's text exactly. Bugs like "the answer differs from the web page" on the web version are all caught by this case.

The window, screen-capture, and tray layers can't be auto-asserted; they're checked manually via `trace.log` (or stderr with `SNAPI_TRACE=1`) plus the probes above. Snipping has two switches dedicated to diagnosis and completely inert otherwise: `SNAPI_TRACE=1` (logs also go to stderr) and `SNAPI_AUTOSNIP=1` (auto-enters a snip 1.5 seconds after launch, saving you from pressing the hotkey during verification). On-screen text can be read out via OCR instead of by eye:

```bash
# screenshot + recognize text (WinRT OCR). With -W/-H it captures a region; or pass -Path for an existing image
powershell -NoProfile -ExecutionPolicy Bypass -File tools\ocr_screen.ps1 -X 1115 -Y 85 -W 987 -H 1015
# add -Boxes to also print each word's **screen coordinates** (tab-separated); take its center point when you want to click a line of text
powershell -NoProfile -ExecutionPolicy Bypass -File tools\ocr_screen.ps1 -X 0 -Y 0 -W 3840 -H 2160 -Boxes
```

All the panel's button labels ("Continue from previous", "Ask", "Re-snip", "Show Web", …) can be read this way, letting you verify button positions and wording changes. The film strip itself is images with no text, so "how many images reached the panel" currently can only be confirmed via the payloads in `trace.log` and the test rigs above — the status-line text for this was removed. The web-version path has two more probes that need a real desktop/network:

```bash
cd src-tauri
cargo run --features probe --bin webchat_probe          # can the embedded window load the remote page (opens a window)
cargo run --features probe --bin webchat_probe -- --dry # paste image + type (no send), validating the driver's early steps
cargo run --features probe --bin webchat_probe -- --collect  # read an existing session, validating body + chain of thought come out
```

**The only step never auto-verified is the "click send" moment**, and the real end-to-end streaming read after sending — that step pushes something to the site, so it can only be pressed by the user in the panel. Every step before it (ready wait, image paste, typing, body read, chain-of-thought read, final verification re-read) has been verified on the real page (`--collect`). The same goes for "Regenerate / Edit Question" on the web version: each internal step is one of the verified actions above, but **the whole "reopen a conversation and re-ask" flow cannot be auto-run** (it would send several messages from your account). On the API path these two features are purely local (requests rebuilt from rounds) and have tests watching them.

## Code Structure

```
src/                       frontend (pure static, no build step)
  common.js                Tauri call wrappers, Markdown rendering, error reporting
  hub.html/js              console: gives users who "can't find the entry" a visible button
  toast.html/js            startup toast: the small bottom-right notification that dismisses itself after a few seconds
  overlay.html/js          snip overlay
  panel.html/js            question-and-answer floating panel
  settings.html/js         settings page
  style.css                shared styles for all pages
src-tauri/src/
  lib.rs                   window orchestration, hotkey, tray, IPC commands
  capture.rs               freeze-frame capture, region cropping, compressed encoding
  api.rs                   OpenAI-compatible API (including streaming parser)
  webchat.rs               embedded web version as backend: paste/type/send/read answer (selectors all at the top of the file)
  config.rs                multiple API configurations + backend toggle + window geometry read/write
  history.rs               conversation persistence (index + one file per conversation)
  bin/webchat_probe.rs     web-version probe (only built with --features probe)
src-tauri/examples/
  probe.rs                 screen-capture path probe
  api_probe.rs             API path probe
tools/
  mock_openai_server.py    local mock API
  panel_logic_test.cjs     panel frontend logic tests (Node + DOM stub)
  settings_logic_test.cjs  settings page logic tests (same)
  make_icon.py             draws the icon master image (drag marquee + letter A)
  check_icon.py            measures icon geometry, checks legibility at 16px
```

### Three Key Implementation Decisions

**Snipping is "freeze first, then crop on the static image", not "snip first, then capture that region live".**
When the hotkey fires, the full screen is captured first, that image fills the overlay window, the user draws a box on the static image, and cropping happens from the lossless frame in memory. This way the overlay window can't photograph itself into the screenshot, and there's no race or flicker from "hide the overlay then capture".

**All windows are reused, never destroyed; closing means hiding.**
This lesson was taught by a bug. The original approach was "close the old one, create a new one", and the second snip always failed with `a webview with label ... already exists`. The root cause wasn't "not closing fast enough": **destroying a window from within its own IPC call** (the overlay's JS calls `dismiss_overlay`, destroying itself), leaves the webview in a zombie state of "native window already destroyed, but the label still occupied" — `get_webview_window` keeps returning `Some`, waiting any amount of time doesn't help, and a window with that label can never be created again. The fix: windows are created lazily and afterwards only hidden / repositioned-resized / refreshed via events. It's also faster — each snip saves a window creation.

**Every command that creates or shows a window is `async`.**
Tauri's synchronous commands execute inline in the main thread's message handler, and calling `WebviewWindowBuilder::build()` at that position deadlocks: creating a WebView2 child window requires the message pump to keep running, but the message handler is occupying the main thread. The symptom is deceptive — the window handle is created but it never shows, the whole main thread hangs, and even the tray stops responding. Likewise, tray menu events must not create windows directly. Going async also moved CPU-heavy work like cropping and JPEG encoding off the main thread as a bonus.

**The embedded web-version window gets no permissions at all.**
It's the only window loading a remote site, so the `windows` entry in `capabilities/default.json` **lists the local windows individually** (overlay / panel / settings / hub / toast) rather than using `"*"`. Tauri already gates remote origins separately (IPC is unreachable without configuring `dangerousRemoteDomainIpcAccess`); listing names adds one more layer on top: if remote-origin restrictions were ever relaxed, this window still couldn't invoke a single command. The cost: remember to add new local windows to this list.

**Data on the web-version path can only leave through `document.title`.**
The remote page has no Tauri permissions (previous item), so it cannot call back into our commands, and "reading the answer" needed another exit. `document.title` is the only channel that requires no permission: the page writes deltas into the title, and the Rust side receives them via `on_document_title_changed` (an event Tauri doesn't wire up by default — we registered it when creating the window, so the window's own title isn't disturbed). The title has two empirically observed quirks: **newlines get swallowed** (payloads must be joined into one line) and **there's a length cap** (so only a small chunk is pushed each time, the rest next round — see `CHUNK` in `webchat.rs`). By the way: don't try stuffing results into a URL fragment and reading `window.url()` — it's not guaranteed to update.

**Window geometry lives in its own file (`windows.json`), not together with `config.json`.**
Window sizes are written by the program at any moment, while `config.json` is saved as a whole-document overwrite from the settings page. If they shared a file, a user who dragged a window and then clicked "Save Settings" would have their just-dragged sizes overwritten by the stale data from the settings page. Storing them separately keeps the two from interfering.

**Images attach to "rounds", not to "conversations".**
Once "continue from previous" exists, a conversation can contain several images (one more per snip). If images were still stored on the conversation (one slot), the second image would displace the first, and replaying history would produce "old question with new image" — the model reads it wrong outright. So each round carries its own `image_b64`, and requests walk the rounds, with each image following its own question; the conversation-level `image_b64` is only a staging slot for "just snipped, not yet asked", moved into a new round (`start_turn`) the moment a question is asked. Old-format records (image stored on the conversation) are migrated to the first round on read, so older records don't lose their image.

**Window geometry is recorded "while moving", not on close.**
Initially geometry was saved only when the window received a close request (clicking ✕). The result: users often move the window and go straight back to snipping — or exit the program entirely — never clicking ✕, so that move was wasted, and the next snip's panel would "run" back next to the selection again. Now the move/resize events persist immediately (throttled at 300ms, so dragging doesn't hammer the file), with one final save on exit as a backstop.

**Borderless-window resizing is hand-rolled.**
Windows gives borderless windows no native resize border, so the panel places 5px transparent strips (`.rz`) on all four edges and corners; while held, they call `startResizeDragging("NorthEast")` to hand dragging to the system resize loop, feeling the same as a native window (otherwise you'd have to hand-write mouse-displacement math, easy to get wrong across monitors and DPI changes). This requires the `core:window:allow-start-resize-dragging` permission.

### Window and Exit Lifecycle

- The program lives in the tray; **closing any window never exits the program** (Tauri's default is "exit when the last window closes"; that is intercepted here with `RunEvent::ExitRequested`: a `code` of `None` means it was triggered by a window closing, and `prevent_exit()` is called directly).
- True exit has only two paths: "Exit" in the tray icon's right-click menu, or "Exit Program" in the console window. They go through `app.exit(0)` with an exit code and are not blocked by the interception above.
- So the hotkey supports **continuous snipping**, each one a fresh conversation; the panel window is reused.
- The web-version window is part of this system too: clicking `✕` only hides it; the page and login state persist, so clicking "Web" next time comes back instantly.
- When history exceeds 200 entries, the oldest are dropped automatically (along with their screenshot files).

## Known Limitations

- Single screen only. On multi-monitor setups only the main screen is captured; regions on other screens can't be selected.
- No magnifier, no edge snapping during snipping, and no arrow-key fine-tuning. For tiny text in a screenshot, zoom in before snipping.
- Conversation history can only be deleted one by one or cleared entirely — no search, no export.
- Records store that session's screenshot (base64 JSON); 200 entries can occupy tens to hundreds of MB — clear occasionally if that bothers you.
- No rate limiting or usage statistics. If you're worried about the bill, set a spending cap on your provider's side first.
- On the web version, **the site's page structure is a dependency** (class names of the input box, send button, and answer container). A site redesign can break it; the symptom is an error telling you to click "Show Web". The fix is editing the selector constants at the top of `webchat.rs` — a recon probe exists specifically for this; just fix them per the printed class names. This is the unavoidable price of "no API quota consumed, yet the experience stays the same": without touching the page you'd have to reverse-engineer the site's private protocol, which breaks even more often.
- On the web-version path, **the login state is a separate copy** (the webview's own data directory), not shared with your browser's, so it needs its own login; when it expires you must log in again (the program will point you there).
- The web-version window **is normally hidden**: the driver operates inside the page via script and doesn't depend on the window being visible. (Measured specifically: in a hidden window, `visibilityState` is still `visible`, and animation frames and timers are not throttled, so "running hidden" won't itself stop the site from rendering text. If you truly suspect throttling, change `ensure_webchat(app, false)` to `true` in `run_turn` in `webchat.rs` to let it run exposed.)
- On the web version, **occasionally "not a single character is read"** (the site has actually already answered). There are now four layers of safeguards:
  1. If no **body** delta arrives for ten seconds, the page is asked to re-send the whole thing (up to three times). What's measured is the body's own quiet time, not "any payload" — the chain of thought streaming in while the body stays silent is normal (the model thinks before speaking); substituting the chain of thought's activity for the body's would lose a silent body for good. No re-send while the page reports "still generating". **No longer requiring "the page says a body already exists"** — that clue itself comes from payloads, which are precisely the thing you might receive none of.
  2. The "no change for a while means done" threshold has two tiers: when the body hasn't started, wait 30 seconds instead of 1.5 (pauses longer than 1.5 seconds between deep-thought breaths are common; the old behavior would prematurely finish a round still thinking, with the panel reporting "no body at all" while the page was writing merrily along — that's exactly how it broke in practice); and while the page still reports "generating" (the stop button is present), it won't finish even more so.
  3. When the page-side collection script throws an exception, it also pushes out a payload with that run's **cause** (along with the actual body length on the page), landing in the log and in the timeout message. Previously one exception killed the whole run with zero reports getting out.
  4. When the body's HTML→Markdown conversion can't handle some content, it **falls back to plain text** (the body survives), and the failure count is logged — no longer letting one converter's defect sink the entire read.
  5. The "re-send the whole thing" path has a **baseline guard**: as long as no new answer node has appeared on the page, no re-send happens, preventing the **previous** round's answer from being pushed up as this round's ("new question with old answer" is harder to spot than reading nothing). The baseline is read from the page **before asking** (previously it was read after asking, by which time this round's answer node might already exist, and the guard would wrongly block the new answer as stale residue); if unreadable it's treated as 0 (i.e. no limit), so the normal path is never blocked.
  If it truly times out, the error includes "how many reports were received, how many answers the page last reported, the body length, and whether the collection script errored" — that one line tells you whether the page failed to render or the read-back broke. Earlier, this situation just said "you may need to log in", with no clue at all; it took a long time to diagnose.
- After starting a new conversation, it **waits for the page to stabilize before clicking the "Deep Think" toggle**, retrying once if the click misses. There was once a regenerated conversation with no thinking block — that click had fired too early.
- On the web version, the chain of thought and the body are **two independent streams**: the chain of thought is read by the thinking blocks' own count, not attached to the answer node — the model may think for a long time before speaking, and watching only the answer node means that whole stretch reads as nothing ("stuck on Analyzing, no chain of thought visible" comes from exactly this). The body is still pushed as deltas and stitched on the panel side.
- On the web version, content shown **during streaming** can occasionally differ slightly from the web page (the site redrew part of what it just output, and the redraw is pushed back in segments), but **the final verification re-read aligns the whole thing** — the answer persisted to disk, and the copy displayed in the panel after the answer ends, match the web page.
- On the web version, **the "click send" moment has not been auto-verified** (it pushes something to the site; only the user can press it). Every step before it — ready wait, image paste, typing, body read, chain-of-thought read, verification re-read — has been verified on the real page. Also, images and questions sent via the web version **consume your web account's quota**, not the API's.
- Packaging an installer (`npm run tauri build`) requires downloading NSIS over the network; if it fails, `target/release/ask-with-snip.exe` alone is still usable.
