use std::{ffi::OsStr, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, time::timeout};

pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.kill_on_drop(true);
    command
}

pub fn nix() -> Command {
    let mut command = command("nix");
    command.args(["--extra-experimental-features", "nix-command flakes"]);
    command
}

pub async fn checked(command: &mut Command) -> Result<()> {
    let name = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let status = command
        .status()
        .await
        .with_context(|| format!("start {name}"))?;
    if !status.success() {
        bail!("{name} exited with {status}");
    }
    Ok(())
}

pub async fn output(command: &mut Command, limit: Duration) -> Result<String> {
    let name = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    command.stdin(Stdio::null());
    let output = timeout(limit, command.output())
        .await
        .with_context(|| format!("{name} timed out after {} seconds", limit.as_secs()))?
        .with_context(|| format!("start {name}"))?;
    if !output.status.success() {
        bail!(
            "{name} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).with_context(|| format!("{name} returned non-UTF-8 output"))
}

pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
