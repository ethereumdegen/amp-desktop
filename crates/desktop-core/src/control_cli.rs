//! Owner-only commands for an already-running desktop; never starts a runtime.
use std::{ffi::OsString, path::PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

const HELP: &str = "AMP Desktop remote owner commands\n\nUsage:\n  remote status [--data-dir PATH]\n  remote enable [--url HTTPS_TS_NET_ORIGIN] [--port PORT] [--data-dir PATH]\n  remote configure [--url HTTPS_TS_NET_ORIGIN | --clear-url] [--port PORT] [--data-dir PATH]\n  remote disable [--data-dir PATH]\n  remote token [--data-dir PATH]\n  remote rotate-token [--data-dir PATH]\n  remote help\n  remote --help\n\nConnects to the already-running AMP Desktop as the same OS user.\nUse the same app data directory as that desktop; no app or engine is started.\nPorts must be between 1 and 65535. URL policy is enforced by the desktop.\nOnly token and rotate-token print the raw secret; protect their output.\nSame-user processes are trusted owners with full remote administration access.\n";

struct Command {
    directory: PathBuf,
    method: &'static str,
    args: Value,
    secret: bool,
}

/// Arguments exclude the top-level `remote`. Returns stdout, including its newline.
/// No command launches a desktop, and mutations are never automatically retried.
pub async fn run(args: Vec<OsString>, default_directory: PathBuf) -> Result<String> {
    let Some(command) = parse(args, default_directory)? else {
        return Ok(HELP.to_owned());
    };
    let response =
        crate::local_control::request(&command.directory, command.method, command.args).await?;
    if command.secret {
        let token = response
            .get("token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty() && !token.contains(['\r', '\n']))
            .context("Desktop returned an invalid remote token response")?;
        Ok(format!("{token}\n"))
    } else {
        Ok(format!("{}\n", serde_json::to_string_pretty(&response)?))
    }
}

fn parse(args: Vec<OsString>, default_directory: PathBuf) -> Result<Option<Command>> {
    let mut args = args.into_iter();
    let command = args
        .next()
        .context("Missing remote command; use remote help")?;
    let command = command
        .to_str()
        .context("Remote command must be valid UTF-8")?;
    if matches!(command, "help" | "--help") {
        anyhow::ensure!(
            args.next().is_none(),
            "Remote help does not accept arguments"
        );
        return Ok(None);
    }
    let (method, secret) = match command {
        "status" => ("remote_status", false),
        "enable" | "configure" | "disable" => ("remote_configure", false),
        "token" => ("remote_token", true),
        "rotate-token" => ("remote_rotate_token", true),
        _ => bail!("Unknown remote command; use remote help"),
    };
    let configurable = matches!(command, "enable" | "configure");
    let mut directory = default_directory;
    let mut saw_directory = false;
    let mut patch = Map::new();
    match command {
        "enable" => {
            patch.insert("enabled".into(), Value::Bool(true));
        }
        "disable" => {
            patch.insert("enabled".into(), Value::Bool(false));
        }
        _ => {}
    }
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--data-dir") => {
                anyhow::ensure!(!saw_directory, "--data-dir may only be specified once");
                let value = flag_value(&mut args, "--data-dir requires a path")?;
                directory = PathBuf::from(value);
                saw_directory = true;
            }
            Some("--url") if configurable => {
                anyhow::ensure!(
                    !patch.contains_key("tailnetUrl"),
                    "Use only one of --url and --clear-url, once"
                );
                let value = flag_value(&mut args, "--url requires an HTTPS tailnet origin")?;
                let url = value.to_str().context("--url requires valid UTF-8")?;
                // Do not duplicate the controller's authoritative URL policy here.
                patch.insert("tailnetUrl".into(), Value::String(url.to_owned()));
            }
            Some("--clear-url") if command == "configure" => {
                anyhow::ensure!(
                    !patch.contains_key("tailnetUrl"),
                    "Use only one of --url and --clear-url, once"
                );
                patch.insert("tailnetUrl".into(), Value::Null);
            }
            Some("--port") if configurable => {
                anyhow::ensure!(
                    !patch.contains_key("port"),
                    "--port may only be specified once"
                );
                let value = flag_value(&mut args, "--port requires a number between 1 and 65535")?;
                let text = value
                    .to_str()
                    .context("--port requires a number between 1 and 65535")?;
                anyhow::ensure!(
                    text.bytes().all(|byte| byte.is_ascii_digit()),
                    "--port requires a number between 1 and 65535"
                );
                let port = text
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .context("--port requires a number between 1 and 65535")?;
                patch.insert("port".into(), Value::from(port));
            }
            _ => bail!("Unsupported argument for remote command; use remote help"),
        }
    }
    anyhow::ensure!(
        command != "configure" || !patch.is_empty(),
        "Configure requires --url, --clear-url, or --port"
    );
    Ok(Some(Command {
        directory,
        method,
        args: if method == "remote_configure" {
            Value::Object(patch)
        } else {
            Value::Null
        },
        secret,
    }))
}

fn flag_value(args: &mut impl Iterator<Item = OsString>, error: &'static str) -> Result<OsString> {
    let value = args.next().context(error)?;
    anyhow::ensure!(
        !value.is_empty() && !value.to_string_lossy().starts_with("--"),
        "{error}"
    );
    Ok(value)
}

#[cfg(test)]
#[path = "control_cli_tests.rs"]
mod tests;
