use moxy::config;
use moxy::server;
use moxy::state::{EnvLookup, EnvSettings};
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
moxy - local gateway for AI model endpoints

USAGE:
    moxy [serve]                         Run the server (default)
    moxy check                           Validate moxy.xml and exit
    moxy keys create --name <NAME> [--owner <SUB>]
                                         Create an API key (server must be stopped)
    moxy keys list                       List API keys (server must be stopped)

ENVIRONMENT:
    MOXY_CONFIG    path to moxy.xml (default ./moxy.xml)
    MOXY_DATA_DIR  directory of the KV store (default ./data)
    MOXY_LOG       error | warn | info | debug | trace (default info)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let lookup: EnvLookup = Arc::new(config::process_env);
    let env = EnvSettings::from_env(&*lookup);
    server::init_logging(&env.log);

    match args.first().map(String::as_str) {
        None | Some("serve") => serve(env, lookup),
        Some("check") => check(env, lookup),
        Some("keys") => keys(env, &args[1..]),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("moxy {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn serve(env: EnvSettings, lookup: EnvLookup) -> ExitCode {
    match server::bootstrap(env, lookup) {
        Ok(state) => {
            server::run(state);
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error!("{e}");
            eprintln!("moxy: {e}");
            ExitCode::FAILURE
        }
    }
}

fn check(env: EnvSettings, lookup: EnvLookup) -> ExitCode {
    let text = match std::fs::read_to_string(&env.config_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {}: {e}", env.config_path.display());
            return ExitCode::FAILURE;
        }
    };
    match config::load_str(&text, &*lookup) {
        Ok(a) => {
            println!(
                "{} is valid (version {}, {} routes)",
                env.config_path.display(),
                a.version,
                a.resolved.routes.len()
            );
            ExitCode::SUCCESS
        }
        Err(errs) => {
            eprintln!("{} is invalid:", env.config_path.display());
            for e in errs {
                eprintln!("  - {e}");
            }
            ExitCode::FAILURE
        }
    }
}

fn keys(env: EnvSettings, args: &[String]) -> ExitCode {
    let kv = match server::open_store(&env) {
        Ok(kv) => kv,
        Err(e) => {
            eprintln!("moxy: {e}");
            return ExitCode::FAILURE;
        }
    };
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match args.first().map(String::as_str) {
        Some("create") => {
            let Some(name) = flag("--name") else {
                eprintln!("--name is required\n\n{USAGE}");
                return ExitCode::from(2);
            };
            let owner = flag("--owner").unwrap_or_else(|| "local-cli".into());
            match moxy::auth::new_api_key(&*kv, &name, &owner) {
                Ok((key, rec)) => {
                    println!("{key}");
                    eprintln!("created API key {} ('{}') owned by {}", rec.id, rec.name, rec.owner_sub);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("moxy: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("list") => match moxy::auth::list_api_keys(&*kv) {
            Ok(keys) => {
                for k in keys {
                    println!(
                        "{}\t{}\t{}\t{}{}",
                        k.id,
                        k.name,
                        k.owner_sub,
                        k.created_at.to_rfc3339(),
                        if k.revoked { "\trevoked" } else { "" }
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("moxy: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
