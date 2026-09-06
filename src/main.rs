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
#[command(name = "holler", about = "Talk circuit for the herd")]
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
    /// This process's own status document (`role: server`). Reaches a
    /// live `holler serve` process over the local control channel if
    /// one is running on this host; otherwise reports a local-only,
    /// not-connected document.
    Status {
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
}

#[derive(clap::Args)]
struct ServeArgs {
    /// `[host:]port`, e.g. `127.0.0.1:41807` or `[::1]:41807`.
    /// Repeatable, to serve more than one address family (ADR 0004).
    /// Defaults to `HOLLER_LISTEN`, then `127.0.0.1:41807`.
    #[arg(long)]
    listen: Vec<String>,
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
        Commands::Status { json } => run_status(json),
        Commands::Roster { json } => run_roster(json),
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

fn run_status(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = TokenStore::open()?;
    let state_dir = store.dir().to_path_buf();
    let live = wire::control::query_status(&state_dir);
    let hostname = wire::local_hostname()?;

    let (listening, clients) = match &live {
        Some(doc) => (doc.listening.clone(), doc.clients),
        None => (Vec::new(), 0),
    };
    let body = wire::hello::status_query_ok_body(&hostname, &listening, clients);

    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        println!("role:      server");
        println!("hostname:  {hostname}");
        println!(
            "protocol:  {} (min {} max {})",
            wire::hello::PROTOCOL_VERSION,
            wire::hello::PROTOCOL_VERSION,
            wire::hello::PROTOCOL_VERSION
        );
        println!(
            "listening: {}",
            if listening.is_empty() {
                "not running".to_string()
            } else {
                listening.join(", ")
            }
        );
        println!("clients:   {clients}");
        println!("status:    {}", if live.is_some() { "healthy" } else { "not running" });
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
            println!("client {id} is now {new_state}");
            Ok(())
        }
    }
}

