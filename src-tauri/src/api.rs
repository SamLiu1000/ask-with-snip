use crate::config::Profile;
use crate::history::Turn;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

pub const EVT_DELTA: &str = "ai-delta";
/// Incremental chunk of the chain of thought (the "thinking" part of reasoning
/// models), delivered as its own event: the panel collapses and displays it separately
pub const EVT_REASONING: &str = "ai-reasoning";
/// Content already streamed out is invalidated; replace it wholesale with this payload.
///
/// When going through the web version, the site will **redraw the small segment it
/// just emitted** (half-finished table rows, late-arriving citation marks). At that
/// point the increments no longer line up, so the whole segment has to be resent once.
/// The payload is `{ answer?, reasoning? }` -- whichever stream is resent gets its field.
pub const EVT_REPLACE: &str = "ai-replace";
pub const EVT_DONE: &str = "ai-done";
pub const EVT_ERROR: &str = "ai-error";

/// A per-request cancellation switch.
///
/// It is a cloneable shared flag because "who wants to cancel" and "who is
/// streaming" live on opposite sides: the moment the command is pressed in the
/// hand, the side reading the stream sees it on the next loop iteration. **Do not
/// rely on a timeout as a fallback** -- a genuinely stuck stream may never give
/// you data, and all the user sees is "still analyzing" forever.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Something pushed out of one streaming request: body text, or chain of thought.
#[derive(Debug)]
pub enum Chunk {
    Text(String),
    Reasoning(String),
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// Assembles a user message. When images are present, uses the array form of
/// content (the OpenAI multimodal convention).
pub fn user_message(prompt: &str, images: &[String]) -> Value {
    if images.is_empty() {
        return json!({ "role": "user", "content": prompt });
    }
    let mut content = vec![json!({ "type": "text", "text": prompt })];
    for image in images {
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:image/jpeg;base64,{image}") }
        }));
    }
    json!({ "role": "user", "content": content })
}

/// Normalizes a user-entered proxy address into a URL reqwest accepts. An empty
/// string returns None (direct connection). A bare host:port ("127.0.0.1:7890")
/// is treated as an HTTP proxy, consistent with the hint on the settings page.
fn normalize_proxy(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.contains("://") {
        Some(raw.to_string())
    } else {
        Some(format!("http://{raw}"))
    }
}

fn build_client(timeout_secs: u64, proxy: &str) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs.max(10)));
    if let Some(url) = normalize_proxy(proxy) {
        let p = reqwest::Proxy::all(&url).map_err(|e| format!("Invalid proxy address ({url}): {e}"))?;
        builder = builder.proxy(p);
    }
    builder
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))
}

fn build_request(
    client: &reqwest::Client,
    profile: &Profile,
    body: &Value,
) -> Result<reqwest::RequestBuilder, String> {
    if profile.api_key.trim().is_empty() {
        return Err("This profile has no API Key set; please fill it in the settings first.".into());
    }
    if profile.model.trim().is_empty() {
        return Err("This profile has no model name set; please fill it in the settings first.".into());
    }

    let mut req = client
        .post(profile.endpoint())
        .bearer_auth(profile.api_key.trim())
        .header("Content-Type", "application/json")
        .json(body);
    for (key, value) in profile.extra_headers_map() {
        req = req.header(key, value);
    }
    Ok(req)
}

/// Assembles conversation turns into OpenAI messages.
///
/// Each turn carries only **its own** captured screenshot: follow-up questions carry
/// no image (the image is already in the context; resending is both expensive and
/// slow), while a new "continue with a fresh selection after the previous context"
/// turn carries its new image. So just walk the turns in order -- each image stays
/// attached to the question it belongs to and never drifts out of place.
pub fn messages_from_turns(system_prompt: &str, turns: &[Turn]) -> Vec<Value> {
    let mut messages = Vec::with_capacity(turns.len() * 2 + 1);
    if !system_prompt.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    for turn in turns {
        messages.push(user_message(&turn.question, &turn.all_images()));
        // A turn with no answer yet (question in flight) must not get an assistant message
        if !turn.answer.trim().is_empty() {
            messages.push(json!({ "role": "assistant", "content": turn.answer }));
        }
    }
    messages
}

#[derive(Debug)]
enum SseDelta {
    Text(String),
    Reasoning(String),
    Done,
}

/// Parses one SSE line.
///
/// The response formats of OpenAI-compatible implementations differ somewhat;
/// this handles the most common variations: comment lines / heartbeats (starting
/// with `:`) are ignored; `[DONE]` is treated as the end; an error embedded in the
/// stream is raised directly as a failure.
fn parse_sse_line(line: &str) -> Result<Option<SseDelta>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return Ok(None);
    }
    let data = match line.strip_prefix("data:") {
        Some(d) => d.trim(),
        None => return Ok(None),
    };
    if data.is_empty() {
        return Ok(None);
    }
    if data == "[DONE]" {
        return Ok(Some(SseDelta::Done));
    }

    let value: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        // Not JSON -- pretend we didn't see it; don't let one dirty line interrupt the whole answer
        Err(_) => return Ok(None),
    };

    if let Some(err) = value.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.to_string())
            .unwrap_or_else(|| err.to_string());
        return Err(format!("API returned an error: {}", truncate(&msg, 400)));
    }

    // Chain of thought precedes the body: a chunk carrying both is rare, but if it
    // happens, deliver the "thinking" part first
    if let Some(text) = extract_reasoning(&value) {
        return Ok(Some(SseDelta::Reasoning(text)));
    }

    Ok(extract_text(&value).map(SseDelta::Text))
}

/// Extracts the chain of thought from a piece of response JSON.
///
/// The field name varies by vendor: DeepSeek's reasoning models use
/// `reasoning_content`, some other implementations use `reasoning`. Streaming puts
/// it in `delta`, non-streaming in `message`; both are recognized.
fn extract_reasoning(value: &Value) -> Option<String> {
    let choice = value.get("choices")?.as_array()?.first()?;
    for holder in [choice.get("delta"), choice.get("message")] {
        let Some(holder) = holder else { continue };
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = holder.get(key).and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// Extracts the body text from a piece of response JSON. Handles both
/// `delta.content` (streaming) and `message.content` (non-streaming).
fn extract_text(value: &Value) -> Option<String> {
    let choice = value.get("choices")?.as_array()?.first()?;

    if let Some(text) = choice
        .get("delta")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str())
    {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    if let Some(text) = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    None
}

/// Streaming request; hands each incremental chunk to the callback.
///
/// Returns the full answer for the caller to write into history. On cancellation,
/// the **part already received** is returned as usual -- the user pressing cancel
/// does not mean this portion of the answer is worthless; keeping it beats discarding it.
///
/// This deliberately does not depend on `AppHandle`: by separating "fetching data"
/// from "pushing to the frontend", this logic can be verified on its own outside the
/// Tauri runtime (see examples/api_probe.rs).
pub async fn stream_chat_with<F>(
    profile: &Profile,
    timeout_secs: u64,
    messages: Vec<Value>,
    cancel: &Cancel,
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(Chunk) + Send,
{
    let client = build_client(timeout_secs, &profile.proxy)?;
    let body = json!({
        "model": profile.model.trim(),
        "stream": true,
        "messages": messages,
    });
    let resp = build_request(&client, profile, &body)?
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("API returned HTTP {status}: {}", truncate(&text, 600)));
    }

    let mut stream = resp.bytes_stream();
    // Accumulate by byte and split lines by '\n', so multi-byte characters are not
    // severed at chunk boundaries
    let mut pending: Vec<u8> = Vec::new();
    let mut raw = String::new();
    let mut full = String::new();
    let mut finished = false;

    while let Some(chunk) = stream.next().await {
        // Cancellation must be checked here: a genuinely stuck stream never yields
        // the next chunk, and a timeout alone cannot save you
        if cancel.is_cancelled() {
            break;
        }

        let chunk = chunk.map_err(|e| format!("Reading the response stream was interrupted: {e}"))?;
        pending.extend_from_slice(&chunk);
        raw.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);

            match parse_sse_line(&line)? {
                Some(SseDelta::Text(text)) => {
                    full.push_str(&text);
                    on_chunk(Chunk::Text(text));
                }
                Some(SseDelta::Reasoning(text)) => on_chunk(Chunk::Reasoning(text)),
                Some(SseDelta::Done) => {
                    finished = true;
                    break;
                }
                None => {}
            }
        }
        if finished {
            break;
        }
    }

    // Why we stopped: finished naturally, or the user pressed cancel
    if cancel.is_cancelled() {
        if full.trim().is_empty() {
            return Err("Cancelled; no content was received this time.".into());
        }
        return Ok(full);
    }

    // Fallback: some third-party implementations claim stream but return one complete
    // JSON directly (no data: lines). In that case nothing was parsed above, so try
    // parsing the whole response once here.
    if full.is_empty() {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if let Some(err) = value.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| err.to_string());
                return Err(format!("API returned an error: {}", truncate(&msg, 400)));
            }
            if let Some(text) = extract_text(&value) {
                on_chunk(Chunk::Text(text.clone()));
                full = text;
            }
        }
    }

    if full.trim().is_empty() {
        return Err(format!(
            "The API returned no body content. Raw response fragment: {}",
            truncate(&raw, 400)
        ));
    }

    Ok(full)
}

/// Streaming request in the Tauri environment: pushes increments and chain of
/// thought to the frontend as separate events.
pub async fn stream_chat(
    app: &AppHandle,
    profile: &Profile,
    timeout_secs: u64,
    messages: Vec<Value>,
    cancel: &Cancel,
) -> Result<String, String> {
    let app = app.clone();
    stream_chat_with(profile, timeout_secs, messages, cancel, move |chunk| {
        let (event, text) = match chunk {
            Chunk::Text(text) => (EVT_DELTA, text),
            Chunk::Reasoning(text) => (EVT_REASONING, text),
        };
        let _ = app.emit(event, text);
    })
    .await
}

/// Used by the settings page's "Test connection": sends one minimal non-streaming
/// request and brings back the model's reply.
pub async fn test_connection(profile: &Profile, timeout_secs: u64) -> Result<String, String> {
    let client = build_client(timeout_secs, &profile.proxy)?;
    let body = json!({
        "model": profile.model.trim(),
        "stream": false,
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": "连接测试，请只回复两个字：正常"
        }],
    });
    let resp = build_request(&client, profile, &body)?
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("API returned HTTP {status}: {}", truncate(&text, 600)));
    }

    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("Response is not valid JSON: {e}"))?;
    if let Some(err) = value.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.to_string())
            .unwrap_or_else(|| err.to_string());
        return Err(format!("API returned an error: {}", truncate(&msg, 400)));
    }
    extract_text(&value).ok_or_else(|| {
        format!(
            "No model output found in the response. Raw response: {}",
            truncate(&text, 400)
        )
    })
}

/// Models-list URL: same base as the chat endpoint with /models at the end
/// (endpoint_for gives .../chat/completions, so swap that last segment).
fn models_url(profile: &Profile) -> String {
    let chat = profile.endpoint();
    match chat.rfind("/chat/completions") {
        Some(i) => format!("{}/models", &chat[..i]),
        None => chat,
    }
}

/// Used by the settings page to fill the model-name dropdown: GET /models on the
/// provider and return the model IDs. OpenAI-compatible providers answer
/// {data:[{id:...}]}; Anthropic answers the same shape (its /models wants
/// x-api-key + anthropic-version; those headers are ignored by everyone else).
pub async fn list_models(profile: &Profile, timeout_secs: u64) -> Result<Vec<String>, String> {
    if profile.api_key.trim().is_empty() {
        return Err("This profile has no API Key set; please fill it in the settings first.".into());
    }
    let url = models_url(profile);
    if url.is_empty() || !url.contains("/models") {
        return Err("This profile has no Base URL set; please fill it in the settings first.".into());
    }
    let client = build_client(timeout_secs, &profile.proxy)?;
    let mut req = client
        .get(&url)
        .bearer_auth(profile.api_key.trim())
        .header("Accept", "application/json")
        .header("x-api-key", profile.api_key.trim())
        .header("anthropic-version", "2023-06-01");
    for (key, value) in profile.extra_headers_map() {
        req = req.header(key, value);
    }
    let resp = req.send().await.map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("API returned HTTP {status}: {}", truncate(&text, 600)));
    }

    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("Response is not valid JSON: {e}"))?;
    let arr = value
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| {
            format!(
                "No model list found in the response. Raw response: {}",
                truncate(&text, 400)
            )
        })?;
    let mut ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_openai_delta() {
        let line = r#"data: {"choices":[{"delta":{"content":"你好"}}]}"#;
        match parse_sse_line(line).unwrap() {
            Some(SseDelta::Text(t)) => assert_eq!(t, "你好"),
            _ => panic!("should parse out text"),
        }
    }

    #[test]
    fn parses_non_streaming_message_form() {
        // Non-streaming form: message.content instead of delta.content
        let line = r#"data: {"choices":[{"message":{"content":"hi"}}]}"#;
        match parse_sse_line(line).unwrap() {
            Some(SseDelta::Text(t)) => assert_eq!(t, "hi"),
            _ => panic!("should parse out text"),
        }
    }

    #[test]
    fn ignores_keepalive_and_unrelated_lines() {
        assert!(parse_sse_line("").unwrap().is_none());
        assert!(parse_sse_line(": ping").unwrap().is_none());
        assert!(parse_sse_line("event: message").unwrap().is_none());
        assert!(parse_sse_line("garbage data").unwrap().is_none());
    }

    #[test]
    fn detects_done_marker() {
        assert!(matches!(
            parse_sse_line("data: [DONE]").unwrap(),
            Some(SseDelta::Done)
        ));
    }

    #[test]
    fn surfaces_inline_error() {
        let line = r#"data: {"error":{"message":"invalid api key"}}"#;
        let err = parse_sse_line(line).unwrap_err();
        assert!(err.contains("invalid api key"), "actual: {err}");
    }

    #[test]
    fn role_only_delta_yields_nothing() {
        // The first chunk usually has only a role, no content
        let line = r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert!(parse_sse_line(line).unwrap().is_none());
    }

    #[test]
    fn parses_reasoning_content() {
        // Reasoning models emit a "thinking" segment first; the field name varies by
        // vendor, so both are recognized
        let deepseek = r#"data: {"choices":[{"delta":{"reasoning_content":"check the error first"}}]}"#;
        match parse_sse_line(deepseek).unwrap() {
            Some(SseDelta::Reasoning(t)) => assert_eq!(t, "check the error first"),
            other => panic!("should be treated as reasoning, actual: {other:?}"),
        }

        let alt = r#"data: {"choices":[{"delta":{"reasoning":"another name for it"}}]}"#;
        match parse_sse_line(alt).unwrap() {
            Some(SseDelta::Reasoning(t)) => assert_eq!(t, "another name for it"),
            other => panic!("should be treated as reasoning, actual: {other:?}"),
        }
    }

    #[test]
    fn reasoning_beats_text_when_both_present() {
        let line = r#"data: {"choices":[{"delta":{"reasoning_content":"think","content":"say"}}]}"#;
        match parse_sse_line(line).unwrap() {
            Some(SseDelta::Reasoning(t)) => assert_eq!(t, "think"),
            other => panic!("when a chunk carries both, reasoning goes first, actual: {other:?}"),
        }
    }

    #[test]
    fn cancel_flag_is_shared() {
        let cancel = Cancel::new();
        let watcher = cancel.clone();
        assert!(!watcher.is_cancelled());
        cancel.cancel();
        assert!(watcher.is_cancelled(), "the cloned copy should also see the cancellation");
    }

    #[test]
    fn user_message_shape() {
        let no_img = user_message("look at this", &[]);
        assert_eq!(no_img["content"], "look at this");

        let one = user_message("look at this", &["AAA".into()]);
        assert_eq!(one["content"][0]["type"], "text");
        assert_eq!(one["content"][1]["type"], "image_url");
        assert_eq!(
            one["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,AAA"
        );
    }

    #[test]
    fn user_message_carries_every_selected_image() {
        // However many images are selected in the filmstrip, the question carries
        // that many -- order must also be preserved (the user captured them in order)
        let many = user_message("compare these three", &["A".into(), "B".into(), "C".into()]);
        let parts = many["content"].as_array().unwrap();
        assert_eq!(parts.len(), 4, "one text segment + three images");
        assert_eq!(parts[0]["type"], "text");
        for (index, expected) in ["A", "B", "C"].iter().enumerate() {
            assert_eq!(parts[index + 1]["type"], "image_url");
            assert_eq!(
                parts[index + 1]["image_url"]["url"],
                format!("data:image/jpeg;base64,{expected}")
            );
        }
    }

    #[test]
    fn rejects_missing_credentials_before_sending() {
        let profile = Profile {
            api_key: String::new(),
            model: "m".into(),
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let err = build_request(&client, &profile, &json!({})).unwrap_err();
        assert!(err.contains("API Key"), "actual: {err}");
    }

    #[test]
    fn each_turn_carries_its_own_image() {
        // A "continue with a fresh selection" flow: the first question carries an
        // image, the follow-up does not, the second selection carries a new image,
        // and its follow-up does not
        let turns = vec![
            Turn {
                question: "first question".into(),
                answer: "first answer".into(),
                images: vec!["IMG1".into()],
                ..Default::default()
            },
            Turn {
                question: "follow-up".into(),
                answer: "follow-up answer".into(),
                ..Default::default()
            },
            Turn {
                question: "second image".into(),
                answer: "second answer".into(),
                images: vec!["IMG2".into()],
                ..Default::default()
            },
            Turn {
                question: "another follow-up".into(),
                answer: String::new(),
                ..Default::default()
            },
        ];
        let messages = messages_from_turns("system prompt", &turns);

        // system + (user, assistant) × 3 + the last user that is still unanswered
        assert_eq!(messages.len(), 8);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages[1]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,IMG1"
        );
        assert_eq!(messages[3]["content"], "follow-up", "the follow-up must not carry an image again");
        assert_eq!(
            messages[5]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,IMG2",
            "the second selection's turn must carry the new image"
        );
        assert_eq!(messages[7]["content"], "another follow-up");
    }

    #[test]
    fn pending_turn_has_no_assistant_message() {
        let turns = vec![Turn {
            question: "just asked".into(),
            answer: String::new(),
            images: vec!["IMG".into()],
            ..Default::default()
        }];
        let messages = messages_from_turns("", &turns);
        assert_eq!(messages.len(), 1, "an empty assistant message must not appear while unanswered");
        assert_eq!(messages[0]["content"][0]["text"], "just asked");
    }

    #[test]
    fn empty_system_prompt_and_image_are_omitted() {
        let turns = vec![Turn {
            question: "q".into(),
            answer: "a".into(),
            ..Default::default()
        }];
        let messages = messages_from_turns("   ", &turns);
        assert_eq!(messages[0]["role"], "user", "an empty system prompt must not occupy a message");
        // With neither a system prompt nor images, content is just a plain string
        assert_eq!(messages[0]["content"], "q");
    }
}
