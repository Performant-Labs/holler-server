//! Join tokens: mint / list / delete / ping (issue #29).
//!
//! One durable record per join (`token_id`, prefixed `tok_`). The join
//! **secret** (prefixed `hlr_`, per ADR 0010) is separate: TTL,
//! single-use, shown once at mint, persisted only as an
//! HMAC-SHA-256(pepper, secret) hash. This is not WebSocket session
//! auth — that is the credential minted at redeem (issue #30).
//!
//! Storage is a single JSON file (no database engine needed for a
//! CLI-only store with no live server yet): `HOLLER_STATE_DIR`
//! (default `./holler-server-state`) holding `tokens.json`. Concurrent
//! CLI invocations are not made safe (no file locking) — out of scope
//! for a local operator tool with a single caller at a time.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

type HmacSha256 = Hmac<Sha256>;

const STATE_DIR_ENV: &str = "HOLLER_STATE_DIR";
const DEFAULT_STATE_DIR: &str = "./holler-server-state";
const STATE_FILE: &str = "tokens.json";
const PEPPER_ENV: &str = "HOLLER_SERVER_PEPPER";

/// Default TTL applied to a minted secret when `--ttl` is omitted.
pub const DEFAULT_TTL: Duration = Duration::hours(24);

// --------------------------------------------------------------------------
// Errors
// --------------------------------------------------------------------------

/// Failures from the token store.
#[derive(Debug)]
pub enum TokenError {
    /// `HOLLER_SERVER_PEPPER` is unset. Fail closed (ADR 0009's pattern,
    /// mirrored here): never mint against an empty/default pepper.
    PepperMissing,
    /// `--ttl` did not parse (expects `<n><s|m|h|d>`, e.g. `24h`).
    InvalidTtl(String),
    /// No record with this `token_id`.
    NotFound(String),
    /// `ping` on a token that was never redeemed (or whose secret is
    /// stale/invalidated) — nothing to ping.
    Unbound(String),
    /// `ping` on a bound token with no live connection to it.
    Disconnected(String),
    Io(io::Error),
    Serde(serde_json::Error),
    /// The OS CSPRNG failed to produce randomness.
    Rng(getrandom::Error),
    /// Time formatting/parsing failed (should not happen for values we
    /// generated ourselves).
    Time(String),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::PepperMissing => write!(
                f,
                "{PEPPER_ENV} is not set; refusing to mint a token without a server pepper"
            ),
            TokenError::InvalidTtl(s) => {
                write!(f, "invalid --ttl {s:?} (expected e.g. 30m, 24h, 7d)")
            }
            TokenError::NotFound(id) => write!(f, "no such token: {id}"),
            TokenError::Unbound(id) => write!(f, "token {id} has not been redeemed; nothing to ping"),
            TokenError::Disconnected(id) => write!(f, "token {id} is bound but not connected"),
            TokenError::Io(e) => write!(f, "token store I/O error: {e}"),
            TokenError::Serde(e) => write!(f, "token store is corrupt: {e}"),
            TokenError::Rng(e) => write!(f, "failed to generate randomness: {e}"),
            TokenError::Time(msg) => write!(f, "time error: {msg}"),
        }
    }
}

impl Error for TokenError {}

impl From<io::Error> for TokenError {
    fn from(e: io::Error) -> Self {
        TokenError::Io(e)
    }
}

impl From<serde_json::Error> for TokenError {
    fn from(e: serde_json::Error) -> Self {
        TokenError::Serde(e)
    }
}

impl From<getrandom::Error> for TokenError {
    fn from(e: getrandom::Error) -> Self {
        TokenError::Rng(e)
    }
}

impl From<time::error::Format> for TokenError {
    fn from(e: time::error::Format) -> Self {
        TokenError::Time(e.to_string())
    }
}

// --------------------------------------------------------------------------
// Record & state
// --------------------------------------------------------------------------

/// The durable, persisted state of a token record.
///
/// `Stale` is not stored here — it is derived at read time from
/// `Unused` + an elapsed `expires` (see [`TokenRecord::display_state`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum StoredState {
    /// Minted, secret not yet redeemed, not (yet) expired.
    Unused,
    /// Secret was redeemed (issue #30); a live credential exists.
    Bound,
    /// Deleted while unused (`delete`'s "invalidate" case).
    Invalidated,
    /// Deleted while bound (`delete`'s "revoke" case; full socket-drop
    /// + credential revocation lands with #30).
    Revoked,
}

/// The state string shown by `list` / `ping`, including the read-time
/// derived `stale` case.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayState {
    Unused,
    Bound,
    Stale,
    Invalidated,
    Revoked,
}

impl fmt::Display for DisplayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DisplayState::Unused => "unused",
            DisplayState::Bound => "bound",
            DisplayState::Stale => "stale",
            DisplayState::Invalidated => "invalidated",
            DisplayState::Revoked => "revoked",
        };
        f.write_str(s)
    }
}

/// One durable join-token record. Never carries the plaintext secret —
/// only the HMAC hash, persisted for comparison at redeem (#30).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TokenRecord {
    pub token_id: String,
    secret_hash: String,
    state: StoredState,
    pub label: Option<String>,
    /// OS hostname of the redeeming client. Filled at redeem (#30);
    /// `None` until then.
    pub machine: Option<String>,
    pub created_at: String,
    /// RFC 3339 expiry of the *unused secret*. Irrelevant once bound.
    pub expires: String,
    pub last_seen: Option<String>,
}

impl TokenRecord {
    /// The state as `list`/`ping` should show it: `Unused` becomes
    /// `Stale` once `expires` is in the past.
    pub fn display_state(&self, now: OffsetDateTime) -> DisplayState {
        match self.state {
            StoredState::Unused => match OffsetDateTime::parse(&self.expires, &Rfc3339) {
                Ok(expires) if expires <= now => DisplayState::Stale,
                _ => DisplayState::Unused,
            },
            StoredState::Bound => DisplayState::Bound,
            StoredState::Invalidated => DisplayState::Invalidated,
            StoredState::Revoked => DisplayState::Revoked,
        }
    }
}

/// A `list`-safe view of a record: no `secret_hash` field exists on
/// this type at all, so it cannot leak the hash by omission bugs the
/// way reusing [`TokenRecord`] for output could.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct TokenView {
    pub token_id: String,
    pub state: String,
    pub machine: String,
    pub label: String,
    pub last_seen: String,
    pub expires: String,
}

impl TokenView {
    fn from_record(record: &TokenRecord, now: OffsetDateTime) -> Self {
        TokenView {
            token_id: record.token_id.clone(),
            state: record.display_state(now).to_string(),
            machine: record.machine.clone().unwrap_or_else(|| "-".to_string()),
            label: record.label.clone().unwrap_or_else(|| "-".to_string()),
            last_seen: record.last_seen.clone().unwrap_or_else(|| "-".to_string()),
            expires: record.expires.clone(),
        }
    }
}

/// Result of a successful mint: the secret is returned here and
/// nowhere else — the store never holds it, only its hash.
#[derive(Debug)]
pub struct MintResult {
    pub token_id: String,
    pub secret: String,
    pub expires: String,
}

/// The outcome of a successful `ping` against a bound token.
#[derive(Debug)]
pub enum PingOutcome {
    Connected { hostname: String, rtt_ms: u64 },
}

/// Liveness check for a bound token's WebSocket connection.
///
/// No listener exists yet (issue #31 is only building the test
/// harness so far) — this trait is the seam #31 will implement for
/// real. [`AlwaysDisconnected`] is the only implementation today and
/// is what every `ping` currently reports for a bound token.
pub trait ConnectionProbe {
    fn probe(&self, token_id: &str) -> ConnectionStatus;
}

pub enum ConnectionStatus {
    Disconnected,
    Connected { hostname: String, rtt_ms: u64 },
}

/// Default probe until #31 wires in a real liveness check.
pub struct AlwaysDisconnected;

impl ConnectionProbe for AlwaysDisconnected {
    fn probe(&self, _token_id: &str) -> ConnectionStatus {
        ConnectionStatus::Disconnected
    }
}

// --------------------------------------------------------------------------
// Store
// --------------------------------------------------------------------------

pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// Open the store at `HOLLER_STATE_DIR` (default
    /// `./holler-server-state`), creating the directory if needed.
    pub fn open() -> Result<Self, TokenError> {
        let dir = std::env::var(STATE_DIR_ENV).unwrap_or_else(|_| DEFAULT_STATE_DIR.to_string());
        let dir = PathBuf::from(dir);
        fs::create_dir_all(&dir)?;
        Ok(TokenStore {
            path: dir.join(STATE_FILE),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        TokenStore { path }
    }

    fn load(&self) -> Result<Vec<TokenRecord>, TokenError> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => {
                if contents.trim().is_empty() {
                    Ok(Vec::new())
                } else {
                    Ok(serde_json::from_str(&contents)?)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, records: &[TokenRecord]) -> Result<(), TokenError> {
        let json = serde_json::to_string_pretty(records)?;
        fs::write(&self.path, json)?;
        Ok(())
    }

    /// Mint a new token: generate a `token_id` + secret, persist only
    /// the HMAC hash of the secret, and return the secret once.
    ///
    /// Fails closed if `HOLLER_SERVER_PEPPER` is unset (ADR 0009's
    /// fail-closed pattern, mirrored here): a missing pepper must never
    /// silently downgrade to an empty/default key.
    pub fn mint(&self, label: Option<String>, ttl: Duration) -> Result<MintResult, TokenError> {
        let pepper = std::env::var(PEPPER_ENV).map_err(|_| TokenError::PepperMissing)?;
        if pepper.is_empty() {
            return Err(TokenError::PepperMissing);
        }
        self.mint_with_pepper(pepper.as_bytes(), label, ttl)
    }

    /// The pepper-taking core of [`TokenStore::mint`], split out so
    /// tests can supply a pepper directly instead of mutating the
    /// process-global `HOLLER_SERVER_PEPPER` env var (which would race
    /// under the parallel test runner).
    fn mint_with_pepper(
        &self,
        pepper: &[u8],
        label: Option<String>,
        ttl: Duration,
    ) -> Result<MintResult, TokenError> {
        let token_id = format!("tok_{}", random_hex(16)?);
        let secret = format!("hlr_{}", random_hex(32)?);
        let secret_hash = hash_secret(pepper, &secret);

        let now = OffsetDateTime::now_utc();
        let expires = (now + ttl).format(&Rfc3339)?;
        let created_at = now.format(&Rfc3339)?;

        let record = TokenRecord {
            token_id: token_id.clone(),
            secret_hash,
            state: StoredState::Unused,
            label,
            machine: None,
            created_at,
            expires: expires.clone(),
            last_seen: None,
        };

        let mut records = self.load()?;
        records.push(record);
        self.save(&records)?;

        Ok(MintResult {
            token_id,
            secret,
            expires,
        })
    }

    /// All records (unused and bound), display-ready and secret-free.
    pub fn list(&self) -> Result<Vec<TokenView>, TokenError> {
        let now = OffsetDateTime::now_utc();
        let records = self.load()?;
        Ok(records
            .iter()
            .map(|r| TokenView::from_record(r, now))
            .collect())
    }

    /// Transition a record's state: unused/stale -> invalidated,
    /// bound -> revoked. The actual socket-drop for a bound token
    /// lands with #30; this only corrects the stored state.
    pub fn delete(&self, token_id: &str) -> Result<DisplayState, TokenError> {
        let mut records = self.load()?;
        let record = records
            .iter_mut()
            .find(|r| r.token_id == token_id)
            .ok_or_else(|| TokenError::NotFound(token_id.to_string()))?;

        record.state = match record.state {
            StoredState::Bound => StoredState::Revoked,
            StoredState::Unused | StoredState::Invalidated | StoredState::Revoked => {
                StoredState::Invalidated
            }
        };
        let new_state = record.display_state(OffsetDateTime::now_utc());
        self.save(&records)?;
        Ok(new_state)
    }

    /// CLI contract for `holler token ping <id>`: unused/stale/deleted
    /// tokens error; a bound token with no live connection fails; a
    /// bound token with a live connection reports hostname + RTT.
    pub fn ping(
        &self,
        token_id: &str,
        probe: &dyn ConnectionProbe,
    ) -> Result<PingOutcome, TokenError> {
        let records = self.load()?;
        let record = records
            .iter()
            .find(|r| r.token_id == token_id)
            .ok_or_else(|| TokenError::NotFound(token_id.to_string()))?;

        match record.state {
            StoredState::Bound => match probe.probe(token_id) {
                ConnectionStatus::Connected { hostname, rtt_ms } => {
                    Ok(PingOutcome::Connected { hostname, rtt_ms })
                }
                ConnectionStatus::Disconnected => {
                    Err(TokenError::Disconnected(token_id.to_string()))
                }
            },
            StoredState::Unused | StoredState::Invalidated | StoredState::Revoked => {
                Err(TokenError::Unbound(token_id.to_string()))
            }
        }
    }
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// `n` bytes from the OS CSPRNG, hex-encoded.
fn random_hex(n: usize) -> Result<String, TokenError> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf)?;
    Ok(to_hex(&buf))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// HMAC-SHA-256(pepper, secret), hex-encoded (ADR 0010). Persisted in
/// place of the plaintext secret, which is never stored anywhere.
fn hash_secret(pepper: &[u8], secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(pepper).expect("HMAC-SHA-256 accepts a key of any length");
    mac.update(secret.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// Parse a `--ttl` value: `<n><s|m|h|d>`, e.g. `30m`, `24h`, `7d`.
pub fn parse_ttl(s: &str) -> Result<Duration, TokenError> {
    let s = s.trim();
    let invalid = || TokenError::InvalidTtl(s.to_string());
    if s.len() < 2 {
        return Err(invalid());
    }
    let (num_part, unit) = s.split_at(s.len() - 1);
    let n: i64 = num_part.parse().map_err(|_| invalid())?;
    if n < 0 {
        return Err(invalid());
    }
    let duration = match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        _ => return Err(invalid()),
    };
    Ok(duration)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PEPPER: &[u8] = b"test-pepper-do-not-use-in-prod";

    fn store_in(dir: &std::path::Path) -> TokenStore {
        TokenStore::at(dir.join(STATE_FILE))
    }

    /// This is the one test that touches the real `mint()` entry point
    /// and its env var — every other test calls `mint_with_pepper`
    /// directly so parallel tests never race on process-global env
    /// state. We only assert the var is unset in *our own* process
    /// view before checking the fail-closed path; we do not set it.
    #[test]
    fn mint_fails_closed_without_pepper() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        // Guard against test pollution from outside this process
        // (e.g. a developer's shell exporting it); this test requires
        // it to be genuinely absent to prove the fail-closed path.
        if std::env::var(PEPPER_ENV).is_ok() {
            panic!(
                "{PEPPER_ENV} is set in the test environment; unset it to run this test"
            );
        }

        let err = store.mint(None, DEFAULT_TTL).unwrap_err();
        assert!(matches!(err, TokenError::PepperMissing));
    }

    #[test]
    fn mint_produces_token_id_and_secret_and_persists_only_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        let result = store
            .mint_with_pepper(TEST_PEPPER, Some("laptop".into()), DEFAULT_TTL)
            .unwrap();

        assert!(result.token_id.starts_with("tok_"));
        assert!(result.secret.starts_with("hlr_"));

        // Only the hash lives on disk — the plaintext secret never
        // appears in the store file.
        let raw = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(!raw.contains(&result.secret));

        let records = store.load().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].token_id, result.token_id);
        assert_ne!(records[0].secret_hash, result.secret);
        assert_eq!(records[0].state, StoredState::Unused);
    }

    #[test]
    fn ttl_computes_a_correct_expires() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        let before = OffsetDateTime::now_utc();
        let result = store
            .mint_with_pepper(TEST_PEPPER, None, Duration::hours(1))
            .unwrap();
        let after = OffsetDateTime::now_utc();

        let expires = OffsetDateTime::parse(&result.expires, &Rfc3339).unwrap();
        assert!(expires >= before + Duration::hours(1));
        assert!(expires <= after + Duration::hours(1));
    }

    #[test]
    fn delete_on_unused_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store.mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL).unwrap();

        let new_state = store.delete(&minted.token_id).unwrap();
        assert_eq!(new_state, DisplayState::Invalidated);

        let views = store.list().unwrap();
        assert_eq!(views[0].state, "invalidated");
    }

    #[test]
    fn delete_on_bound_revokes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store.mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL).unwrap();

        // No redeem flow exists yet (#30); force the record to `Bound`
        // directly to exercise the revoke transition.
        let mut records = store.load().unwrap();
        records[0].state = StoredState::Bound;
        records[0].machine = Some("build-box".to_string());
        store.save(&records).unwrap();

        let new_state = store.delete(&minted.token_id).unwrap();
        assert_eq!(new_state, DisplayState::Revoked);
        assert_eq!(store.load().unwrap()[0].state, StoredState::Revoked);
    }

    #[test]
    fn delete_unknown_token_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let err = store.delete("tok_doesnotexist").unwrap_err();
        assert!(matches!(err, TokenError::NotFound(_)));
    }

    #[test]
    fn list_never_exposes_the_secret_in_any_form() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, Some("phone".into()), DEFAULT_TTL)
            .unwrap();

        let views = store.list().unwrap();
        let json = serde_json::to_string(&views).unwrap();

        assert!(!json.contains(&minted.secret));
        // Nor the raw secret's random suffix in isolation, nor a
        // `secret_hash` field at all — `TokenView` has no such field.
        assert!(!json.contains("secret_hash"));
        assert!(!json.contains("secret"));
    }

    #[test]
    fn list_shows_stale_once_ttl_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, Duration::seconds(-1))
            .unwrap();
        let _ = minted;

        let views = store.list().unwrap();
        assert_eq!(views[0].state, "stale");
    }

    #[test]
    fn ping_unused_token_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store.mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL).unwrap();

        let err = store.ping(&minted.token_id, &AlwaysDisconnected).unwrap_err();
        assert!(matches!(err, TokenError::Unbound(_)));
    }

    #[test]
    fn ping_unknown_token_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let err = store.ping("tok_nope", &AlwaysDisconnected).unwrap_err();
        assert!(matches!(err, TokenError::NotFound(_)));
    }

    #[test]
    fn ping_bound_but_disconnected_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store.mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL).unwrap();

        let mut records = store.load().unwrap();
        records[0].state = StoredState::Bound;
        store.save(&records).unwrap();

        let err = store.ping(&minted.token_id, &AlwaysDisconnected).unwrap_err();
        assert!(matches!(err, TokenError::Disconnected(_)));
    }

    struct AlwaysConnected;
    impl ConnectionProbe for AlwaysConnected {
        fn probe(&self, _token_id: &str) -> ConnectionStatus {
            ConnectionStatus::Connected {
                hostname: "kiwi".to_string(),
                rtt_ms: 12,
            }
        }
    }

    #[test]
    fn ping_bound_and_connected_reports_hostname_and_rtt() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store.mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL).unwrap();

        let mut records = store.load().unwrap();
        records[0].state = StoredState::Bound;
        store.save(&records).unwrap();

        let outcome = store.ping(&minted.token_id, &AlwaysConnected).unwrap();
        match outcome {
            PingOutcome::Connected { hostname, rtt_ms } => {
                assert_eq!(hostname, "kiwi");
                assert_eq!(rtt_ms, 12);
            }
        }
    }

    #[test]
    fn ttl_parser_accepts_expected_suffixes() {
        assert_eq!(parse_ttl("30m").unwrap(), Duration::minutes(30));
        assert_eq!(parse_ttl("24h").unwrap(), Duration::hours(24));
        assert_eq!(parse_ttl("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_ttl("90s").unwrap(), Duration::seconds(90));
        assert!(parse_ttl("24x").is_err());
        assert!(parse_ttl("").is_err());
    }
}
