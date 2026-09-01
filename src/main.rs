//! `themed` — a last-write-wins register holding one theme, replicated across a
//! small trusted fleet by push. See README.md.

mod control;
mod daemon;
mod peer;
mod proto;
mod reconcile;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde_json::Value;

/// Peer port, matched by the fleet's service definition.
const DEFAULT_PORT: u16 = 47100;

#[derive(Parser)]
#[command(
    version,
    about = "Keeps one theme in sync across a small trusted fleet"
)]
struct Cli {
    /// Control socket path (daemon: where to listen; client: where to connect).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Log every message, not just state transitions.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    daemon: DaemonArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Set the theme via the local daemon; prints nothing on success.
    Set {
        /// The theme blob, a single JSON value.
        blob: String,
    },
    /// Print the current theme blob as compact JSON.
    Get,
}

/// Flags for the default (daemon) mode.
#[derive(Args)]
struct DaemonArgs {
    /// This host's name; only used to break version ties.
    #[arg(long = "self")]
    self_name: Option<String>,

    /// Peer listener address. Defaults to this host's Tailscale IPv4.
    #[arg(long)]
    listen: Option<String>,

    /// Where the current record is cached between runs.
    #[arg(long)]
    state_file: Option<PathBuf>,

    /// Shell command run on every theme change, with the blob on stdin.
    #[arg(long)]
    reconcile_cmd: Option<String>,

    /// A peer to push to, as `host:port`. Repeatable; none is fine.
    #[arg(long = "peer")]
    peers: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    env_logger::Builder::new()
        .filter_level(if cli.verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .parse_default_env()
        .init();

    let socket = cli.socket.unwrap_or_else(default_socket);

    let result = match cli.command {
        Some(Command::Set { blob }) => {
            parse_blob(&blob).and_then(|blob| control::set(&socket, blob))
        }
        Some(Command::Get) => control::get(&socket).map(|r| println!("{}", r.blob)),
        None => run_daemon(cli.daemon, socket),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run_daemon(args: DaemonArgs, socket: PathBuf) -> std::io::Result<()> {
    let self_name = args.self_name.unwrap_or_else(hostname);
    let listen = args
        .listen
        .unwrap_or_else(|| format!("{}:{DEFAULT_PORT}", tailscale_ip()));

    daemon::run(daemon::Config {
        self_name,
        listen,
        state_file: args.state_file.unwrap_or_else(default_state_file),
        socket,
        reconcile_cmd: args.reconcile_cmd,
        peers: args.peers,
    })
}

fn parse_blob(text: &str) -> std::io::Result<Value> {
    serde_json::from_str(text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("blob: {e}")))
}

/// `$XDG_RUNTIME_DIR/themed.sock`, or a temp-dir fallback (macOS has no
/// `XDG_RUNTIME_DIR`); the fleet passes `--socket` explicitly anyway.
fn default_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("themed.sock"),
        None => std::env::temp_dir().join("themed.sock"),
    }
}

/// `$XDG_STATE_HOME/themed/state.json`, falling back to `~/.local/state`.
fn default_state_file() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("themed/state.json")
}

/// Only feeds the version tiebreaker, so a miss is harmless.
fn hostname() -> String {
    let from_file = std::fs::read_to_string("/etc/hostname").ok();
    let from_uname = || {
        std::process::Command::new("uname")
            .arg("-n")
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    from_file
        .or_else(from_uname)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Ask tailscale for our address, retrying until tailscaled answers — the user
/// session can easily start before the tailnet is up.
fn tailscale_ip() -> Ipv4Addr {
    let mut delay = Duration::from_secs(1);
    loop {
        match std::process::Command::new("tailscale")
            .args(["ip", "-4"])
            .output()
        {
            Ok(out) if out.status.success() => {
                match parse_tailscale_ip(&String::from_utf8_lossy(&out.stdout)) {
                    Some(ip) => return ip,
                    None => log::warn!(
                        "`tailscale ip -4` printed no usable address; retrying in {delay:?}"
                    ),
                }
            }
            Ok(out) => log::warn!(
                "`tailscale ip -4` failed: {}; retrying in {delay:?}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => log::warn!("cannot run tailscale: {e}; retrying in {delay:?}"),
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// Take the first line of `tailscale ip -4` output as a literal IPv4 address.
/// Parsing rather than string-pasting keeps anything odd — a hostname, an
/// error banner, several addresses — from reaching the listener as something
/// that would be resolved or bound.
fn parse_tailscale_ip(output: &str) -> Option<Ipv4Addr> {
    output.lines().next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_output_must_be_a_literal_address() {
        assert_eq!(
            parse_tailscale_ip("100.64.0.1\n"),
            Some(Ipv4Addr::new(100, 64, 0, 1))
        );
        // Only the first address; the rest is not ours to guess at.
        assert_eq!(
            parse_tailscale_ip("100.64.0.1\n100.64.0.2\n"),
            Some(Ipv4Addr::new(100, 64, 0, 1))
        );

        for junk in [
            "",
            "\n",
            "not an address",
            "fezzik.example.ts.net",
            "100.64.0.1 extra",
            "100.64.0.1:47100",
            "fd7a:115c:a1e0::1",
            "Logged out.",
        ] {
            assert_eq!(parse_tailscale_ip(junk), None, "accepted {junk:?}");
        }
    }
}
