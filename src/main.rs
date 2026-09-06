//! `holler` CLI (issue #29: the first entry point in this crate).
//!
//! `token mint/list/delete/ping/redeem` and `client list/detach`
//! (aliases of the bound-token list/delete paths, issue #30) exist
//! today. `Cli`/`Commands` are shaped so a later story (#35, "Meta-O
//! CLI wiring") can add sibling top-level subcommands (`status`,
//! `support`, `roster`, `say`, ...) without restructuring this file.
//!
//! `token redeem` is a manual-testing convenience, not the production
//! path: real redemption is triggered by a live `holler-client` `join`
//! over the WebSocket listener (issue #31, not yet built), which calls
//! [`holler_server::token::TokenStore::redeem`] directly. This verb
//! exists only so an operator can exercise the same library call from
//! a shell without a real client.

use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use holler_server::advertise::{self, AdvertiseState};
use holler_server::token::{parse_ttl, PingOutcome, RedeemResult, TokenError, TokenStore, DEFAULT_TTL};
use holler_server::wire;
use holler_server::wire::control::RosterRowDoc;

/// `holler token list` / `holler client list` share this table shape;
/// `client list` just pre-filters to bound records.
fn print_table(views: &[holler_server::token::TokenView]) {
    const HEADERS: [&str; 7] = [
        "TOKEN_ID",
        "STATE",
        "CLIENT_ID",
        "MACHINE",
        "LABEL",
        "LAST_SEEN",
        "EXPIRES",
    ];
    let rows: Vec<[&str; 7]> = views
        .iter()
        .map(|v| {
            [
                v.token_id.as_str(),
                v.state.as_str(),
                v.client_id.as_str(),
                v.machine.as_str(),
                v.label.as_str(),
                v.last_seen.as_str(),
                v.expires.as_str(),
            ]
        })
        .collect();

    let mut widths = HEADERS.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let print_row = |cells: &[&str; 7]| {
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect();
        println!("{}", line.join("  ").trim_end());
    };

    print_row(&HEADERS);
    for row in &rows {
        print_row(row);
    }
}

#[derive(Parser)]
#[command(name = "holler", version, about = "Your agents are just a holler away")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage join tokens.
    Token(TokenArgs),
    /// Manage bound clients — aliases of the bound-token list/detach
    /// paths under `token`, scoped to records that are actually bound.
    Client(ClientArgs),
    /// Run the WebSocket listener (issue #31: "first talk"). Blocks
    /// until Ctrl-C.
    Serve(ServeArgs),
    /// This process's own status document (`role: server`), or (with an
    /// `id`) a live client's status, relayed over the wire (issue #37).
    /// Reaches a live `holler serve` process over the local control
    /// channel if one is running on this host; otherwise reports a
    /// local-only, not-connected document.
    Status {
        /// Token id, client id, or hostname of a connected client —
        /// relays `query status` to it instead of answering locally.
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// `holler support [<id>] <feature>`: do you (or a connected
    /// client) support this protocol feature or harness, right now.
    Support {
        /// `<feature>` (local) or `<id> <feature>` (relayed to that
        /// client). A single argument that is not a known feature or
        /// harness id is an error ("missing feature"), per spec §8.
        #[arg(num_args = 1..=2)]
        args: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// `holler caps [<id>]`: full capability document (status + a map
    /// of every known feature/harness → `{ok, kind, reason?}`).
    Caps {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// `holler query [<id>] <cmd> [args...]`: the general query form —
    /// `status` / `caps` / `support [feature]` / `protocol [version]`,
    /// local or (with a leading `<id>`) relayed to a connected client.
    Query {
        #[arg(num_args = 1..)]
        args: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Who can be hollered at: sessions advertised by `presence`,
    /// their harness, owning client, and connected/reconnecting/gone
    /// state (issue #32, ADR 0006). Reaches a live `holler serve`
    /// process over the local control channel; empty (not an error) if
    /// none is running, since the roster only exists in-memory.
    Roster {
        #[arg(long)]
        json: bool,
    },
    /// `holler say <session> <text>`: prompt a session by name (ADR
    /// 0007), routed by the live `holler serve` process to whichever
    /// connection currently hosts it (via the roster, issue #32), and
    /// print back its `reply` text once `done: true` arrives. Fails with
    /// `unknown_session` if the roster does not know `<session>`, or its
    /// owning connection is gone.
    Say { session: String, text: String },
    /// `holler interrupt <session>`: cancel a session's in-flight turn
    /// (ADR 0005) — a **control** frame, not a queued `prompt`: it reaches
    /// the session's connection immediately, even mid-turn, and the
    /// session stays on the roster afterward, promptable again. Routed by
    /// the roster the same way `say` is. Reports `unknown_session` if the
    /// roster does not know `<session>`; reports (non-zero exit) if the
    /// connection is gone, or if it is still alive but did not `ack` the
    /// cancel within the interrupt's own short timeout — "may not have
    /// landed," distinct from "not connected" (issue #54).
    Interrupt { session: String },
}

/// The four `query` `cmd`s the protocol defines (spec §7). `holler
/// query`'s general form uses this to tell a leading `<id>` apart from
/// the `cmd` itself: an id is never one of these words.
const KNOWN_QUERY_CMDS: &[&str] = &["status", "caps", "support", "protocol"];

#[derive(clap::Args)]
struct ServeArgs {
    /// `[host:]port`, e.g. `127.0.0.1:41807` or `[::1]:41807`.
    /// Repeatable, to serve more than one address family (ADR 0004).
    /// Defaults to `HOLLER_LISTEN`, then `127.0.0.1:41807`.
    #[arg(long)]
    listen: Vec<String>,
    /// `host[:port]` naming how others should reach this server (issue
    /// #66), e.g. `myhost.example.com:41807`. Persisted so a later
    /// `holler token mint` can print a ready-to-run `holler join`
    /// command. If omitted, falls back to `--listen` when that is
    /// already a real, non-loopback address; otherwise `mint` warns
    /// instead of guessing.
    #[arg(long)]
    advertise: Option<String>,
}

#[derive(clap::Args)]
struct TokenArgs {
    #[command(subcommand)]
    command: TokenCommands,
}

#[derive(Subcommand)]
enum TokenCommands {
    /// Mint a new join token; prints the token_id and secret once.
    #[command(alias = "create")]
    Mint {
        #[arg(long)]
        label: Option<String>,
        /// e.g. 30m, 24h, 7d. Defaults to 24h.
        #[arg(long)]
        ttl: Option<String>,
    },
    /// List unused and bound tokens. Never prints the secret.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Invalidate an unused token, or revoke a bound one.
    #[command(alias = "rm", alias = "remove")]
    Delete { id: String },
    /// Check whether a bound token's client is connected.
    Ping { id: String },
    /// Redeem a join token's secret: bind it to `machine` and mint a
    /// client_id + credential. Manual-testing convenience — the
    /// production path is a live client's `join` (issue #31).
    Redeem {
        id: String,
        #[arg(long)]
        secret: String,
        #[arg(long)]
        machine: String,
    },
}

#[derive(clap::Args)]
struct ClientArgs {
    #[command(subcommand)]
    command: ClientCommands,
}

#[derive(Subcommand)]
enum ClientCommands {
    /// List bound clients (alias of `token list`, filtered to `bound`).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Detach (revoke) a bound client (alias of `token delete`).
    #[command(alias = "rm", alias = "remove")]
    Detach { id: String },
}

fn main() {
    let cli = Cli::parse();
    let result: Result<(), Box<dyn std::error::Error>> = match cli.command {
        Commands::Token(args) => run_token(args.command).map_err(Into::into),
        Commands::Client(args) => run_client(args.command).map_err(Into::into),
        Commands::Serve(args) => run_serve(args),
        Commands::Status { id, json } => run_status(id, json),
        Commands::Support { args, json } => run_support(args, json),
        Commands::Caps { id, json } => run_caps(id, json),
        Commands::Query { args, json } => run_query(args, json),
        Commands::Roster { json } => run_roster(json),
        Commands::Say { session, text } => run_say(session, text),
        Commands::Interrupt { session } => run_interrupt(session),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Resolve the addresses `holler serve` binds: `--listen` (repeatable)
/// wins outright over `HOLLER_LISTEN` (comma-separated, for the same
/// two-families case as a repeated flag); neither present defaults to
/// `127.0.0.1:41807` (ADR 0004).
fn resolve_listen_addrs(flag_values: &[String]) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error>> {
    let specs: Vec<String> = if !flag_values.is_empty() {
        flag_values.to_vec()
    } else if let Ok(env_val) = std::env::var("HOLLER_LISTEN") {
        env_val
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec!["127.0.0.1:41807".to_string()]
    };
    specs
        .iter()
        .map(|s| wire::parse_listen_spec(s).map_err(|e| e.into()))
        .collect()
}

fn run_serve(args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let listen_addrs = resolve_listen_addrs(&args.listen)?;
    let debug = holler_server::debug::DebugLevel::resolve(None, std::env::var("HOLLER_DEBUG").ok().as_deref())?;

    // Persist what this run resolved to advertise (issue #66) before
    // accepting any connections, so a `mint` invocation racing right
    // after `serve` starts always sees it.
    let resolved_advertise = advertise::resolve_advertise(args.advertise.as_deref(), &listen_addrs);
    let store = TokenStore::open()?;
    advertise::persist(store.dir(), resolved_advertise.as_deref())?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let config = wire::ServeConfig { listen_addrs, debug };
        let handle = wire::serve(config).await?;
        let addrs: Vec<String> = handle.addrs.iter().map(|a| format!("ws://{a}")).collect();
        println!("holler-server listening on: {}", addrs.join(", "));

        tokio::signal::ctrl_c().await?;
        println!("holler-server: shutting down");
        handle.shutdown().await;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}

fn run_status(id: Option<String>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    run_query_command("status", id, Vec::new(), json)
}

fn run_support(mut args: Vec<String>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // clap's `num_args = 1..=2` guarantees 1 or 2 elements.
    let (target, feature) = if args.len() == 2 {
        (Some(args.remove(0)), args.remove(0))
    } else {
        (None, args.remove(0))
    };
    run_query_command("support", target, vec![feature], json)
}

fn run_caps(id: Option<String>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    run_query_command("caps", id, Vec::new(), json)
}

fn run_query(args: Vec<String>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // `holler query <cmd> [args...]` vs `holler query <id> <cmd> [args...]`:
    // a leading word that is not one of the four known `cmd`s, with a
    // second word to be the `cmd`, is a target id (spec §8's
    // disambiguation for `support`, generalized). A single unrecognized
    // word is always treated as a local `cmd` — it fails closed via
    // `query::dispatch`'s own `unknown_cmd`, not a CLI-level "missing
    // cmd" error, matching ADR 0009 (never invent, but still dispatch).
    let (target, cmd, rest) = if args.len() >= 2 && !KNOWN_QUERY_CMDS.contains(&args[0].as_str()) {
        (Some(args[0].clone()), args[1].clone(), args[2..].to_vec())
    } else {
        (None, args[0].clone(), args[1..].to_vec())
    };
    run_query_command(&cmd, target, rest, json)
}

/// Shared path for `status`/`support`/`caps`/`query`: reach a live
/// `holler serve` process over the control channel (issue #37); for an
/// untargeted query with no live server, fall back to this binary's own
/// compile-time view (zero clients, no confirmed harnesses — the same
/// "not running" answer `holler status` has always given). A targeted
/// query has no local fallback — resolving `<id>` requires a live
/// registry, so no live server means the target is simply unreachable.
fn run_query_command(
    cmd: &str,
    target: Option<String>,
    args: Vec<String>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = TokenStore::open()?;
    let state_dir = store.dir().to_path_buf();

    let reply = wire::control::run_query(&state_dir, target.clone(), cmd, args.clone());
    let body = match reply {
        Some(wire::control::QueryReply::Ok { query_ok }) => query_ok,
        Some(wire::control::QueryReply::Err { error }) => {
            return Err(format!("{}: {}", error.code, error.message.unwrap_or_default()).into());
        }
        Some(wire::control::QueryReply::NotConnected) if target.is_some() => {
            return Err(format!(
                "no live connection matches {:?}",
                target.expect("target.is_some() was just checked")
            )
            .into());
        }
        _ if target.is_some() => {
            return Err("no live `holler serve` process is reachable on this host".into());
        }
        _ => {
            // Untargeted, no live server: this binary's own local view.
            let hostname = wire::local_hostname()?;
            match wire::query::dispatch(cmd, &args, &hostname, &[], 0, 0, &[]) {
                Ok(body) => body,
                Err(err) => {
                    return Err(
                        format!("{}: {}", err.code, err.message.unwrap_or_default()).into(),
                    );
                }
            }
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&body.rest)?);
    } else if body.cmd == "status" || body.cmd == "caps" {
        print_status_like(&body);
    } else {
        print_generic(&body);
    }
    Ok(())
}

fn print_roster_table(rows: &[RosterRowDoc]) {
    const HEADERS: [&str; 5] = ["SESSION", "HARNESS", "CLIENT_ID", "STATE", "LAST_SEEN"];
    let last_seen_cells: Vec<String> = rows
        .iter()
        .map(|r| format!("{}s ago", r.last_seen_ms / 1000))
        .collect();
    let rows_cells: Vec<[&str; 5]> = rows
        .iter()
        .zip(last_seen_cells.iter())
        .map(|(r, last_seen)| {
            [
                r.name.as_str(),
                r.harness.as_str(),
                r.client_id.as_str(),
                r.state.as_str(),
                last_seen.as_str(),
            ]
        })
        .collect();

    let mut widths = HEADERS.map(str::len);
    for row in &rows_cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let print_row = |cells: &[&str; 5]| {
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect();
        println!("{}", line.join("  ").trim_end());
    };

    print_row(&HEADERS);
    for row in &rows_cells {
        print_row(row);
    }
}

fn run_roster(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = TokenStore::open()?;
    let rows = wire::control::query_roster(store.dir());
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_roster_table(&rows);
    }
    Ok(())
}

/// `holler say <session> <text>` (issue #33): reach a live `holler serve`
/// process over the control channel and route a `prompt` to whichever
/// connection the roster says hosts `<session>`. No local fallback —
/// there is no roster to consult without a live server.
fn run_say(session: String, text: String) -> Result<(), Box<dyn std::error::Error>> {
    let store = TokenStore::open()?;
    let state_dir = store.dir().to_path_buf();
    match wire::control::run_say(&state_dir, session, text) {
        Some(wire::control::SayReply::Ok { text, exit }) => {
            println!("{text}");
            if let Some(code) = exit {
                if code != 0 {
                    std::process::exit(code.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
                }
            }
            Ok(())
        }
        Some(wire::control::SayReply::Interrupted) => {
            Err("prompt was interrupted before it completed".into())
        }
        Some(wire::control::SayReply::Err { error }) => {
            Err(format!("{}: {}", error.code, error.message.unwrap_or_default()).into())
        }
        None => Err("no live `holler serve` process is reachable on this host".into()),
    }
}

/// `holler interrupt <session>` (issue #34, ADR 0005): reach a live
/// `holler serve` process over the control channel and send a control-
/// frame `interrupt` to whichever connection the roster says hosts
/// `<session>`. No local fallback — there is no roster to consult without
/// a live server. `TimedOut` and `Disconnected` are reported with
/// distinct messages (issue #54): a live-but-silent connection ("may not
/// have landed") is not the same operator-facing fact as no connection at
/// all.
fn run_interrupt(session: String) -> Result<(), Box<dyn std::error::Error>> {
    let store = TokenStore::open()?;
    let state_dir = store.dir().to_path_buf();
    match wire::control::run_interrupt(&state_dir, session.clone()) {
        Some(wire::control::InterruptReply::Ok) => {
            println!("interrupted {session}");
            Ok(())
        }
        Some(wire::control::InterruptReply::TimedOut) => Err(format!(
            "interrupt sent to {session:?}, but no ack arrived in time — \
             the connection is alive but the cancel may not have landed"
        )
        .into()),
        Some(wire::control::InterruptReply::Disconnected) => {
            Err(format!("session {session:?}'s connection is gone").into())
        }
        Some(wire::control::InterruptReply::Err { error }) => {
            Err(format!("{}: {}", error.code, error.message.unwrap_or_default()).into())
        }
        None => Err("no live `holler serve` process is reachable on this host".into()),
    }
}

/// `holler token delete` / `client detach` (issue #78): after the durable
/// on-disk revoke (`store.delete`, already done by the time this is
/// called) succeeds, best-effort-notify a live `holler serve` process to
/// also force-close the token's live connection, so `holler
/// status`/`roster` stop reporting it connected. No live server reachable
/// is not a failure — the on-disk revoke is what mattered, and the CLI
/// still reports success either way (same tolerance `run_say`/
/// `run_interrupt` have for "no live server").
fn notify_live_revoke(state_dir: &std::path::Path, id: &str) {
    let _ = wire::control::run_revoke(state_dir, id.to_string());
}

/// Pretty-print a `status`/`caps` `query_ok` body the way `holler
/// status` has always looked; `caps` additionally lists its
/// `capabilities` map.
fn print_status_like(body: &holler_server::proto::QueryOkBody) {
    use serde_json::Value;
    let rest = &body.rest;
    let get_str = |k: &str| rest.get(k).and_then(Value::as_str).unwrap_or("?");
    let listening = rest.get("listening").and_then(Value::as_array);
    let listening_str = match listening {
        Some(l) if !l.is_empty() => l
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => "not running".to_string(),
    };

    println!("role:      {}", get_str("role"));
    println!("hostname:  {}", get_str("hostname"));
    println!(
        "protocol:  {} (min {} max {})",
        rest.get("protocol").and_then(Value::as_u64).unwrap_or(0),
        rest.get("protocol_min").and_then(Value::as_u64).unwrap_or(0),
        rest.get("protocol_max").and_then(Value::as_u64).unwrap_or(0)
    );
    println!("listening: {listening_str}");
    println!(
        "clients:   {}",
        rest.get("clients").and_then(Value::as_u64).unwrap_or(0)
    );
    println!(
        "status:    {}",
        if listening.is_some_and(|l| !l.is_empty()) {
            "healthy"
        } else {
            "not running"
        }
    );

    if let Some(capabilities) = rest.get("capabilities").and_then(Value::as_object) {
        println!("capabilities:");
        let mut ids: Vec<&String> = capabilities.keys().collect();
        ids.sort();
        for id in ids {
            let ok = capabilities[id]
                .get("ok")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            println!("  {id}: {}", if ok { "ok" } else { "no" });
        }
    }
}

/// Pretty-print any other `query_ok` body (`support`, `protocol`) as
/// `field: value` lines, sorted for stable output.
fn print_generic(body: &holler_server::proto::QueryOkBody) {
    println!("cmd: {}", body.cmd);
    if let Some(map) = body.rest.as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            println!("{key}: {}", map[key]);
        }
    }
}

fn run_token(command: TokenCommands) -> Result<(), TokenError> {
    let store = TokenStore::open()?;
    match command {
        TokenCommands::Mint { label, ttl } => {
            let ttl = match ttl {
                Some(s) => parse_ttl(&s)?,
                None => DEFAULT_TTL,
            };
            let minted = store.mint(label, ttl)?;
            println!("token_id: {}", minted.token_id);
            println!("secret:   {}", minted.secret);
            println!("expires:  {}", minted.expires);
            println!("\nThis secret is shown once and is not stored. Save it now.");
            print_join_command(store.dir(), &minted.token_id, &minted.secret);
            Ok(())
        }
        TokenCommands::List { json } => {
            let views = store.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&views)?);
            } else {
                print_table(&views);
            }
            Ok(())
        }
        TokenCommands::Delete { id } => {
            let new_state = store.delete(&id)?;
            notify_live_revoke(store.dir(), &id);
            println!("token {id} is now {new_state}");
            Ok(())
        }
        TokenCommands::Ping { id } => {
            let probe = wire::control::LiveProbe::new(store.dir().to_path_buf());
            match store.ping(&id, &probe) {
                Ok(PingOutcome::Connected { hostname, rtt_ms }) => {
                    println!("{hostname} rtt={rtt_ms}ms");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        TokenCommands::Redeem {
            id,
            secret,
            machine,
        } => {
            let redeemed = store.redeem(&id, &secret, machine)?;
            print_redeem_result(&redeemed);
            Ok(())
        }
    }
}

/// Prints a ready-to-run `holler join` command for a freshly minted
/// token (issue #66), using whatever `serve` recorded as its reachable
/// address — or a clear warning in place of a command that would be
/// wrong for anyone but a same-machine test. `--token` is
/// `<token_id>:<secret>`, the format holler-client's `join`/`join_ok`
/// implementation (ADR 0015) expects.
fn print_join_command(state_dir: &std::path::Path, token_id: &str, secret: &str) {
    match advertise::read(state_dir) {
        AdvertiseState::Reachable(address) => {
            println!("\nRun on the joining machine:");
            println!("  holler join --server ws://{address} --token {token_id}:{secret}");
        }
        AdvertiseState::LoopbackOnly | AdvertiseState::Unknown => {
            println!(
                "\nwarning: server is bound to loopback only (or has not been started) — no \
                 reachable address to advertise; set --advertise on `holler serve`, or \
                 construct the join command manually with the correct address."
            );
        }
    }
}

fn print_redeem_result(redeemed: &RedeemResult) {
    println!("token_id:   {}", redeemed.token_id);
    println!("client_id:  {}", redeemed.client_id);
    println!("credential: {}", redeemed.credential);
    println!("\nThis credential is shown once and is not stored. Save it now.");
}

fn run_client(command: ClientCommands) -> Result<(), TokenError> {
    let store = TokenStore::open()?;
    match command {
        ClientCommands::List { json } => {
            let views: Vec<_> = store
                .list()?
                .into_iter()
                .filter(|v| v.state == "bound")
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&views)?);
            } else {
                print_table(&views);
            }
            Ok(())
        }
        ClientCommands::Detach { id } => {
            let new_state = store.delete(&id)?;
            notify_live_revoke(store.dir(), &id);
            println!("client {id} is now {new_state}");
            Ok(())
        }
    }
}

