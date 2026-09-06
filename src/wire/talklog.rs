//! Durable talk log (issue #33): enough of every `prompt` -> `reply`*
//! exchange for `holler-server wait` (issue #35, not yet built) to answer "what
//! happened since I last checked" without having been connected live for
//! it.
//!
//! One JSON file per session, under `<HOLLER_STATE_DIR>/talk/<session>.json`
//! — the same state-directory convention `token::TokenStore` uses. This is
//! not a database: entries are loaded, mutated, and rewritten whole on every
//! append (mirroring `TokenStore`'s own persistence), which is fine at the
//! volume one interactive talk circuit produces. It only needs to survive
//! the CLI process exiting between `say` and a later `wait`.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::proto::ReplyBody;

fn now_ts() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// One recorded `reply` frame (partial or final) against a prompt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TalkReplyRecord {
    pub text: Option<String>,
    #[serde(default)]
    pub chunks: Vec<String>,
    pub done: bool,
    pub exit: Option<i64>,
    pub ts: String,
}

/// One `prompt` -> `reply`* exchange, keyed by its wire correlation id.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TalkEntry {
    pub id: String,
    pub session: String,
    pub prompt_text: String,
    pub prompt_ts: String,
    #[serde(default)]
    pub replies: Vec<TalkReplyRecord>,
}

/// Durable, file-backed talk history, one JSON array of [`TalkEntry`] per
/// session.
pub struct TalkLog {
    dir: PathBuf,
    /// Guards read-modify-write races between concurrent appends in this
    /// process (e.g. two `say`s in flight at once) — the filesystem
    /// itself gives no such guarantee.
    lock: Mutex<()>,
}

impl TalkLog {
    pub fn new(state_dir: PathBuf) -> Self {
        TalkLog {
            dir: state_dir.join("talk"),
            lock: Mutex::new(()),
        }
    }

    /// A session name, mapped to a safe filename: anything other than
    /// ASCII alphanumerics/`-`/`_` becomes `_`. A session name comes from
    /// a client's `presence` advertisement, so this is what stands between
    /// an adversarial or careless session name and a path-traversal write
    /// (`../../etc/...` collapses to a harmless `______etc___`, never
    /// leaving `self.dir`).
    fn safe_filename(name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
            + ".json"
    }

    fn path_for(&self, session: &str) -> PathBuf {
        self.dir.join(Self::safe_filename(session))
    }

    fn load(&self, session: &str) -> Vec<TalkEntry> {
        match fs::read_to_string(self.path_for(session)) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn save(&self, session: &str, entries: &[TalkEntry]) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_string_pretty(entries).expect("TalkEntry always serializes");
        fs::write(self.path_for(session), json)
    }

    /// Record a `prompt` this process just sent, before it awaits any
    /// reply — so the log has the request even if no reply ever arrives.
    pub fn record_prompt(&self, id: &str, session: &str, text: &str) {
        let _guard = self.lock.lock().expect("talk log mutex poisoned");
        let mut entries = self.load(session);
        entries.push(TalkEntry {
            id: id.to_string(),
            session: session.to_string(),
            prompt_text: text.to_string(),
            prompt_ts: now_ts(),
            replies: Vec::new(),
        });
        if let Err(e) = self.save(session, &entries) {
            eprintln!("holler-server: could not persist talk log for {session:?}: {e}");
        }
    }

    /// Append one `reply` frame (partial or final) to the entry `id`
    /// names. A reply whose `id` matches no recorded prompt (e.g. this
    /// process restarted mid-exchange) is dropped — there is no entry to
    /// attach it to.
    pub fn record_reply(&self, id: &str, session: &str, reply: &ReplyBody) {
        let _guard = self.lock.lock().expect("talk log mutex poisoned");
        let mut entries = self.load(session);
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        entry.replies.push(TalkReplyRecord {
            text: reply.text.clone(),
            chunks: reply.chunks.clone(),
            done: reply.done,
            exit: reply.exit,
            ts: now_ts(),
        });
        if let Err(e) = self.save(session, &entries) {
            eprintln!("holler-server: could not persist talk log for {session:?}: {e}");
        }
    }

    /// Read back everything recorded for `session`, oldest first — for
    /// `holler-server wait` (issue #35) and for tests to verify persistence
    /// directly rather than trusting a write happened.
    pub fn read(&self, session: &str) -> Vec<TalkEntry> {
        let _guard = self.lock.lock().expect("talk log mutex poisoned");
        self.load(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(text: &str, done: bool) -> ReplyBody {
        ReplyBody {
            session: "alpha".to_string(),
            text: Some(text.to_string()),
            chunks: Vec::new(),
            done,
            exit: if done { Some(0) } else { None },
        }
    }

    #[test]
    fn read_on_a_never_written_session_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let log = TalkLog::new(dir.path().to_path_buf());
        assert!(log.read("alpha").is_empty());
    }

    #[test]
    fn prompt_then_replies_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let log = TalkLog::new(dir.path().to_path_buf());
        log.record_prompt("id-1", "alpha", "hello");
        log.record_reply("id-1", "alpha", &reply("hi ", false));
        log.record_reply("id-1", "alpha", &reply("there", true));

        let entries = log.read("alpha");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "id-1");
        assert_eq!(entries[0].prompt_text, "hello");
        assert_eq!(entries[0].replies.len(), 2);
        assert_eq!(entries[0].replies[0].text.as_deref(), Some("hi "));
        assert!(!entries[0].replies[0].done);
        assert_eq!(entries[0].replies[1].text.as_deref(), Some("there"));
        assert!(entries[0].replies[1].done);
    }

    #[test]
    fn a_reply_to_an_unrecorded_prompt_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let log = TalkLog::new(dir.path().to_path_buf());
        log.record_reply("no-such-id", "alpha", &reply("stray", true));
        assert!(log.read("alpha").is_empty());
    }

    #[test]
    fn sessions_are_isolated_by_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = TalkLog::new(dir.path().to_path_buf());
        log.record_prompt("id-1", "alpha", "hello alpha");
        log.record_prompt("id-2", "beta", "hello beta");

        assert_eq!(log.read("alpha").len(), 1);
        assert_eq!(log.read("beta").len(), 1);
        assert_eq!(log.read("alpha")[0].prompt_text, "hello alpha");
    }

    #[test]
    fn a_path_traversal_session_name_stays_inside_the_talk_dir() {
        let dir = tempfile::tempdir().unwrap();
        let log = TalkLog::new(dir.path().to_path_buf());
        log.record_prompt("id-1", "../../etc/passwd", "pwned?");

        let path = log.path_for("../../etc/passwd");
        assert!(path.starts_with(&log.dir), "{path:?} escaped {:?}", log.dir);
        assert_eq!(log.read("../../etc/passwd").len(), 1);
    }
}
