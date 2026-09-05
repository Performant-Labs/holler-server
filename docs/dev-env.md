# Development environment

How to set up a machine to **work on** Holler (server + client), not how to run a production hub.

Holler is two processes and a CLI. There is no web UI. You do **not** need Playwright, Herdr on the client box, Tailscale, or IPv6.

Implementation language is not pinned yet. This page is the **runtime shape** the binaries will expect. When the first code lands ([#28](https://github.com/Performant-Labs/holler-server/issues/28) / test canary [#41](https://github.com/Performant-Labs/holler-server/issues/41)), add the toolchain (compiler, `make`/`go test`/…) in the section below.

## What you need

| | Required now | Later (when binaries exist) |
| --- | --- | --- |
| OS | Linux, macOS, or Windows | same |
| Git | yes | yes |
| Both repos | yes (docs + issues live in both) | yes |
| Compiler / test runner | — | whatever #28/#41 pick; **not** bash-only |
| OpenCode | no | only for the **acceptance gate**, not for the wire harness |
| Herdr | no | only on the hub, for the gate |
| Tailscale | no | optional underlay |
| IPv6 | no | optional (`::1` tests skip if missing) |

## Clone

Sibling checkouts so the wire harness can find the client stub:

```bash
mkdir -p ~/src/holler && cd ~/src/holler
git clone https://github.com/Performant-Labs/holler-server.git
git clone https://github.com/Performant-Labs/holler-client.git
```

Windows: same, any directory; use `127.0.0.1`, not `localhost`.

## Read first

| | |
| --- | --- |
| [How they talk](protocol/talk.md) | Two hops: Holler WebSocket, ACP stdio |
| [Testing](testing.md) | Harness, canary, not Playwright |
| [Protocol index](protocol/README.md) | Every protocol |
| [Issue #1](https://github.com/Performant-Labs/holler-server/issues/1) | Product brief |
| Builder order | [server epic #27](https://github.com/Performant-Labs/holler-server/issues/27), [client epic #22](https://github.com/Performant-Labs/holler-client/issues/22) |

## Once there is a `holler` binary

Two terminals on the **same machine**. Loopback `ws` is enough (ADR 0004 / 0010).

**Terminal A — server**

```bash
export HOLLER_DEBUG=quiet          # none | quiet | noisy; never logs secrets
# export HOLLER_PEPPER=…           # HMAC pepper; file mode 0600 also fine (ADR 0010)
holler --listen 127.0.0.1:41807    # default if omitted
holler token mint --label dev
```

Copy the join secret once. Do not paste it into debug logs.

**Terminal B — client**

```bash
export HOLLER_DEBUG=quiet
holler join --server ws://127.0.0.1:41807 --token '<join-secret>'
holler status
```

Omitted port is **41807**. IPv6: `--listen [::1]:41807` and `--server ws://[::1]:41807`. Do not use the name `localhost`.

**Prove the wire (stub, not OpenCode)** — [server #40](https://github.com/Performant-Labs/holler-server/issues/40), [client #32](https://github.com/Performant-Labs/holler-client/issues/32):

- Server runner: `tests/wire/`
- Client ACP stub: `holler-client/tests/stub-acp/` (`stub-acp.exe` on Windows)

**Fail-fast canary** (no `holler` binary required): [server #41](https://github.com/Performant-Labs/holler-server/issues/41) — `tests/wire/selftest/`. If that is green while the runner swallows failures, stop.

## Toolchain (fill in when code exists)

- Language / commands to build `holler` on Linux, macOS, Windows
- How CI runs #41 then `tests/wire/` on all three OSes
- Where the HMAC pepper lives in a dev checkout

Until then: edit docs and issues; do not assume a package manager.

## Don’t

- Bind `localhost` (often IPv6 on Windows while the default listen is IPv4)
- Put join secrets or credentials in log files or `--debug=noisy` output (redact)
- Use Playwright for e2e
- Run Herdr on the client box
- Treat Tailscale as Holler auth or encryption
