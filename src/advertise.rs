//! Persists what `holler serve` was told about its own reachable
//! address, so a separate `holler token mint` invocation can read it
//! back and print a ready-to-run `holler join` command (issue #66).
//!
//! `mint` and `serve` are separate process invocations with no shared
//! memory. Storage mirrors `token`'s own convention (a small JSON file
//! under `HOLLER_STATE_DIR`) rather than the issue #31 control socket,
//! because `mint` must be able to answer even after `serve` has exited —
//! the control socket only works while `serve` is still the live process
//! on the other end.

use std::fs;
use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

const ADVERTISE_FILE: &str = "advertise.json";

/// `Some(address)` when `serve` had something reachable to advertise;
/// `None` when it ran but was loopback-only. The file's mere existence
/// (regardless of which variant) is what tells `mint` a `serve` has run
/// at all in this `HOLLER_STATE_DIR` — see [`AdvertiseState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdvertiseRecord {
    address: Option<String>,
}

/// What `mint` finds on record for the server's advertise address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvertiseState {
    /// A reachable address is on record — safe to build a join command.
    Reachable(String),
    /// `serve` has run, but recorded no reachable address (loopback-only
    /// `--listen`, no `--advertise`).
    LoopbackOnly,
    /// No `serve` has ever recorded advertise state in this
    /// `HOLLER_STATE_DIR`.
    Unknown,
}

/// Whether an IP is usable as an advertised join address: not loopback,
/// and not the unspecified "all interfaces" address — nothing outside
/// this host could literally connect to `0.0.0.0` or `::`.
fn is_advertisable_ip(ip: &IpAddr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified()
}

/// Resolves what `serve` should persist as its advertise address.
///
/// An explicit `--advertise` wins outright and is trusted as given — an
/// operator naming a NAT/reverse-proxy/hostname address that this
/// process cannot itself verify is exactly the point of the flag.
/// Otherwise, the first `--listen` address that is neither loopback nor
/// unspecified is used. `None` means loopback-only (still a meaningful
/// answer for [`persist`] to record, distinct from "no `serve` has ever
/// run").
pub fn resolve_advertise(advertise_flag: Option<&str>, listen_addrs: &[SocketAddr]) -> Option<String> {
    if let Some(explicit) = advertise_flag {
        return Some(explicit.to_string());
    }
    listen_addrs
        .iter()
        .find(|a| is_advertisable_ip(&a.ip()))
        .map(SocketAddr::to_string)
}

/// Persist what [`resolve_advertise`] resolved to
/// `HOLLER_STATE_DIR/advertise.json`, so a later `mint` can read it back.
pub fn persist(state_dir: &Path, advertise: Option<&str>) -> io::Result<()> {
    let record = AdvertiseRecord {
        address: advertise.map(str::to_string),
    };
    let json = serde_json::to_string_pretty(&record)?;
    fs::write(state_dir.join(ADVERTISE_FILE), json)
}

/// Read back what a `serve` process (if any) persisted.
pub fn read(state_dir: &Path) -> AdvertiseState {
    let contents = match fs::read_to_string(state_dir.join(ADVERTISE_FILE)) {
        Ok(contents) => contents,
        Err(_) => return AdvertiseState::Unknown,
    };
    match serde_json::from_str::<AdvertiseRecord>(&contents) {
        Ok(AdvertiseRecord {
            address: Some(addr),
        }) => AdvertiseState::Reachable(addr),
        Ok(AdvertiseRecord { address: None }) => AdvertiseState::LoopbackOnly,
        Err(_) => AdvertiseState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_advertise_wins_over_listen() {
        let listen: Vec<SocketAddr> = vec!["10.0.0.5:41807".parse().unwrap()];
        assert_eq!(
            resolve_advertise(Some("myhost.example.com:41807"), &listen),
            Some("myhost.example.com:41807".to_string())
        );
    }

    #[test]
    fn non_loopback_listen_is_used_as_fallback() {
        let listen: Vec<SocketAddr> = vec!["10.0.0.5:41807".parse().unwrap()];
        assert_eq!(
            resolve_advertise(None, &listen),
            Some("10.0.0.5:41807".to_string())
        );
    }

    #[test]
    fn loopback_only_listen_resolves_to_none() {
        let listen: Vec<SocketAddr> = vec!["127.0.0.1:41807".parse().unwrap(), "[::1]:41807".parse().unwrap()];
        assert_eq!(resolve_advertise(None, &listen), None);
    }

    #[test]
    fn unspecified_listen_is_not_advertisable() {
        let listen: Vec<SocketAddr> = vec!["0.0.0.0:41807".parse().unwrap()];
        assert_eq!(resolve_advertise(None, &listen), None);
    }

    #[test]
    fn first_advertisable_listen_addr_wins_when_mixed() {
        let listen: Vec<SocketAddr> = vec![
            "127.0.0.1:41807".parse().unwrap(),
            "192.168.1.9:41807".parse().unwrap(),
        ];
        assert_eq!(
            resolve_advertise(None, &listen),
            Some("192.168.1.9:41807".to_string())
        );
    }

    #[test]
    fn persist_and_read_roundtrip_reachable() {
        let dir = tempfile::tempdir().unwrap();
        persist(dir.path(), Some("example.test:41807")).unwrap();
        assert_eq!(
            read(dir.path()),
            AdvertiseState::Reachable("example.test:41807".to_string())
        );
    }

    #[test]
    fn persist_and_read_roundtrip_loopback_only() {
        let dir = tempfile::tempdir().unwrap();
        persist(dir.path(), None).unwrap();
        assert_eq!(read(dir.path()), AdvertiseState::LoopbackOnly);
    }

    #[test]
    fn read_with_no_file_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), AdvertiseState::Unknown);
    }
}
