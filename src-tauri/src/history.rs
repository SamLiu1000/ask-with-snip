//! Persistence for conversation history.
//!
//! Storage layout (everything lives under `history/` in the app config directory):
//!
//! ```text
//! history/
//!   index.json          summaries of all conversations; the list page reads only this file
//!   <id>.json           the full content of one conversation (including that screenshot's base64)
//! ```
//!
//! Splitting into "index + one file per conversation" keeps the list page fast: summaries are
//! plain text, so one read is enough to render the whole list without touching dozens or
//! hundreds of large image-bearing files. Deleting an entry removes from both files.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

/// Maximum number of conversations to keep. Beyond this, the oldest are dropped
/// so the history directory doesn't grow without bound.
pub const MAX_ENTRIES: usize = 200;

const TITLE_MAX_CHARS: usize = 40;

/// One question/answer turn.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Turn {
    pub question: String,
    pub answer: String,
    /// The images attached to this turn's question (base64, without the data: prefix);
    /// there can be several — however many are selected in the film strip on the panel
    /// get sent with one question.
    ///
    /// Images hang off the turn rather than the conversation because after "continue
    /// the context with another snip" a single conversation can contain several images.
    /// When history is replayed, each image must follow its own question, otherwise the
    /// model sees "old questions with new images". Empty for follow-up turns (no new snip).
    #[serde(default)]
    pub images: Vec<String>,
    /// Legacy-format compatibility slot (older turns held a single image). Migrated into
    /// `images` on load; always empty in new records.
    #[serde(default)]
    pub image_b64: String,
}

impl Turn {
    /// The images this turn actually carries (including the legacy one).
    pub fn all_images(&self) -> Vec<String> {
        let mut images = self.images.clone();
        if images.is_empty() && !self.image_b64.is_empty() {
            images.push(self.image_b64.clone());
        }
        images
    }
}

/// A screenshot in the staging area: snipped but not yet sent with any question.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct StagedImage {
    pub image_b64: String,
    /// Selected? Only selected (highlighted) images are sent with the next question.
    /// Freshly snipped images are selected by default.
    #[serde(default = "default_true")]
    pub selected: bool,
    /// Size as cropped (shown in the UI)
    pub crop_w: u32,
    pub crop_h: u32,
    /// Size actually sent to the model (long edge may have been scaled down)
    pub sent_w: u32,
    pub sent_h: u32,
}

fn default_true() -> bool {
    true
}

/// A conversation: several question/answer turns (possibly spanning multiple snips).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Conversation {
    pub id: String,
    /// unix milliseconds
    pub created_at: i64,
    /// Images just snipped but not yet asked about; there can be several (the panel's
    /// "+" keeps adding more). The selected ones move onto the turn when the question
    /// is sent (see ask_inner in lib.rs).
    #[serde(default)]
    pub staged: Vec<StagedImage>,
    /// Legacy-format compatibility slot (the staging area used to hold a single image).
    /// Migrated into `staged` on load.
    #[serde(default)]
    pub image_b64: String,
    /// Size as cropped, shown in the UI (from the most recent snip)
    pub crop_w: u32,
    pub crop_h: u32,
    /// Size actually sent to the model (long edge may have been scaled down)
    pub sent_w: u32,
    pub sent_h: u32,
    pub turns: Vec<Turn>,
    /// The site session URL for the web-chat backend (`https://chat.deepseek.com/a/chat/s/<id>`).
    /// Recorded right after a turn finishes: afterwards, opening this conversation to keep
    /// asking, regenerating, or "continuing the context" all deep-link straight back to that
    /// session and keep chatting there — the site has its own context, so earlier turns
    /// don't need to be replayed one by one.
    /// Empty string for API-based conversations, or when the site never produced a session
    /// URL (e.g. it didn't answer).
    #[serde(default)]
    pub webchat_url: String,
}

impl Conversation {
    /// Title shown in the list: the first question, truncated if too long.
    pub fn title(&self) -> String {
        let first = self
            .turns
            .iter()
            .map(|t| t.question.trim())
            .find(|q| !q.is_empty())
            .unwrap_or("");
        truncate_title(first)
    }

    /// Which image the UI should display: prefer the last staged one, otherwise the
    /// last one used by a turn.
    pub fn display_image(&self) -> &str {
        if let Some(last) = self.staged.last() {
            return &last.image_b64;
        }
        if !self.image_b64.is_empty() {
            return &self.image_b64;
        }
        for turn in self.turns.iter().rev() {
            if let Some(image) = turn.images.iter().find(|image| !image.is_empty()) {
                return image;
            }
            if !turn.image_b64.is_empty() {
                return &turn.image_b64;
            }
        }
        ""
    }

    /// Record a new question turn, carrying along the images to send.
    ///
    /// Note this **takes** the images, it doesn't copy them: they now belong to this
    /// turn and must not remain in the staging area, otherwise the next question would
    /// send them again.
    pub fn start_turn(&mut self, question: &str, images: Vec<String>) {
        self.turns.push(Turn {
            question: question.to_string(),
            answer: String::new(),
            images,
            image_b64: String::new(),
        });
    }

    /// Take the selected images out of the staging area (used when asking).
    /// Unselected ones stay staged.
    ///
    /// Collected back-to-front to avoid shifting on every removal; the taken images
    /// keep their original order.
    pub fn take_selected_staged(&mut self) -> Vec<String> {
        let mut taken = Vec::new();
        let mut index = self.staged.len();
        for item in self.staged.iter().rev() {
            index -= 1;
            if item.selected {
                taken.push((index, item.image_b64.clone()));
            }
        }
        self.staged.retain(|item| !item.selected);
        taken.sort_by_key(|(index, _)| *index);
        taken.into_iter().map(|(_, image)| image).collect()
    }

    /// Append a freshly snipped image to the staging area (the panel's "+" path),
    /// selected by default.
    pub fn stage(&mut self, image: StagedImage) {
        self.staged.push(image);
    }

    /// Toggle the selection state of staged image `index`, returning the new value.
    pub fn toggle_staged(&mut self, index: usize) -> Result<bool, String> {
        let item = self
            .staged
            .get_mut(index)
            .ok_or_else(|| format!("No screenshot #{}", index + 1))?;
        item.selected = !item.selected;
        Ok(item.selected)
    }

    /// Remove staged image `index` (the ✕ on that film-strip card): don't send this
    /// one to the AI after all.
    ///
    /// The conversation itself is kept — it may still have Q&A turns; the panel falls
    /// back to showing the most recent turn's images.
    pub fn remove_staged(&mut self, index: usize) -> Result<(), String> {
        if index >= self.staged.len() {
            return Err(format!("No screenshot #{}", index + 1));
        }
        self.staged.remove(index);
        Ok(())
    }

    /// Remove the already-sent image `image` from turn `turn` (the ✕ on a dimmed
    /// film-strip card).
    ///
    /// This **edits the record**, not just the UI: that turn has one fewer image, so
    /// later "continue the context" follow-ups and regenerations won't include it.
    /// The caller is therefore expected to persist the change.
    pub fn remove_turn_image(&mut self, turn: usize, image: usize) -> Result<(), String> {
        let Some(entry) = self.turns.get_mut(turn) else {
            return Err(format!("No turn #{}", turn + 1));
        };
        if image >= entry.images.len() {
            return Err(format!("Turn #{} has no image #{}", turn + 1, image + 1));
        }
        entry.images.remove(image);
        Ok(())
    }

    pub fn summary(&self) -> Summary {
        Summary {
            id: self.id.clone(),
            created_at: self.created_at,
            title: self.title(),
            turn_count: self.turns.len(),
            crop_w: self.crop_w,
            crop_h: self.crop_h,
        }
    }

    /// Clear the last turn's answer, keeping its question and images (used by
    /// "regenerate").
    ///
    /// The image must not be lost: that turn was asked with a screenshot, so the same
    /// image has to be sent again when re-answering. Returns the cleared turn's
    /// question, or None when there are no turns.
    pub fn clear_last_answer(&mut self) -> Option<String> {
        let last = self.turns.last_mut()?;
        last.answer.clear();
        Some(last.question.clone())
    }

    /// Replace the last turn's question (answer cleared, image kept); used by
    /// "edit question".
    ///
    /// Returns the turn's question after the rewrite (also confirming success for
    /// the caller).
    pub fn rewrite_last_question(&mut self, question: &str) -> Option<String> {
        let last = self.turns.last_mut()?;
        last.question = question.trim().to_string();
        last.answer.clear();
        Some(last.question.clone())
    }

    /// Replace the last turn's question; when `images` is Some, also replace that
    /// turn's screenshots — "edit question" lets the user drop images before re-asking
    /// (empty Vec = re-ask as plain text). Returns the new question.
    pub fn rewrite_last_turn(
        &mut self,
        question: &str,
        images: Option<Vec<String>>,
    ) -> Option<String> {
        let last = self.turns.last_mut()?;
        last.question = question.trim().to_string();
        last.answer.clear();
        if let Some(images) = images {
            last.images = images;
        }
        Some(last.question.clone())
    }
}

/// Summary used by the list. Deliberately contains no images, so it reads fast.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Summary {
    pub id: String,
    pub created_at: i64,
    pub title: String,
    pub turn_count: usize,
    pub crop_w: u32,
    pub crop_h: u32,
}

fn truncate_title(text: &str) -> String {
    let cleaned = text.replace(['\n', '\r'], " ");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "(no question)".to_string();
    }
    let mut out: String = cleaned.chars().take(TITLE_MAX_CHARS).collect();
    if cleaned.chars().count() > TITLE_MAX_CHARS {
        out.push('…');
    }
    out
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Generate an id that is time-ordered and never collides within one millisecond.
static SEQ: AtomicU32 = AtomicU32::new(0);

pub fn new_id() -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", now_millis(), seq)
}

fn history_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = crate::data_root(app).join("history");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create the history directory: {e}"))?;
    Ok(dir)
}

fn index_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(history_dir(app)?.join("index.json"))
}

/// Ids are generated by us (timestamp-sequence), but guard against path traversal
/// anyway: only digits and hyphens are allowed.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || c == '-')
}

fn entry_path(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    if !is_safe_id(id) {
        return Err(format!("Invalid record id: {id}"));
    }
    Ok(history_dir(app)?.join(format!("{id}.json")))
}

/// Read the index, newest first (sorted by time, descending).
pub fn list(app: &AppHandle) -> Vec<Summary> {
    let mut items: Vec<Summary> = index_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<Summary>>(&s).ok())
        .unwrap_or_default();
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    items
}

fn write_index(app: &AppHandle, items: &[Summary]) -> Result<(), String> {
    let path = index_path(app)?;
    let text = serde_json::to_string_pretty(items).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("Failed to write the history index: {e}"))
}

/// Save a conversation: write the content file and update the index; drop the oldest
/// entries when over the cap.
pub fn save(app: &AppHandle, conversation: &Conversation) -> Result<(), String> {
    let path = entry_path(app, &conversation.id)?;
    let text = serde_json::to_string_pretty(conversation).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("Failed to write the conversation record: {e}"))?;

    let mut items = list(app);
    let summary = conversation.summary();
    match items.iter_mut().find(|s| s.id == summary.id) {
        Some(slot) => *slot = summary,
        None => items.push(summary),
    }
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Drop entries beyond the cap (deleting their files too)
    while items.len() > MAX_ENTRIES {
        let dropped = items.pop();
        if let Some(dropped) = dropped {
            if let Ok(path) = entry_path(app, &dropped.id) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    write_index(app, &items)
}

pub fn load(app: &AppHandle, id: &str) -> Result<Conversation, String> {
    let path = entry_path(app, id)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read the conversation record: {e}"))?;
    let conv: Conversation =
        serde_json::from_str(&text).map_err(|e| format!("Failed to parse the conversation record: {e}"))?;
    Ok(migrate_legacy_image(conv))
}

/// Migrate legacy-format records to the new format.
///
/// Two migrations:
///   1. Earliest: the image lived on the **conversation** (single image). Images now hang
///      off turns ("continue the context with another snip" puts several images in one
///      conversation). When the old file loads, that image lands in the "snipped but not
///      yet asked" slot; left there it would be attached to the **next** question and the
///      original question would lose its image.
///   2. Later: turns and the staging area went from **single** to **multiple** images
///      (`image_b64` → `images`/`staged`).
fn migrate_legacy_image(mut conv: Conversation) -> Conversation {
    // Turns: single → multiple
    for turn in conv.turns.iter_mut() {
        if turn.images.is_empty() && !turn.image_b64.is_empty() {
            turn.images = vec![std::mem::take(&mut turn.image_b64)];
        }
    }
    // Pending image on the conversation → into the first turn (oldest case), otherwise
    // into the staging area
    if !conv.image_b64.is_empty() {
        let image = std::mem::take(&mut conv.image_b64);
        let staged = StagedImage {
            image_b64: image,
            selected: true,
            crop_w: conv.crop_w,
            crop_h: conv.crop_h,
            sent_w: conv.sent_w,
            sent_h: conv.sent_h,
        };
        if conv.turns.is_empty() {
            conv.staged.push(staged);
        } else if let Some(first) = conv.turns.first_mut() {
            if first.images.is_empty() {
                first.images = vec![staged.image_b64];
            } else {
                conv.staged.push(staged);
            }
        }
    }
    conv
}

pub fn delete(app: &AppHandle, id: &str) -> Result<(), String> {
    if let Ok(path) = entry_path(app, id) {
        let _ = std::fs::remove_file(path);
    }
    let items: Vec<Summary> = list(app).into_iter().filter(|s| s.id != id).collect();
    write_index(app, &items)
}

pub fn clear(app: &AppHandle) -> Result<(), String> {
    for item in list(app) {
        if let Ok(path) = entry_path(app, &item.id) {
            let _ = std::fs::remove_file(path);
        }
    }
    write_index(app, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(questions: &[&str]) -> Conversation {
        Conversation {
            id: "1-0".into(),
            created_at: 1,
            turns: questions
                .iter()
                .map(|q| Turn {
                    question: (*q).into(),
                    answer: String::new(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn staged_images_can_be_removed_one_by_one() {
        // The ✕ on each film-strip card: remove this one, keep the rest as-is and in order
        let mut conv = conversation(&["a question was asked"]);
        for image in ["IMG1", "IMG2", "IMG3"] {
            conv.stage(StagedImage {
                image_b64: image.into(),
                selected: true,
                crop_w: 1,
                crop_h: 1,
                sent_w: 1,
                sent_h: 1,
            });
        }
        conv.remove_staged(1).expect("remove the middle one");
        let left: Vec<&str> = conv.staged.iter().map(|s| s.image_b64.as_str()).collect();
        assert_eq!(left, vec!["IMG1", "IMG3"]);
        assert!(conv.remove_staged(5).is_err(), "out-of-range must error, not silently do nothing");
        assert_eq!(conv.staged.len(), 2, "the failing call must not touch the data");
    }

    #[test]
    fn a_sent_image_can_be_removed_from_its_turn() {
        // The ✕ on a dimmed card edits the record: that turn has one fewer image, so
        // follow-ups / regeneration no longer carry it
        let mut conv = conversation(&["first question", "second question"]);
        conv.turns[0].images = vec!["A".into(), "B".into()];
        conv.turns[1].images = vec!["C".into()];

        conv.remove_turn_image(0, 1).expect("remove the second image of the first turn");
        assert_eq!(conv.turns[0].all_images(), vec!["A"]);
        assert_eq!(conv.turns[1].all_images(), vec!["C"], "other turns are unaffected");

        assert!(conv.remove_turn_image(0, 9).is_err(), "image index out of range must error");
        assert!(conv.remove_turn_image(9, 0).is_err(), "turn index out of range must also error");
        assert_eq!(conv.turns[1].all_images(), vec!["C"], "the failing call must not touch the data");
    }

    #[test]
    fn legacy_record_gets_its_image_on_the_first_turn() {
        // Legacy format: the image is on the conversation, turns have none (empty after
        // deserialization)
        let legacy = r#"{
            "id": "5-0", "created_at": 5, "image_b64": "OLDIMG",
            "crop_w": 10, "crop_h": 20, "sent_w": 10, "sent_h": 20,
            "turns": [{"question": "first question", "answer": "first answer"},
                      {"question": "follow-up", "answer": "follow-up answer"}]
        }"#;
        let conv = migrate_legacy_image(serde_json::from_str::<Conversation>(legacy).unwrap());

        assert_eq!(conv.turns[0].all_images(), vec!["OLDIMG"], "the legacy image must land on the first turn");
        assert!(conv.turns[1].all_images().is_empty(), "the follow-up turn must not gain images out of thin air");
        assert_eq!(conv.image_b64, "", "after migration it must not remain in the pending slot");
        assert_eq!(conv.display_image(), "OLDIMG");
    }

    #[test]
    fn legacy_pending_image_goes_to_the_staging_area() {
        // Legacy format + nothing asked yet: that image should become a staged one (selected)
        let legacy = r#"{
            "id": "6-0", "created_at": 6, "image_b64": "PENDING",
            "crop_w": 30, "crop_h": 40, "sent_w": 30, "sent_h": 40,
            "turns": []
        }"#;
        let conv = migrate_legacy_image(serde_json::from_str::<Conversation>(legacy).unwrap());
        assert_eq!(conv.staged.len(), 1);
        assert_eq!(conv.staged[0].image_b64, "PENDING");
        assert!(conv.staged[0].selected, "migrated images default to selected, same behavior as before");
        assert_eq!(conv.staged[0].crop_w, 30);
        assert_eq!(conv.display_image(), "PENDING");
    }

    #[test]
    fn migration_leaves_new_records_alone() {
        // New format: each turn carries its own images; the staging area holds fresh,
        // not-yet-asked snips
        let mut conv = conversation(&["first question"]);
        conv.turns[0].images = vec!["IMG1".into()];
        conv.stage(staged("PENDING", true));

        let conv = migrate_legacy_image(conv);
        assert_eq!(conv.turns[0].all_images(), vec!["IMG1"]);
        assert_eq!(conv.staged.len(), 1, "the new-format staging area must not be touched");
        assert_eq!(conv.display_image(), "PENDING", "the freshly snipped image takes display priority");
    }

    #[test]
    fn display_image_falls_back_to_the_last_turn() {
        let mut conv = conversation(&["first question", "second question"]);
        conv.turns[0].images = vec!["IMG1".into()];
        conv.turns[1].images = vec!["IMG2".into()];
        assert_eq!(conv.display_image(), "IMG2", "with nothing pending, use the last turn's image");

        let empty = Conversation::default();
        assert_eq!(empty.display_image(), "");
    }

    #[test]
    fn clear_last_answer_keeps_the_question_and_its_image() {
        // Regenerate: the question (and its images) must survive — re-answering has to
        // send the same batch of images again
        let mut conv = conversation(&["first question", "second question"]);
        conv.turns[0].answer = "first answer".into();
        conv.turns[0].images = vec!["IMG1".into()];
        conv.turns[1].answer = "second answer".into();
        conv.turns[1].images = vec!["IMG2".into(), "IMG3".into()];

        assert_eq!(conv.clear_last_answer().as_deref(), Some("second question"));
        assert_eq!(conv.turns.len(), 2, "must not delete the turn itself");
        assert_eq!(conv.turns[1].question, "second question");
        assert_eq!(conv.turns[1].images, vec!["IMG2", "IMG3"], "all images must be kept");
        assert_eq!(conv.turns[1].answer, "", "the answer is cleared");
        assert_eq!(conv.turns[0].answer, "first answer", "earlier turns are untouched");

        let mut empty = Conversation::default();
        assert_eq!(empty.clear_last_answer(), None);
    }

    #[test]
    fn rewrite_last_question_only_touches_the_last_turn() {
        let mut conv = conversation(&["first question", "second question"]);
        conv.turns[1].answer = "second answer".into();
        conv.turns[1].images = vec!["IMG2".into()];

        assert_eq!(
            conv.rewrite_last_question("  edited question  ").as_deref(),
            Some("edited question"),
            "leading and trailing whitespace must be trimmed"
        );
        assert_eq!(conv.turns[1].question, "edited question");
        assert_eq!(conv.turns[1].answer, "", "once the question changes, the old answer no longer counts");
        assert_eq!(conv.turns[1].images, vec!["IMG2"], "images must be kept: it's still the same screenshot");
        assert_eq!(conv.turns[0].question, "first question");
    }

    #[test]
    fn turns_roundtrip_with_their_images() {
        let mut conv = conversation(&["first question", "second question"]);
        conv.turns[0].images = vec!["IMG1".into(), "IMG1B".into()];
        conv.turns[1].images = vec!["IMG2".into()];
        let json = serde_json::to_string(&conv).unwrap();
        let back: Conversation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.turns[0].all_images(), vec!["IMG1", "IMG1B"], "multiple images persist and reload unchanged");
        assert_eq!(back.turns[1].all_images(), vec!["IMG2"]);
    }

    #[test]
    fn start_turn_carries_the_selected_staged_images() {
        let mut conv = Conversation::default();
        conv.stage(staged("IMG1", true));
        conv.stage(staged("IMG2", false));
        let taken = conv.take_selected_staged();
        assert_eq!(taken, vec!["IMG1"], "only the selected images go with the question");
        assert_eq!(conv.staged.len(), 1, "unselected ones stay in the staging area");
        assert_eq!(conv.staged[0].image_b64, "IMG2");

        conv.start_turn("first question", taken);
        assert_eq!(conv.turns[0].images, vec!["IMG1"], "freshly snipped images belong to this turn");
        assert_eq!(conv.turns[0].all_images(), vec!["IMG1"]);

        // Follow-up: no new snip, so this turn carries no images
        conv.start_turn("follow-up", vec![]);
        assert!(conv.turns[1].images.is_empty());

        // Two more snips, both selected
        conv.stage(staged("IMG3", true));
        conv.stage(staged("IMG4", true));
        let taken = conv.take_selected_staged();
        assert_eq!(taken, vec!["IMG3", "IMG4"], "multiple selected images are carried in snip order");
        conv.start_turn("second question", taken);
        assert_eq!(conv.turns[2].images, vec!["IMG3", "IMG4"]);
        assert_eq!(conv.display_image(), "IMG2", "an unselected image stays staged, so it is the one displayed");
    }

    fn staged(image: &str, selected: bool) -> StagedImage {
        StagedImage {
            image_b64: image.into(),
            selected,
            crop_w: 10,
            crop_h: 20,
            sent_w: 10,
            sent_h: 20,
        }
    }

    #[test]
    fn toggle_staged_flips_selection() {
        let mut conv = Conversation::default();
        conv.stage(staged("IMG1", true));
        assert_eq!(conv.toggle_staged(0), Ok(false));
        assert_eq!(conv.toggle_staged(0), Ok(true));
        assert!(conv.toggle_staged(3).is_err(), "out of range must error, not panic");
    }

    #[test]
    fn start_turn_works_without_any_image() {
        let mut conv = Conversation::default();
        conv.start_turn("text-only question", vec![]);
        assert_eq!(conv.turns.len(), 1);
        assert!(conv.turns[0].images.is_empty());
        assert_eq!(conv.display_image(), "");
    }

    #[test]
    fn title_uses_first_question() {
        let conv = conversation(&["first question", "second question"]);
        assert_eq!(conv.title(), "first question");
    }

    #[test]
    fn title_skips_blank_questions() {
        let conv = conversation(&["   ", "the real question"]);
        assert_eq!(conv.title(), "the real question");
    }

    #[test]
    fn title_handles_no_question_yet() {
        assert_eq!(conversation(&[]).title(), "(no question)");
    }

    #[test]
    fn title_truncates_long_text_and_strips_newlines() {
        let long = "a".repeat(100);
        let conv = conversation(&[long.as_str()]);
        let title = conv.title();
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1, "40 chars plus one ellipsis");
        assert!(title.ends_with('…'));

        let multiline = conversation(&["first line\nsecond line"]).title();
        assert_eq!(multiline, "first line second line");
    }

    #[test]
    fn summary_carries_counts_but_not_image() {
        let mut conv = conversation(&["a question"]);
        conv.image_b64 = "AAAA".repeat(1000);
        conv.crop_w = 800;
        conv.crop_h = 600;
        let summary = conv.summary();

        assert_eq!(summary.turn_count, 1);
        assert_eq!((summary.crop_w, summary.crop_h), (800, 600));
        // The summary struct must not contain an image field at all (that's why the list is fast)
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("image_b64"), "the summary must not carry an image: {json}");
        assert!(json.len() < 300, "the summary should be tiny, got {} bytes", json.len());
    }

    #[test]
    fn ids_are_time_ordered_and_unique() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b, "must not collide even within the same millisecond");
        let (ta, _) = a.split_once('-').unwrap();
        let (tb, _) = b.split_once('-').unwrap();
        // The prefix is a millisecond timestamp, so lexicographic order equals time order
        assert!(ta.parse::<i64>().unwrap() <= tb.parse::<i64>().unwrap());
    }

    #[test]
    fn rejects_unsafe_ids() {
        assert!(is_safe_id("1712345678-0"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("../config"));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id("id with space"));
    }
}
