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
//! (default `./holler-server-state`) holding `tokens.json`. Every
//! mutating operation (`mint`/`delete`/`redeem`/`verify_credential`)
//! holds an advisory whole-file lock (`flock`/`LockFileEx`, via `fs4`)
//! on a sibling `tokens.json.lock` for the duration of its
//! load-modify-save cycle (issue #89), so two processes — e.g. a `holler
//! serve` and a concurrent one-shot CLI invocation, or two CLI
//! invocations — racing the same `HOLLER_STATE_DIR` can't clobber each
//! other's write.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use fs4::FileExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

type HmacSha256 = Hmac<Sha256>;

const STATE_DIR_ENV: &str = "HOLLER_STATE_DIR";
const DEFAULT_STATE_DIR: &str = "./holler-server-state";
const STATE_FILE: &str = "tokens.json";
const LOCK_FILE: &str = "tokens.json.lock";
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
    /// `redeem` presented a secret that does not match the stored hash.
    /// Fails closed: no state mutation happens on this path.
    WrongSecret(String),
    /// `verify_credential` (issue #31: WebSocket `auth`) presented a
    /// credential that does not match the stored hash. Fails closed: no
    /// state mutation happens on this path.
    WrongCredential(String),
    /// `redeem` on a token that was already redeemed once (secrets are
    /// single-use; same `token_id`, but a second `join` needs a new mint).
    AlreadyBound(String),
    /// `redeem` on a token that was invalidated (deleted while unused).
    Invalidated(String),
    /// `redeem` on a token that was revoked (deleted while bound).
    Revoked(String),
    /// `redeem` on a token whose unused secret's TTL has elapsed.
    Stale(String),
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
            TokenError::Unbound(id) => {
                write!(f, "token {id} has not been redeemed; nothing to ping")
            }
            TokenError::Disconnected(id) => write!(f, "token {id} is bound but not connected"),
            TokenError::WrongSecret(id) => write!(f, "wrong secret for token {id}"),
            TokenError::WrongCredential(id) => write!(f, "wrong credential for token {id}"),
            TokenError::AlreadyBound(id) => {
                write!(f, "token {id} was already redeemed and is bound to a client")
            }
            TokenError::Invalidated(id) => {
                write!(f, "token {id} was invalidated and cannot be redeemed")
            }
            TokenError::Revoked(id) => write!(f, "token {id} was revoked and cannot be redeemed"),
            TokenError::Stale(id) => {
                write!(f, "token {id}'s secret has expired and cannot be redeemed")
            }
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
    /// OS hostname of the redeeming client. Filled at redeem; `None`
    /// until then.
    pub machine: Option<String>,
    /// Identifier for the redeeming client, distinct from `token_id`
    /// (the join token survives a `client_id` reissue if we ever add
    /// one; today it's minted once, at redeem). `None` until bound.
    pub client_id: Option<String>,
    /// HMAC-SHA-256(pepper, credential) — same discipline as
    /// `secret_hash`, never the plaintext. Cleared on revoke so a
    /// revoked record can never validate a credential again even by
    /// accident. `None` until bound.
    credential_hash: Option<String>,
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
    /// Shown once bound — `holler client list` needs something that
    /// identifies *the client*, not just the token that provisioned it.
    /// `"-"` until bound.
    pub client_id: String,
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
            client_id: record.client_id.clone().unwrap_or_else(|| "-".to_string()),
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

/// Result of a successful redeem: the client credential is returned
/// here and nowhere else — the store never holds it, only its hash
/// (mirrors [`MintResult`]'s one-time-shown-secret shape).
#[derive(Debug)]
pub struct RedeemResult {
    /// Unchanged — the same `token_id` the join token was minted with.
    pub token_id: String,
    /// New identifier for the redeeming client (prefixed `cli_`).
    pub client_id: String,
    /// The long-lived client credential (prefixed `hlr_live_`, per ADR
    /// 0010's naming for join secret vs. client credential). Shown
    /// once; refreshing it is a later story's concern, not this one's.
    pub credential: String,
}

/// Result of a successful [`TokenStore::verify_credential`]: identifies
/// which bound client just authenticated (issue #31 `auth`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClient {
    pub token_id: String,
    pub client_id: String,
    pub machine: String,
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
        let dir = state_dir();
        fs::create_dir_all(&dir)?;
        Ok(TokenStore {
            path: dir.join(STATE_FILE),
        })
    }

    /// The directory this store (and the issue #31 control-socket path,
    /// which lives alongside it) resolve to: `HOLLER_STATE_DIR`, default
    /// `./holler-server-state`. Exposed so `wire::control` can locate the
    /// same directory without duplicating the env var name.
    pub fn dir(&self) -> &std::path::Path {
        self.path
            .parent()
            .expect("store path is always `<dir>/tokens.json`")
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

    /// The sibling lock file [`TokenStore::with_lock`] holds for the
    /// duration of one mutating operation. Kept separate from
    /// `tokens.json` itself (rather than locking the data file directly)
    /// so `save`'s whole-file rewrite is never itself the thing holding
    /// or contending for the lock.
    fn lock_path(&self) -> PathBuf {
        self.dir().join(LOCK_FILE)
    }

    /// Run `f` (one full load-modify-save cycle) while holding an
    /// advisory exclusive lock on [`TokenStore::lock_path`] (issue #89):
    /// blocks — does not poll or time out — until the lock is free, which
    /// is fine here since every caller's critical section is small,
    /// synchronous file I/O with no risk of a long hold. Guards against
    /// two processes (a `holler serve` and a concurrent CLI invocation,
    /// or two CLI invocations) racing a `mint`/`delete`/`redeem`/
    /// `verify_credential` against the same `HOLLER_STATE_DIR` and
    /// losing one side's update to a last-write-wins clobber.
    fn with_lock<T>(&self, f: impl FnOnce() -> Result<T, TokenError>) -> Result<T, TokenError> {
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
        FileExt::lock(&lock_file)?;
        let result = f();
        // Best-effort: an `unlock` failure here does not un-do `f`'s
        // already-committed write, and the OS releases the lock anyway
        // once `lock_file` drops at the end of this scope.
        let _ = FileExt::unlock(&lock_file);
        result
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
            client_id: None,
            credential_hash: None,
            created_at,
            expires: expires.clone(),
            last_seen: None,
        };

        self.with_lock(move || {
            let mut records = self.load()?;
            records.push(record);
            self.save(&records)
        })?;

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
    /// lands with #31 (needs a live listener); this corrects the
    /// stored state and, for a revoke, clears `credential_hash` so the
    /// revoked record can never validate a credential again even by
    /// accident (the credential itself was never stored, only its
    /// hash, but a cleared hash removes even that comparison target).
    pub fn delete(&self, token_id: &str) -> Result<DisplayState, TokenError> {
        self.with_lock(|| {
            let mut records = self.load()?;
            let record = records
                .iter_mut()
                .find(|r| r.token_id == token_id)
                .ok_or_else(|| TokenError::NotFound(token_id.to_string()))?;

            record.state = match record.state {
                StoredState::Bound => {
                    record.credential_hash = None;
                    StoredState::Revoked
                }
                StoredState::Unused | StoredState::Invalidated | StoredState::Revoked => {
                    StoredState::Invalidated
                }
            };
            let new_state = record.display_state(OffsetDateTime::now_utc());
            self.save(&records)?;
            Ok(new_state)
        })
    }

    /// Redeem a join token: verify the presented secret, consume it
    /// (single-use), and bind the record to a newly minted client
    /// identity + credential. Same `token_id`; `state` becomes `Bound`.
    ///
    /// Fails closed the same way `mint` does if the pepper is unset,
    /// and fails closed on a wrong secret or on any state other than
    /// `Unused` — no partial mutation happens on any error path.
    pub fn redeem(
        &self,
        token_id: &str,
        secret: &str,
        machine: String,
    ) -> Result<RedeemResult, TokenError> {
        let pepper = std::env::var(PEPPER_ENV).map_err(|_| TokenError::PepperMissing)?;
        if pepper.is_empty() {
            return Err(TokenError::PepperMissing);
        }
        self.redeem_with_pepper(pepper.as_bytes(), token_id, secret, machine)
    }

    /// The pepper-taking core of [`TokenStore::redeem`] (see
    /// [`TokenStore::mint_with_pepper`] for why this split exists).
    fn redeem_with_pepper(
        &self,
        pepper: &[u8],
        token_id: &str,
        secret: &str,
        machine: String,
    ) -> Result<RedeemResult, TokenError> {
        self.with_lock(move || {
            let mut records = self.load()?;
            let now = OffsetDateTime::now_utc();
            let record = records
                .iter_mut()
                .find(|r| r.token_id == token_id)
                .ok_or_else(|| TokenError::NotFound(token_id.to_string()))?;

            match record.display_state(now) {
                DisplayState::Unused => {}
                DisplayState::Bound => return Err(TokenError::AlreadyBound(token_id.to_string())),
                DisplayState::Invalidated => {
                    return Err(TokenError::Invalidated(token_id.to_string()))
                }
                DisplayState::Revoked => return Err(TokenError::Revoked(token_id.to_string())),
                DisplayState::Stale => return Err(TokenError::Stale(token_id.to_string())),
            }

            if !verify_secret(pepper, secret, &record.secret_hash) {
                return Err(TokenError::WrongSecret(token_id.to_string()));
            }

            let client_id = format!("cli_{}", random_hex(16)?);
            let credential = format!("hlr_live_{}", random_hex(32)?);
            let credential_hash = hash_secret(pepper, &credential);

            record.state = StoredState::Bound;
            record.machine = Some(machine);
            record.client_id = Some(client_id.clone());
            record.credential_hash = Some(credential_hash);

            self.save(&records)?;

            Ok(RedeemResult {
                token_id: token_id.to_string(),
                client_id,
                credential,
            })
        })
    }

    /// Verify a presented client credential (issue #31 `auth`) against
    /// the stored hash for `token_id`, without re-minting or re-redeeming
    /// anything (redeem is the one-time #30 operation; this is the
    /// read-mostly check a live WebSocket connection performs on every
    /// `auth`, including a reconnect). On success, records `last_seen`
    /// (the only mutation) and returns the client identity to bind the
    /// connection to.
    ///
    /// Fails closed the same way `redeem` does: unknown token is
    /// `NotFound`; not yet bound is `Unbound`; invalidated/revoked
    /// records stay rejected; a mismatched credential is
    /// `WrongCredential`. No mutation happens on any error path.
    pub fn verify_credential(
        &self,
        token_id: &str,
        credential: &str,
    ) -> Result<VerifiedClient, TokenError> {
        let pepper = std::env::var(PEPPER_ENV).map_err(|_| TokenError::PepperMissing)?;
        if pepper.is_empty() {
            return Err(TokenError::PepperMissing);
        }
        self.verify_credential_with_pepper(pepper.as_bytes(), token_id, credential)
    }

    /// The pepper-taking core of [`TokenStore::verify_credential`] (see
    /// [`TokenStore::mint_with_pepper`] for why this split exists).
    fn verify_credential_with_pepper(
        &self,
        pepper: &[u8],
        token_id: &str,
        credential: &str,
    ) -> Result<VerifiedClient, TokenError> {
        self.with_lock(|| {
            let mut records = self.load()?;
            let record = records
                .iter_mut()
                .find(|r| r.token_id == token_id)
                .ok_or_else(|| TokenError::NotFound(token_id.to_string()))?;

            match record.state {
                StoredState::Bound => {}
                StoredState::Unused => return Err(TokenError::Unbound(token_id.to_string())),
                StoredState::Invalidated => {
                    return Err(TokenError::Invalidated(token_id.to_string()))
                }
                StoredState::Revoked => return Err(TokenError::Revoked(token_id.to_string())),
            }

            // A `Bound` record always carries a `credential_hash` (set at
            // redeem, only ever cleared by a revoke — which already
            // returned above). Treat a missing hash the same as a wrong
            // credential rather than panicking: fail closed on any
            // inconsistency.
            let matches = record
                .credential_hash
                .as_deref()
                .is_some_and(|hash| verify_secret(pepper, credential, hash));
            if !matches {
                return Err(TokenError::WrongCredential(token_id.to_string()));
            }

            let now = OffsetDateTime::now_utc();
            record.last_seen = Some(now.format(&Rfc3339)?);
            let verified = VerifiedClient {
                token_id: token_id.to_string(),
                client_id: record.client_id.clone().unwrap_or_default(),
                machine: record.machine.clone().unwrap_or_default(),
            };

            self.save(&records)?;
            Ok(verified)
        })
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

/// The state directory `TokenStore::open` resolves to: `HOLLER_STATE_DIR`
/// env, default `./holler-server-state`. A free function (not a method)
/// so callers that need the directory before/without opening a store
/// (issue #31's control-socket path) can resolve it identically.
fn state_dir() -> PathBuf {
    let dir = std::env::var(STATE_DIR_ENV).unwrap_or_else(|_| DEFAULT_STATE_DIR.to_string());
    PathBuf::from(dir)
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

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// HMAC-SHA-256(pepper, secret), hex-encoded (ADR 0010). Persisted in
/// place of the plaintext secret, which is never stored anywhere.
fn hash_secret(pepper: &[u8], secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(pepper).expect("HMAC-SHA-256 accepts a key of any length");
    mac.update(secret.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// Verify `secret` against a stored HMAC hex digest in **constant
/// time** (ADR 0010: "compare HMAC in constant time") via
/// [`Mac::verify_slice`] rather than comparing hex strings.
fn verify_secret(pepper: &[u8], secret: &str, expected_hash_hex: &str) -> bool {
    let expected = match from_hex(expected_hash_hex) {
        Some(bytes) => bytes,
        None => return false,
    };
    let mut mac =
        HmacSha256::new_from_slice(pepper).expect("HMAC-SHA-256 accepts a key of any length");
    mac.update(secret.as_bytes());
    mac.verify_slice(&expected).is_ok()
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

    // -------------------------------------------------------------
    // Issue #89: advisory locking around the load-modify-save cycle.
    // -------------------------------------------------------------

    #[test]
    fn with_lock_holds_an_exclusive_lock_for_its_duration_and_releases_it_after() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());

        let mut held_during_critical_section = None;
        store
            .with_lock(|| {
                // A second, independent handle on the same lock file
                // cannot take it while this closure (the critical
                // section `with_lock` protects) is still running.
                let probe = fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(store.lock_path())
                    .unwrap();
                held_during_critical_section = Some(FileExt::try_lock(&probe).is_err());
                Ok::<(), TokenError>(())
            })
            .unwrap();
        assert_eq!(
            held_during_critical_section,
            Some(true),
            "the lock must be held for the duration of `with_lock`'s critical section"
        );

        // Once `with_lock` has returned, a fresh attempt succeeds — the
        // lock was actually released, not leaked past the call.
        let probe = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(store.lock_path())
            .unwrap();
        assert!(
            FileExt::try_lock(&probe).is_ok(),
            "the lock must be released once `with_lock` returns"
        );
    }

    #[test]
    fn concurrent_mints_from_two_threads_do_not_lose_a_write() {
        // Every thread appends one record via `mint_with_pepper`'s
        // load-modify-save cycle. Without the lock, two threads racing
        // that cycle can both load the same on-disk snapshot, each
        // append their own record to their own in-memory copy, and then
        // the second `save` clobbers the first — losing a mint outright.
        // With the lock serializing the cycle, all of them must land.
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(store_in(dir.path()));

        const THREADS: usize = 8;
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let records = store.load().unwrap();
        assert_eq!(
            records.len(),
            THREADS,
            "a lost update would show up as fewer than {THREADS} records: {records:?}"
        );
        // Every mint actually got its own distinct token_id — not one
        // thread's write silently overwriting another's in place.
        let unique: std::collections::HashSet<_> = records.iter().map(|r| &r.token_id).collect();
        assert_eq!(unique.len(), THREADS);
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
            panic!("{PEPPER_ENV} is set in the test environment; unset it to run this test");
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
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

        let new_state = store.delete(&minted.token_id).unwrap();
        assert_eq!(new_state, DisplayState::Invalidated);

        let views = store.list().unwrap();
        assert_eq!(views[0].state, "invalidated");
    }

    #[test]
    fn delete_on_bound_revokes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

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
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

        let err = store
            .ping(&minted.token_id, &AlwaysDisconnected)
            .unwrap_err();
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
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

        let mut records = store.load().unwrap();
        records[0].state = StoredState::Bound;
        store.save(&records).unwrap();

        let err = store
            .ping(&minted.token_id, &AlwaysDisconnected)
            .unwrap_err();
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
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

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
    fn redeem_transitions_unused_to_bound_and_issues_client_id_and_credential() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

        let result = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap();

        assert_eq!(result.token_id, minted.token_id);
        assert!(result.client_id.starts_with("cli_"));
        assert!(result.credential.starts_with("hlr_live_"));

        // Only the hash lives on disk — the plaintext credential never
        // appears in the store file (same discipline as the join secret).
        let raw = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(!raw.contains(&result.credential));

        let records = store.load().unwrap();
        assert_eq!(records[0].state, StoredState::Bound);
        assert_eq!(records[0].machine.as_deref(), Some("kiwi.local"));
        assert_eq!(records[0].client_id.as_deref(), Some(result.client_id.as_str()));

        let views = store.list().unwrap();
        assert_eq!(views[0].state, "bound");
        assert_eq!(views[0].machine, "kiwi.local");
        assert_eq!(views[0].client_id, result.client_id);
    }

    #[test]
    fn redeem_with_wrong_secret_fails_closed_without_mutating_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

        let err = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                "hlr_not-the-real-secret",
                "kiwi.local".to_string(),
            )
            .unwrap_err();
        assert!(matches!(err, TokenError::WrongSecret(_)));

        let records = store.load().unwrap();
        assert_eq!(records[0].state, StoredState::Unused);
        assert_eq!(records[0].machine, None);
        assert_eq!(records[0].client_id, None);
    }

    #[test]
    fn redeem_unknown_token_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let err = store
            .redeem_with_pepper(TEST_PEPPER, "tok_nope", "hlr_x", "kiwi.local".to_string())
            .unwrap_err();
        assert!(matches!(err, TokenError::NotFound(_)));
    }

    #[test]
    fn redeem_already_bound_token_fails_with_already_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();
        store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "first.local".to_string(),
            )
            .unwrap();

        let err = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "second.local".to_string(),
            )
            .unwrap_err();
        assert!(matches!(err, TokenError::AlreadyBound(_)));

        // The original bind is untouched by the failed second redeem.
        let records = store.load().unwrap();
        assert_eq!(records[0].machine.as_deref(), Some("first.local"));
    }

    #[test]
    fn redeem_invalidated_token_fails_with_invalidated() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();
        store.delete(&minted.token_id).unwrap();

        let err = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap_err();
        assert!(matches!(err, TokenError::Invalidated(_)));
    }

    #[test]
    fn redeem_revoked_token_fails_with_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();
        store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap();
        store.delete(&minted.token_id).unwrap();

        let err = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap_err();
        assert!(matches!(err, TokenError::Revoked(_)));
    }

    #[test]
    fn redeem_stale_token_fails_with_stale() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, Duration::seconds(-1))
            .unwrap();

        let err = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap_err();
        assert!(matches!(err, TokenError::Stale(_)));
    }

    #[test]
    fn delete_on_bound_clears_the_credential_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();
        store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap();

        store.delete(&minted.token_id).unwrap();

        let records = store.load().unwrap();
        assert_eq!(records[0].state, StoredState::Revoked);
        assert_eq!(records[0].credential_hash, None);
    }

    #[test]
    fn verify_credential_accepts_the_real_credential_and_updates_last_seen() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();
        let redeemed = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap();

        assert_eq!(store.load().unwrap()[0].last_seen, None);

        let verified = store
            .verify_credential_with_pepper(TEST_PEPPER, &minted.token_id, &redeemed.credential)
            .unwrap();
        assert_eq!(verified.token_id, minted.token_id);
        assert_eq!(verified.client_id, redeemed.client_id);
        assert_eq!(verified.machine, "kiwi.local");

        assert!(store.load().unwrap()[0].last_seen.is_some());
    }

    #[test]
    fn verify_credential_rejects_wrong_credential_without_mutating_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();
        store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap();

        let err = store
            .verify_credential_with_pepper(TEST_PEPPER, &minted.token_id, "hlr_live_not-it")
            .unwrap_err();
        assert!(matches!(err, TokenError::WrongCredential(_)));
        assert_eq!(store.load().unwrap()[0].last_seen, None);
    }

    #[test]
    fn verify_credential_on_unbound_token_is_unbound() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();

        let err = store
            .verify_credential_with_pepper(TEST_PEPPER, &minted.token_id, "hlr_live_whatever")
            .unwrap_err();
        assert!(matches!(err, TokenError::Unbound(_)));
    }

    #[test]
    fn verify_credential_unknown_token_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let err = store
            .verify_credential_with_pepper(TEST_PEPPER, "tok_nope", "hlr_live_whatever")
            .unwrap_err();
        assert!(matches!(err, TokenError::NotFound(_)));
    }

    #[test]
    fn verify_credential_on_revoked_token_is_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let minted = store
            .mint_with_pepper(TEST_PEPPER, None, DEFAULT_TTL)
            .unwrap();
        let redeemed = store
            .redeem_with_pepper(
                TEST_PEPPER,
                &minted.token_id,
                &minted.secret,
                "kiwi.local".to_string(),
            )
            .unwrap();
        store.delete(&minted.token_id).unwrap();

        let err = store
            .verify_credential_with_pepper(TEST_PEPPER, &minted.token_id, &redeemed.credential)
            .unwrap_err();
        assert!(matches!(err, TokenError::Revoked(_)));
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
