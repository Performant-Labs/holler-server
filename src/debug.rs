//! Debug levels for holler-server: none / quiet / noisy, with secret redaction.
//!
//! Same contract as holler-client's debug levels (issue #31), mirrored here for issue #38.
//! This module only provides the level type, its resolution logic, and redaction helpers —
//! no flag/env wiring and no frame-logging call sites exist yet; a later story wires those in.

use std::fmt;

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
}
