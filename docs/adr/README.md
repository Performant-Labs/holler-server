# ADRs

Architecture decision records for holler-server.

**Do not open a new GitHub issue for an ADR.** Reuse the reserved slot in [#2](https://github.com/Performant-Labs/holler-server/issues/2)–[#26](https://github.com/Performant-Labs/holler-server/issues/26): retitle, write the brief on the issue, keep the `adr` label.

| ADR | Issue | Title |
|-----|-------|--------|
| 0001 | #2 | Talk circuit, not a multiplexer |
| 0002 | #3 | Remote body is holler-client, not Herdr |
| 0003 | #4 | Join token, then a client credential |
| 0004 | #5 | WebSocket transport; listen port 41807 |
| 0005 | #6 | Interrupt is a control frame; the session survives |
| 0006 | #7 | Presence is status |
| 0007 | #8 | Address sessions, not hosts |
| 0008 | #9 | v1 body is OpenCode via ACP |
| 0009 | #10 | Protocol is Holler v1 |
| 0010 | #11 | Tokens hashed at rest; TLS on the wire |
| 0011 | #12 | New harnesses are config, not Holler releases |
| 0012 | #13 | Body protocol is ACP v1 (RFDs, not IETF RFCs) |
| 0013 | #14 | Rust, because we want the official ACP SDK |
| 0014 | #15 | Versioning: SemVer per crate, starting at 0.1.0, independent per repo |
| 0015–0025 | #16–#26 | reserved |

Issue #1 is the project brief, not an ADR.
