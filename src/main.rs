//! `holler` CLI (issue #29: the first entry point in this crate).
//!
//! Only `token mint/list/delete/ping` exists today. `Cli`/`Commands`
//! are shaped so a later story (#35, "Meta-O CLI wiring") can add
//! sibling top-level subcommands (`status`, `support`, `roster`,
//! `say`, ...) without restructuring this file.

use clap::{Parser, Subcommand};
use holler_server::token::{
    parse_ttl, AlwaysDisconnected, PingOutcome, TokenError, TokenStore, DEFAULT_TTL,
};

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
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Token(args) => run_token(args.command),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
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
        TokenCommands::Ping { id } => match store.ping(&id, &AlwaysDisconnected) {
            Ok(PingOutcome::Connected { hostname, rtt_ms }) => {
                println!("{hostname} rtt={rtt_ms}ms");
                Ok(())
            }
            Err(e) => Err(e),
        },
    }
}

fn print_table(views: &[holler_server::token::TokenView]) {
    const HEADERS: [&str; 6] = [
        "TOKEN_ID",
        "STATE",
        "MACHINE",
        "LABEL",
        "LAST_SEEN",
        "EXPIRES",
    ];
    let rows: Vec<[&str; 6]> = views
        .iter()
        .map(|v| {
            [
                v.token_id.as_str(),
                v.state.as_str(),
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

    let print_row = |cells: &[&str; 6]| {
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
