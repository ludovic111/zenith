//! zenith-cli: every zenith command from a terminal, against the running zenith (the local
//! server; it is woken up if it sleeps). Prints JSON.
//!
//! ```text
//! zenith-cli list                              the commands
//! zenith-cli help <command>                    one command's parameters
//! zenith-cli <command> [--param value …]       run it (--flag alone means true)
//! zenith-cli <command> --json '{"param": …}'   run it with JSON parameters
//! zenith-cli docs                              docs/COMMANDS.md
//! zenith-cli setup                             run the server from this zenith.app at login (macOS)
//! zenith-cli setup [--tailscale-serve]         run the server next to zenith-cli at boot (Linux:
//!                                              a systemd user service; see README, "Linux")
//! zenith-cli release keygen <secret-file>      make the update signing key pair
//! zenith-cli release sign <file> [--key <secret-file>]   sign (or ZENITH_UPDATE_SIGNING_KEY)
//! ```

use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Map, Value};
use zenith_client::{Client, ClientIdentity};
use zenith_commands::registry::{self, Caller, Ty, COMMANDS};

fn usage() -> String {
    let mut out = String::from("zenith-cli: drive zenith from a terminal.\n\nUsage:\n  zenith-cli <command> [--param value …]\n  zenith-cli <command> --json '{…}'\n  zenith-cli list | help <command> | docs | setup | --version\n\nCommands:\n");
    for spec in COMMANDS {
        out.push_str(&format!("  {:<28} {}\n", spec.name, spec.summary));
    }
    out
}

fn help(name: &str) -> Option<String> {
    let spec = registry::spec(name)?;
    let mut out = format!("{}: {}\n", spec.name, spec.summary);
    if spec.params.is_empty() {
        out.push_str("\nNo parameters.\n");
    } else {
        out.push_str("\nParameters:\n");
        for p in spec.params {
            let ty = match p.ty {
                Ty::Enum(values) => values.join("|"),
                other => format!("{other:?}").to_lowercase(),
            };
            out.push_str(&format!(
                "  --{:<16} {:<10} {}{}\n",
                p.name,
                ty,
                if p.required { "(required) " } else { "" },
                p.help
            ));
        }
    }
    Some(out)
}

/// `--name value` pairs into JSON parameters, typed by the spec.
fn parse_params(name: &str, args: &[String]) -> Result<Value, String> {
    let spec = registry::spec(name).ok_or_else(|| format!("unknown command {name:?} (see `zenith-cli list`)"))?;
    let mut params = Map::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--json" {
            let text = args.get(i + 1).ok_or("--json needs a value")?;
            let value: Value = serde_json::from_str(text).map_err(|e| format!("--json: {e}"))?;
            match value {
                Value::Object(object) => params.extend(object),
                _ => return Err("--json must be an object".into()),
            }
            i += 2;
            continue;
        }
        let key = arg.strip_prefix("--").ok_or_else(|| format!("unexpected argument {arg:?}"))?;
        let param = spec
            .params
            .iter()
            .find(|p| p.name == key)
            .ok_or_else(|| format!("{name} has no --{key} (see `zenith-cli help {name}`)"))?;
        let next = args.get(i + 1).filter(|v| !v.starts_with("--"));
        let value = match (param.ty, next) {
            (Ty::Bool, None) => {
                i += 1;
                Value::Bool(true)
            }
            (_, None) => return Err(format!("--{key} needs a value")),
            (ty, Some(raw)) => {
                i += 2;
                match ty {
                    Ty::Bool => Value::Bool(matches!(raw.as_str(), "true" | "yes" | "1" | "on")),
                    Ty::Integer => Value::from(raw.parse::<i64>().map_err(|_| format!("--{key} must be a number"))?),
                    Ty::Object | Ty::Any => serde_json::from_str(raw).map_err(|e| format!("--{key}: {e}"))?,
                    Ty::StringList => Value::Array(raw.split(',').map(|s| Value::String(s.trim().to_owned())).collect()),
                    Ty::String | Ty::Enum(_) => Value::String(raw.clone()),
                }
            }
        };
        params.insert(key.to_owned(), value);
    }
    Ok(Value::Object(params))
}

fn release(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("keygen") => {
            let Some(path) = args.get(1) else {
                eprintln!("zenith-cli release keygen <secret-file>");
                return ExitCode::from(2);
            };
            match zenith_commands::update::write_keypair(std::path::Path::new(path)) {
                Ok(public) => {
                    println!("{public}");
                    eprintln!("Secret key written to {path} (0600). Put the public key above in crates/zenith-commands/assets/update-signing.pub and the secret's hex in the ZENITH_UPDATE_SIGNING_KEY secret.");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        Some("sign") => {
            let Some(file) = args.get(1) else {
                eprintln!("zenith-cli release sign <file> [--key <secret-file>]");
                return ExitCode::from(2);
            };
            let key = args.iter().position(|a| a == "--key").and_then(|i| args.get(i + 1)).map(std::path::Path::new);
            match zenith_commands::update::sign_file(key, std::path::Path::new(file)) {
                Ok(sig) => {
                    println!("{}", sig.display());
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        _ => {
            eprintln!("zenith-cli release keygen <secret-file> | sign <file> [--key <secret-file>]");
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().cloned() else {
        print!("{}", usage());
        return ExitCode::from(2);
    };
    match command.as_str() {
        "--version" | "-V" | "version" => {
            println!("zenith-cli {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        "--help" | "-h" | "list" => {
            print!("{}", usage());
            return ExitCode::SUCCESS;
        }
        "help" => {
            return match args.get(1).and_then(|n| help(n)) {
                Some(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                None => {
                    print!("{}", usage());
                    ExitCode::from(2)
                }
            };
        }
        "docs" => {
            print!("{}", registry::markdown());
            return ExitCode::SUCCESS;
        }
        "release" => return release(&args[1..]),
        "setup" => {
            let setup = match zenith_commands::agent::Setup::parse(&args[1..]) {
                Ok(setup) => setup,
                Err(error) => {
                    eprintln!("Error: {error}\nzenith-cli setup [--tailscale-serve [--tailscale-serve-port <port>]]");
                    return ExitCode::from(2);
                }
            };
            return match zenith_commands::agent::setup(&setup) {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    ExitCode::from(1)
                }
            };
        }
        _ => {}
    }
    let params = match parse_params(&command, &args[1..]) {
        Ok(params) => params,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    let result = runtime.block_on(async move {
        let client = Client::connect_local(ClientIdentity {
            surface: "cli",
            app_version: env!("CARGO_PKG_VERSION").into(),
            session_label: "zenith",
        });
        if client.wait_connected(Duration::from_secs(4)).await.is_err() {
            zenith_client::local::kickstart();
            client.wait_connected(Duration::from_secs(20)).await.map_err(|e| {
                format!(
                    "{e}. Is zenith installed? ({})",
                    if cfg!(target_os = "macos") {
                        "npm run mac:install"
                    } else {
                        "zenith-cli setup"
                    }
                )
            })?;
        }
        registry::run(&client, Caller::Cli, &command, params).await.map_err(|e| e.to_string())
    });
    match result {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flags_become_typed_params() {
        let args: Vec<String> = ["--threadId", "t1", "--prompt", "hello", "--wait"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            parse_params("thread.send", &args).unwrap(),
            json!({"threadId": "t1", "prompt": "hello", "wait": true})
        );
        let args: Vec<String> = ["--json", r#"{"threadId":"t1","prompt":"x"}"#].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_params("thread.send", &args).unwrap(), json!({"threadId": "t1", "prompt": "x"}));
        assert!(parse_params("thread.send", &["--nope".to_owned(), "1".to_owned()]).is_err());
        assert!(help("thread.new").unwrap().contains("--projectId"));
    }
}
