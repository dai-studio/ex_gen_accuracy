//! The `moxy` command.
//!
//! `moxy` (or `moxy serve`) runs the server. The remaining subcommands are
//! offline maintenance tools that open the store directly, so they must be
//! run while the server is stopped.

use anyhow::{Context, Result, bail};
use moxy::auth;
use moxy::kv::{KvStore, RedbStore};
use moxy::server::{self, Settings};
use std::process::ExitCode;

const USAGE: &str = "\
Usage: moxy [COMMAND]

Commands:
  serve                       Run the gateway (default)
  check                       Validate moxy.xml and exit
  keys list                   List API keys (server must be stopped)
  keys create <name> [--owner <subject>]
                              Create an API key and print it once (server must be stopped)
  keys revoke <id>            Revoke an API key (server must be stopped)
  version                     Print the version

Environment:
  MOXY_CONFIG    path to moxy.xml (default: ./moxy.xml)
  MOXY_DATA_DIR  directory holding moxy.redb (default: ./data)
  MOXY_LOG       error, warn, info, debug or trace (default: info)
";

fn main() -> ExitCode {
    let settings = match Settings::from_env() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("moxy: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    env_logger::Builder::new()
        .filter_level(settings.log_level)
        .format_timestamp_millis()
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("serve") => server::serve(settings),
        Some("check") => check(&settings),
        Some("keys") => keys(&settings, &args[1..]),
        Some("version" | "--version" | "-V") => {
            println!(
                "moxy {} (serve {})",
                env!("CARGO_PKG_VERSION"),
                server::SERVICE_VERSION
            );
            Ok(())
        }
        Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("unknown command '{other}'\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e:#}");
            eprintln!("moxy: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn check(settings: &Settings) -> Result<()> {
    let env = server::process_env();
    match moxy::state::load_file(&settings.config_path, &env) {
        Ok((cfg, _)) => {
            println!(
                "{} is valid ({} routes)",
                settings.config_path.display(),
                cfg.routes.len()
            );
            Ok(())
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("  {e}");
            }
            bail!("{} is invalid", settings.config_path.display())
        }
    }
}

fn open_store(settings: &Settings) -> Result<RedbStore> {
    RedbStore::open_dir(&settings.data_dir).context("is the server still running? stop it first")
}

fn keys(settings: &Settings, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let kv = open_store(settings)?;
            for k in auth::list_api_keys(&kv)? {
                println!(
                    "{}  {:<20}  owner={}  created={}  last_used={}  {}",
                    k.id,
                    k.name,
                    k.owner,
                    k.created_at.to_rfc3339(),
                    k.last_used_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "never".into()),
                    if k.revoked { "REVOKED" } else { "active" }
                );
            }
            kv.close()
        }
        Some("create") => {
            let name = args
                .get(1)
                .context("usage: moxy keys create <name> [--owner <subject>]")?;
            let owner = match args.get(2).map(String::as_str) {
                Some("--owner") => args.get(3).context("--owner needs a value")?.clone(),
                Some(other) => bail!("unexpected argument '{other}'"),
                None => "cli".to_string(),
            };
            let kv = open_store(settings)?;
            let (plaintext, record) = auth::create_api_key(&kv, name, &owner)?;
            kv.close()?;
            eprintln!(
                "created key {} ({}); it will not be shown again:",
                record.id, record.name
            );
            println!("{plaintext}");
            Ok(())
        }
        Some("revoke") => {
            let id = args.get(1).context("usage: moxy keys revoke <id>")?;
            let kv = open_store(settings)?;
            let found = auth::revoke_api_key(&kv, id)?;
            kv.close()?;
            if !found {
                bail!("no key with id {id}");
            }
            println!("revoked {id}");
            Ok(())
        }
        _ => bail!("usage: moxy keys list | create <name> [--owner <subject>] | revoke <id>"),
    }
}
