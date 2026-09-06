//! Debug levels and output formats for holler-server, with secret redaction.
//!
//! Two independent axes, mirroring holler-client's module of the same name
//! so both halves of the circuit produce logs an analyzer can correlate:
//!
//! - **Level** ([`DebugLevel`]): `none` / `quiet` / `noisy` — *how much* is
//!   logged (issue #38, ADR 0010). Secrets are never printed in the clear.
//! - **Format** ([`LogFormat`]): `text` / `json` — *how it is shaped*
//!   (issue #230). `text` is a fixed-width console line for a human
//!   tailing a session; `json` is JSON Lines for a log analyzer.
//!
//! Precedence for both: an explicit flag (`--debug=` / `--log-format=`)
//! wins over the environment (`HOLLER_DEBUG` / `HOLLER_LOG_FORMAT`); if
//! neither is set the default applies ([`DebugLevel::None`],
//! [`LogFormat::Text`]). An invalid value at whichever precedence level
//! wins is an error — it never silently falls back to a default.
//!
//! # Emission timestamp vs. frame `ts`
//!
//! Every line carries an **emission** timestamp: when *this* process
//! logged the line, from *this* host's clock. That is deliberately not
//! the frame's own `ts` field, which is the peer's claim from the peer's
//! clock. The two diverge in practice — a measured cross-machine session
//! showed ~180ms of skew, enough that sorting a handshake by frame `ts`
//! reordered it against causality — so only the emission timestamp is
//! safe to sort a log by. The frame `ts` is still present inside the
//! frame body at `noisy`, as protocol data.
//!
//! # One grammar for every line
//!
//! Frame lines and non-frame lifecycle lines (connect / disconnect /
//! authenticated) share a single envelope: the latter are
//! [`Direction::Local`] events carrying an `event` field instead of a
//! direction and frame, so a parser never has to special-case them.

use std::fmt;

use serde::Serialize;
use time::OffsetDateTime;

/// The three supported debug verbosity levels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DebugLevel {
    /// No debug statements.
    #[default]
    None,
    /// High-level communication only: direction, envelope type, session/query cmd,
    /// peer hostname / public token_id, correlation id. No bodies.
    Quiet,
    /// All handshakes and frames, JSON one line, secrets redacted.
    Noisy,
}

/// Error returned when a `--debug=`/`HOLLER_DEBUG` value is not one of `none`/`quiet`/`noisy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugLevelError {
    pub value: String,
}

impl fmt::Display for DebugLevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid debug level {:?}: expected one of none, quiet, noisy",
            self.value
        )
    }
}

impl std::error::Error for DebugLevelError {}

impl DebugLevel {
    /// Parses a debug level, case-insensitively. Only `none`/`quiet`/`noisy` are accepted.
    pub fn parse(value: &str) -> Result<DebugLevel, DebugLevelError> {
        match value.to_ascii_lowercase().as_str() {
            "none" => Ok(DebugLevel::None),
            "quiet" => Ok(DebugLevel::Quiet),
            "noisy" => Ok(DebugLevel::Noisy),
            _ => Err(DebugLevelError {
                value: value.to_string(),
            }),
        }
    }

    /// Resolves the effective debug level from an optional flag value and an optional
    /// env value. The flag wins outright over env: if the flag is present, it is parsed
    /// and its result (success or error) is returned without consulting env at all. If
    /// the flag is absent, the env value (if present) is parsed instead. If neither is
    /// present, defaults to `DebugLevel::None`. Never falls through to `Noisy` or any
    /// other default on an invalid value — invalid input always errors.
    pub fn resolve(
        flag: Option<&str>,
        env: Option<&str>,
    ) -> Result<DebugLevel, DebugLevelError> {
        if let Some(flag_value) = flag {
            return DebugLevel::parse(flag_value);
        }
        if let Some(env_value) = env {
            return DebugLevel::parse(env_value);
        }
        Ok(DebugLevel::default())
    }
}

/// How each debug line is shaped on the wire to stderr.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Fixed-width console line, emission timestamp first. At
    /// [`DebugLevel::Noisy`] the redacted frame JSON is appended last, so
    /// a line's frame can still be copied out and replayed.
    #[default]
    Text,
    /// JSON Lines: the whole line is one JSON object, so `jq`/Vector/Loki
    /// can ingest the stream directly. The frame is nested under `frame`.
    Json,
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LogFormat::Text => "text",
            LogFormat::Json => "json",
        })
    }
}

impl fmt::Display for DebugLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DebugLevel::None => "none",
            DebugLevel::Quiet => "quiet",
            DebugLevel::Noisy => "noisy",
        })
    }
}

/// Error returned when a `--log-format=`/`HOLLER_LOG_FORMAT` value is not
/// one of `text`/`json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFormatError {
    pub value: String,
}

impl fmt::Display for LogFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid log format {:?}: expected one of text, json",
            self.value
        )
    }
}

impl std::error::Error for LogFormatError {}

impl LogFormat {
    /// Parses a log format, case-insensitively. Only `text`/`json` are accepted.
    pub fn parse(value: &str) -> Result<LogFormat, LogFormatError> {
        match value.to_ascii_lowercase().as_str() {
            "text" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            _ => Err(LogFormatError {
                value: value.to_string(),
            }),
        }
    }

    /// Resolves the effective log format. Same flag-wins-over-env,
    /// fail-closed contract as [`DebugLevel::resolve`].
    pub fn resolve(flag: Option<&str>, env: Option<&str>) -> Result<LogFormat, LogFormatError> {
        if let Some(flag_value) = flag {
            return LogFormat::parse(flag_value);
        }
        if let Some(env_value) = env {
            return LogFormat::parse(env_value);
        }
        Ok(LogFormat::default())
    }
}

/// The resolved logging configuration: how much, and in what shape.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DebugConfig {
    pub level: DebugLevel,
    pub format: LogFormat,
}

impl DebugConfig {
    pub fn new(level: DebugLevel, format: LogFormat) -> Self {
        DebugConfig { level, format }
    }

    /// Whether anything at all is logged. Call sites use this to skip work
    /// that even building an [`Event`] would cost.
    pub fn is_on(&self) -> bool {
        self.level != DebugLevel::None
    }
}

/// Which way a frame moved, relative to this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Sent by this process.
    Out,
    /// Received by this process.
    In,
    /// Not a frame at all — a local lifecycle event (connect, disconnect,
    /// authenticated). Rendered with a blank direction column so the rest
    /// of the columns still line up, and with no `dir` key in `json`.
    Local,
}

impl Direction {
    fn as_text(self) -> &'static str {
        match self {
            Direction::Out => "->",
            Direction::In => "<-",
            Direction::Local => "  ",
        }
    }

    fn as_json(self) -> Option<&'static str> {
        match self {
            Direction::Out => Some("out"),
            Direction::In => Some("in"),
            Direction::Local => None,
        }
    }
}

/// Width the `type` column is padded to in [`LogFormat::Text`]. The
/// longest v1 frame type is `interrupt` (9); one space of slack keeps a
/// gap before the first `k=v` pair.
const TYPE_COLUMN_WIDTH: usize = 10;

/// How many leading characters of an id/peer survive into a `text` line.
/// Enough to stay distinctive past a `cli_`/`tok_` prefix while keeping
/// the column narrow; `json` always carries the untruncated value.
const SHORT_ID_LEN: usize = 12;

fn short(value: &str) -> &str {
    match value.char_indices().nth(SHORT_ID_LEN) {
        Some((byte_idx, _)) => &value[..byte_idx],
        None => value,
    }
}

/// RFC 3339 UTC at fixed microsecond precision, so the timestamp column is
/// genuinely fixed-width (27 chars) and a parser never has to handle
/// variable fractional digits. Formatted by hand rather than via a `time`
/// format description, which would render a variable number of subsecond
/// digits.
fn emission_ts() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.microsecond(),
    )
}

#[derive(Serialize)]
struct JsonLine<'a> {
    ts: String,
    level: &'static str,
    verbosity: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<&'static str>,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<&'a str>,
    #[serde(flatten)]
    fields: std::collections::BTreeMap<&'static str, &'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame: Option<serde_json::Value>,
}

/// One debug line, built up field by field and rendered by [`Event::emit`]
/// in whichever [`LogFormat`] is configured.
///
/// Construct with [`outgoing`], [`incoming`], or [`local`]. When the
/// configured level is [`DebugLevel::None`] every method is a no-op and
/// the [`Event::frame`]/[`Event::frame_of`] closures are never called, so
/// a disabled logger costs nothing beyond the (stack-only) builder itself.
pub struct Event<'a> {
    cfg: DebugConfig,
    dir: Direction,
    kind: &'a str,
    id: Option<&'a str>,
    peer: Option<&'a str>,
    fields: Vec<(&'static str, String)>,
    frame: Option<String>,
}

/// A frame this process is sending.
pub fn outgoing(cfg: DebugConfig, kind: &str) -> Event<'_> {
    Event::new(cfg, Direction::Out, kind)
}

/// A frame this process just received.
pub fn incoming(cfg: DebugConfig, kind: &str) -> Event<'_> {
    Event::new(cfg, Direction::In, kind)
}

/// A local lifecycle event, not a frame — connect, disconnect,
/// authenticated. Carries an `event` field instead of a direction, so
/// every line in the stream still shares one grammar (issue #230).
pub fn local(cfg: DebugConfig, kind: &str) -> Event<'_> {
    Event::new(cfg, Direction::Local, kind)
}

impl<'a> Event<'a> {
    fn new(cfg: DebugConfig, dir: Direction, kind: &'a str) -> Self {
        Event {
            cfg,
            dir,
            kind,
            id: None,
            peer: None,
            fields: Vec::new(),
            frame: None,
        }
    }

    /// The frame's correlation id.
    pub fn id(mut self, id: &'a str) -> Self {
        self.id = Some(id);
        self
    }

    /// The other end: a `token_id`, `client_id`, or `server`. Never a
    /// secret — see [`redact`] for what must never appear.
    pub fn peer(mut self, peer: &'a str) -> Self {
        self.peer = Some(peer);
        self
    }

    /// An extra `k=v` detail (session name, query cmd, outcome, ...).
    pub fn field(mut self, key: &'static str, value: impl Into<String>) -> Self {
        if self.cfg.is_on() {
            self.fields.push((key, value.into()));
        }
        self
    }

    /// The frame body as pre-rendered JSON text, materialized **only** at
    /// [`DebugLevel::Noisy`] — the closure is not called at any other
    /// level, so redaction and serialization cost nothing when they would
    /// be thrown away.
    ///
    /// The caller is responsible for having already redacted secrets out
    /// of what the closure returns (see [`redact_secret`]). Prefer
    /// [`Event::frame_of`] for a value that is already [`Serialize`].
    pub fn frame(mut self, render: impl FnOnce() -> String) -> Self {
        if self.cfg.level == DebugLevel::Noisy {
            self.frame = Some(render());
        }
        self
    }

    /// The frame body as a [`Serialize`] value — the ergonomic form for
    /// this crate, whose frames are already typed. Same noisy-only
    /// laziness as [`Event::frame`].
    ///
    /// [`redact_hlr_tokens`] is applied to the serialized JSON as
    /// defense-in-depth: none of v1's server-sent frames should carry
    /// `hlr_`-prefixed secret material, and this makes that true by
    /// construction rather than by inspection. A value that somehow fails
    /// to serialize drops the frame rather than risking a panic in a live
    /// server's debug path.
    pub fn frame_of<T: Serialize>(mut self, render: impl FnOnce() -> T) -> Self {
        if self.cfg.level == DebugLevel::Noisy {
            if let Ok(json) = serde_json::to_string(&render()) {
                self.frame = Some(redact_hlr_tokens(&json));
            }
        }
        self
    }

    /// Writes the line to stderr, or does nothing at [`DebugLevel::None`].
    pub fn emit(self) {
        if !self.cfg.is_on() {
            return;
        }
        match self.cfg.format {
            LogFormat::Text => eprintln!("{}", self.render_text()),
            LogFormat::Json => eprintln!("{}", self.render_json()),
        }
    }

    fn render_text(&self) -> String {
        let mut line = format!(
            "{} DEBUG {} {:<width$}",
            emission_ts(),
            self.dir.as_text(),
            self.kind,
            width = TYPE_COLUMN_WIDTH,
        );
        if let Some(id) = self.id {
            line.push_str(&format!(" id={}", short(id)));
        }
        if let Some(peer) = self.peer {
            line.push_str(&format!(" peer={}", short(peer)));
        }
        for (key, value) in &self.fields {
            line.push_str(&format!(" {key}={value}"));
        }
        if let Some(frame) = &self.frame {
            line.push(' ');
            line.push_str(frame);
        }
        line
    }

    fn render_json(&self) -> String {
        // A frame that somehow doesn't re-parse is still worth logging:
        // fall back to carrying it as a JSON string rather than dropping
        // the line entirely.
        let frame = self.frame.as_ref().map(|raw| {
            serde_json::from_str::<serde_json::Value>(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.clone()))
        });
        let line = JsonLine {
            ts: emission_ts(),
            level: "debug",
            verbosity: match self.cfg.level {
                DebugLevel::Noisy => "noisy",
                _ => "quiet",
            },
            dir: self.dir.as_json(),
            kind: self.kind,
            id: self.id,
            peer: self.peer,
            fields: self.fields.iter().map(|(k, v)| (*k, v.as_str())).collect(),
            frame,
        };
        serde_json::to_string(&line).unwrap_or_else(|_| {
            format!(
                r#"{{"ts":"{}","level":"debug","error":"unserializable log line"}}"#,
                emission_ts()
            )
        })
    }
}

/// Field names that must never have their value printed, regardless of debug level.
/// Matched case-insensitively.
const NEVER_PRINT_FIELDS: &[&str] = &[
    "join secret",
    "join_secret",
    "client credential",
    "client_credential",
    "connect ticket",
    "connect_ticket",
    "authorization",
    "pepper",
];

const REDACTED: &str = "[redacted]";

/// Redacts `value` if `field_name` matches a known never-print field (case-insensitive).
/// Fields not on the denylist (e.g. `token_id`, `client_id`, hostname, session names,
/// correlation ids) pass through unchanged.
pub fn redact(field_name: &str, value: &str) -> String {
    let normalized = field_name.to_ascii_lowercase();
    if NEVER_PRINT_FIELDS.contains(&normalized.as_str()) {
        REDACTED.to_string()
    } else {
        value.to_string()
    }
}

/// Scrubs all occurrences of a known secret substring from arbitrary text, for secrets
/// that aren't cleanly attached to a named field. No-op if `secret` is empty or absent
/// from `text`.
pub fn redact_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, REDACTED)
}

/// Redacts any substring in `text` matching the `hlr_` secret-token pattern: the literal
/// prefix `hlr_` followed by a run of one or more base62-ish characters (ASCII letters,
/// digits, `-`, `_`). This is defense-in-depth beyond the named-field denylist, catching
/// `hlr_`-prefixed secret material (per ADR 0010) that ends up in a string without a
/// recognized field name attached. A plain identifier that never carries the `hlr_`
/// prefix (e.g. a `token_id` scheme that doesn't use this convention) is left untouched.
pub fn redact_hlr_tokens(text: &str) -> String {
    const PREFIX: &str = "hlr_";
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PREFIX) {
        result.push_str(&rest[..start]);
        let after_prefix = &rest[start + PREFIX.len()..];
        let token_len = after_prefix
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(after_prefix.len());
        if token_len == 0 {
            // "hlr_" with nothing following it isn't secret material; keep it verbatim.
            result.push_str(PREFIX);
        } else {
            result.push_str(REDACTED);
        }
        rest = &after_prefix[token_len..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DebugLevel::resolve precedence ---

    #[test]
    fn resolve_defaults_to_none_when_neither_set() {
        assert_eq!(DebugLevel::resolve(None, None), Ok(DebugLevel::None));
    }

    #[test]
    fn resolve_uses_env_when_flag_absent() {
        assert_eq!(
            DebugLevel::resolve(None, Some("quiet")),
            Ok(DebugLevel::Quiet)
        );
        assert_eq!(
            DebugLevel::resolve(None, Some("noisy")),
            Ok(DebugLevel::Noisy)
        );
    }

    #[test]
    fn resolve_flag_wins_over_env_same_value() {
        assert_eq!(
            DebugLevel::resolve(Some("quiet"), Some("quiet")),
            Ok(DebugLevel::Quiet)
        );
    }

    #[test]
    fn resolve_flag_wins_over_env_different_value() {
        assert_eq!(
            DebugLevel::resolve(Some("noisy"), Some("quiet")),
            Ok(DebugLevel::Noisy)
        );
        assert_eq!(
            DebugLevel::resolve(Some("none"), Some("noisy")),
            Ok(DebugLevel::None)
        );
    }

    #[test]
    fn resolve_invalid_flag_errors_without_consulting_env() {
        let result = DebugLevel::resolve(Some("bogus"), Some("noisy"));
        assert_eq!(
            result,
            Err(DebugLevelError {
                value: "bogus".to_string()
            })
        );
    }

    #[test]
    fn resolve_invalid_env_errors_when_flag_absent() {
        let result = DebugLevel::resolve(None, Some("bogus"));
        assert_eq!(
            result,
            Err(DebugLevelError {
                value: "bogus".to_string()
            })
        );
    }

    #[test]
    fn resolve_never_falls_through_to_noisy_on_invalid_value() {
        let result = DebugLevel::resolve(Some("loud"), None);
        assert!(result.is_err());
        assert_ne!(result, Ok(DebugLevel::Noisy));
    }

    // --- case-insensitive parsing ---

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(DebugLevel::parse("NONE"), Ok(DebugLevel::None));
        assert_eq!(DebugLevel::parse("Quiet"), Ok(DebugLevel::Quiet));
        assert_eq!(DebugLevel::parse("NoIsY"), Ok(DebugLevel::Noisy));
    }

    #[test]
    fn parse_rejects_unknown_values() {
        assert!(DebugLevel::parse("verbose").is_err());
        assert!(DebugLevel::parse("").is_err());
    }

    // --- redact() ---

    #[test]
    fn redact_hides_never_print_fields() {
        assert_eq!(redact("join secret", "s3cr3t"), REDACTED);
        assert_eq!(redact("join_secret", "s3cr3t"), REDACTED);
        assert_eq!(redact("client credential", "cred"), REDACTED);
        assert_eq!(redact("client_credential", "cred"), REDACTED);
        assert_eq!(redact("connect ticket", "tkt"), REDACTED);
        assert_eq!(redact("connect_ticket", "tkt"), REDACTED);
        assert_eq!(redact("Authorization", "Bearer abc"), REDACTED);
        assert_eq!(redact("AUTHORIZATION", "Bearer abc"), REDACTED);
        assert_eq!(redact("pepper", "p3pp3r"), REDACTED);
        assert_eq!(redact("PEPPER", "p3pp3r"), REDACTED);
    }

    #[test]
    fn redact_passes_through_allowed_fields() {
        assert_eq!(redact("token_id", "tok_abc123"), "tok_abc123");
        assert_eq!(redact("client_id", "cli_xyz"), "cli_xyz");
        assert_eq!(redact("hostname", "myhost.local"), "myhost.local");
        assert_eq!(redact("session_name", "my-session"), "my-session");
        assert_eq!(redact("correlation_id", "corr-123"), "corr-123");
    }

    // --- redact_secret() ---

    #[test]
    fn redact_secret_removes_single_occurrence() {
        let text = "auth failed for secret=s3cr3tvalue in request";
        assert_eq!(
            redact_secret(text, "s3cr3tvalue"),
            "auth failed for secret=[redacted] in request"
        );
    }

    #[test]
    fn redact_secret_removes_multiple_occurrences() {
        let text = "s3cr3t appears twice: s3cr3t";
        assert_eq!(
            redact_secret(text, "s3cr3t"),
            "[redacted] appears twice: [redacted]"
        );
    }

    #[test]
    fn redact_secret_is_noop_when_absent() {
        let text = "nothing sensitive here";
        assert_eq!(redact_secret(text, "s3cr3t"), text);
    }

    // --- redact_hlr_tokens() ---

    #[test]
    fn redact_hlr_tokens_redacts_embedded_token() {
        let text = "connecting with token hlr_AbC123-xyz_789 now";
        assert_eq!(
            redact_hlr_tokens(text),
            "connecting with token [redacted] now"
        );
    }

    #[test]
    fn redact_hlr_tokens_redacts_multiple() {
        let text = "hlr_first123 and hlr_second456";
        assert_eq!(redact_hlr_tokens(text), "[redacted] and [redacted]");
    }

    #[test]
    fn redact_hlr_tokens_leaves_clean_text_untouched() {
        let text = "no secret material in this line";
        assert_eq!(redact_hlr_tokens(text), text);
    }

    #[test]
    fn redact_hlr_tokens_does_not_over_redact_non_hlr_token_id() {
        // A token_id scheme that doesn't use the hlr_ prefix convention is unaffected.
        let text = "token_id=tok_abc123 client_id=cli_xyz789";
        assert_eq!(redact_hlr_tokens(text), text);
    }

    #[test]
    fn redact_hlr_tokens_bare_prefix_with_no_token_is_kept_verbatim() {
        let text = "saw hlr_ with nothing after it";
        assert_eq!(redact_hlr_tokens(text), text);
    }

    // --- LogFormat::resolve precedence (issue #230) ---

    #[test]
    fn log_format_defaults_to_text() {
        assert_eq!(LogFormat::resolve(None, None), Ok(LogFormat::Text));
    }

    #[test]
    fn log_format_flag_wins_over_env() {
        assert_eq!(
            LogFormat::resolve(Some("text"), Some("json")),
            Ok(LogFormat::Text)
        );
    }

    #[test]
    fn log_format_env_used_when_flag_absent() {
        assert_eq!(LogFormat::resolve(None, Some("json")), Ok(LogFormat::Json));
    }

    #[test]
    fn log_format_invalid_flag_errors_without_consulting_env() {
        assert_eq!(
            LogFormat::resolve(Some("yaml"), Some("json")),
            Err(LogFormatError {
                value: "yaml".to_string()
            })
        );
    }

    #[test]
    fn log_format_invalid_env_errors_when_flag_absent() {
        assert!(LogFormat::resolve(None, Some("logfmt")).is_err());
    }

    #[test]
    fn log_format_parse_is_case_insensitive() {
        assert_eq!(LogFormat::parse("JSON"), Ok(LogFormat::Json));
        assert_eq!(LogFormat::parse("Text"), Ok(LogFormat::Text));
    }

    // --- line rendering ---

    fn noisy_json() -> DebugConfig {
        DebugConfig::new(DebugLevel::Noisy, LogFormat::Json)
    }

    fn noisy_text() -> DebugConfig {
        DebugConfig::new(DebugLevel::Noisy, LogFormat::Text)
    }

    #[test]
    fn emission_ts_is_fixed_width_rfc3339_micros() {
        let ts = emission_ts();
        // 2026-09-06T20:59:54.712345Z
        assert_eq!(ts.len(), 27, "ts was {ts:?}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
    }

    #[test]
    fn text_line_starts_with_timestamp_then_fixed_columns() {
        let line = outgoing(noisy_text(), "prompt").id("abc123").render_text();
        assert_eq!(&line[26..27], "Z", "ts should occupy the first column");
        assert!(line.contains(" DEBUG -> prompt     "), "line was {line:?}");
    }

    #[test]
    fn json_line_is_a_single_parseable_object_with_ts_first() {
        let line = incoming(noisy_json(), "reply")
            .id("id-1")
            .peer("cli_e105a5cd")
            .render_json();
        assert!(line.starts_with(r#"{"ts":"#), "line was {line:?}");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(parsed["type"], "reply");
        assert_eq!(parsed["dir"], "in");
        assert_eq!(parsed["id"], "id-1");
        assert_eq!(parsed["peer"], "cli_e105a5cd");
    }

    #[test]
    fn json_nests_the_frame_as_an_object_not_a_string() {
        let line = outgoing(noisy_json(), "prompt")
            .frame(|| r#"{"v":1,"type":"prompt"}"#.to_string())
            .render_json();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["frame"]["v"], 1);
        assert_eq!(parsed["frame"]["type"], "prompt");
    }

    #[test]
    fn frame_of_serializes_and_nests_a_typed_value() {
        #[derive(Serialize)]
        struct Body {
            session: &'static str,
            done: bool,
        }
        let line = incoming(noisy_json(), "reply")
            .frame_of(|| Body {
                session: "m1",
                done: true,
            })
            .render_json();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["frame"]["session"], "m1");
        assert_eq!(parsed["frame"]["done"], true);
    }

    #[test]
    fn frame_of_redacts_hlr_secret_material_as_defense_in_depth() {
        #[derive(Serialize)]
        struct Body {
            credential: &'static str,
        }
        let line = outgoing(noisy_json(), "join_ok")
            .frame_of(|| Body {
                credential: "hlr_live_abc123",
            })
            .render_json();
        assert!(!line.contains("hlr_live_abc123"), "line was {line:?}");
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["frame"]["credential"], REDACTED);
    }

    #[test]
    fn json_carries_the_full_untruncated_id() {
        let long_id = "c14fb1a960b3d14d690e652e53b8b33a";
        let line = outgoing(noisy_json(), "ping").id(long_id).render_json();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], long_id);
    }

    #[test]
    fn text_truncates_long_ids_for_scannability() {
        let long_id = "c14fb1a960b3d14d690e652e53b8b33a";
        let line = outgoing(noisy_text(), "ping").id(long_id).render_text();
        assert!(line.contains("id=c14fb1a960b3"), "line was {line:?}");
        assert!(!line.contains(long_id));
    }

    #[test]
    fn quiet_never_materializes_the_frame() {
        let cfg = DebugConfig::new(DebugLevel::Quiet, LogFormat::Json);
        let mut called = false;
        let line = outgoing(cfg, "prompt")
            .frame(|| {
                called = true;
                "{}".to_string()
            })
            .render_json();
        assert!(!called, "frame closure must not run below noisy");
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(parsed.get("frame").is_none());
        assert_eq!(parsed["verbosity"], "quiet");
    }

    #[test]
    fn none_level_never_materializes_the_frame_either() {
        let cfg = DebugConfig::new(DebugLevel::None, LogFormat::Text);
        let mut called = false;
        outgoing(cfg, "prompt")
            .frame(|| {
                called = true;
                "{}".to_string()
            })
            .emit();
        assert!(!called);
        assert!(!cfg.is_on());
    }

    #[test]
    fn local_events_carry_an_event_field_and_no_direction() {
        let line = local(noisy_json(), "conn")
            .field("event", "authenticated")
            .field("addr", "127.0.0.1:42258")
            .render_json();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(parsed.get("dir").is_none());
        assert!(parsed.get("frame").is_none());
        assert_eq!(parsed["event"], "authenticated");
        assert_eq!(parsed["addr"], "127.0.0.1:42258");
        assert_eq!(parsed["type"], "conn");
    }

    #[test]
    fn extra_fields_are_flattened_not_nested() {
        let line = outgoing(noisy_json(), "query")
            .field("cmd", "status")
            .render_json();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["cmd"], "status", "line was {line:?}");
    }
}
