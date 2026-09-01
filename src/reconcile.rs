//! The reconcile hook: a shell command fed the raw blob on stdin.

use std::io::Write;
use std::process::{Command, Stdio};

use log::{debug, error};
use serde_json::Value;

/// Run `cmd` under `sh -c`, writing `blob` to its stdin and waiting for it.
/// A failing hook is logged, never fatal.
pub fn run(cmd: &str, blob: &Value) {
    debug!("running reconcile hook");
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return error!("reconcile hook failed to start: {e}"),
    };

    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(blob.to_string().as_bytes())
    {
        error!("writing blob to reconcile hook: {e}");
    }

    match child.wait() {
        Ok(status) if status.success() => debug!("reconcile hook ok"),
        Ok(status) => error!("reconcile hook exited with {status}"),
        Err(e) => error!("waiting on reconcile hook: {e}"),
    }
}
