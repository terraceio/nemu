use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{process, state::Project};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub system: String,
    pub cpus: u16,
    pub memory_mi_b: u64,
    pub disk_gi_b: u64,
    pub numa_nodes: u16,
    pub ssh_port: u16,
    pub ports: Vec<Port>,
    pub user: String,
    pub project_mount: String,
    #[serde(default)]
    pub shares: Vec<Share>,
    #[serde(default)]
    pub checks: Vec<Check>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Share {
    pub host: String,
    pub guest: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    pub host: u16,
    pub guest: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub command: String,
}

pub fn host() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => bail!("unsupported host architecture {other}"),
    };
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!("unsupported host operating system {other}"),
    };
    Ok(format!("{arch}-{os}"))
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let expected = format!("{}-linux", std::env::consts::ARCH);
        ensure!(
            self.system == expected,
            "guest system must be {expected}; cross-architecture emulation is not enabled"
        );
        ensure!(
            self.cpus > 0 && self.numa_nodes > 0 && self.cpus.is_multiple_of(self.numa_nodes),
            "cpus must be a positive multiple of numaNodes"
        );
        ensure!(
            self.memory_mi_b >= 512 && self.memory_mi_b.is_multiple_of(u64::from(self.numa_nodes)),
            "memoryMiB must be at least 512 and divisible by numaNodes"
        );
        ensure!(
            self.disk_gi_b > 0 && self.disk_gi_b <= 65536,
            "diskGiB must be between 1 and 65536"
        );
        let mut ports = HashSet::from([self.ssh_port]);
        ensure!(self.ssh_port > 0, "sshPort must be nonzero");
        for port in &self.ports {
            ensure!(
                port.host > 0 && port.guest > 0 && ports.insert(port.host),
                "forwarded host ports must be nonzero and unique, including sshPort"
            );
        }
        ensure!(
            !self.user.is_empty()
                && self
                    .user
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                && !self.user.starts_with('-'),
            "invalid guest user"
        );
        ensure!(
            self.project_mount.starts_with('/')
                && self.project_mount != "/"
                && !self.project_mount.contains(['\n', '\r', '\0']),
            "projectMount must be an absolute non-root guest path"
        );
        let mut mounts = HashSet::from([self.project_mount.as_str(), "/run/nemu-bootstrap"]);
        for share in &self.shares {
            ensure!(
                !share.host.is_empty() && !share.host.contains(['\n', '\r', '\0']),
                "share host paths must be nonempty and on one line"
            );
            ensure!(
                share.guest.starts_with('/')
                    && share.guest != "/"
                    && !share.guest.contains(['\n', '\r', '\0'])
                    && mounts.insert(share.guest.as_str()),
                "share guest paths must be absolute, non-root, and distinct from every other mount"
            );
        }
        Ok(())
    }
}

pub async fn load(project: &Project) -> Result<Config> {
    ensure!(
        project.flake.join("flake.lock").is_file(),
        "missing {}/flake.lock; run `nemu init` or `nix flake lock path:{}` first",
        project.flake.display(),
        project.flake.display()
    );
    let mut command = process::nix();
    command.args([
        "eval",
        "--json",
        "--no-update-lock-file",
        &project.installable("config")?,
    ]);
    let output = process::output(&mut command, Duration::from_secs(120)).await?;
    let config: Config = serde_json::from_str(&output).context("decode Nix nemu configuration")?;
    config.validate()?;
    Ok(config)
}

pub async fn build(project: &Project, attribute: &str, link: &Path) -> Result<PathBuf> {
    let installable = project.installable(attribute)?;
    let mut probe = process::nix();
    probe.args([
        "build",
        "--dry-run",
        "--json",
        "--no-update-lock-file",
        &installable,
    ]);
    let outputs: Vec<serde_json::Value> =
        serde_json::from_str(&process::output(&mut probe, Duration::from_secs(120)).await?)?;
    let paths: Vec<&str> = outputs
        .iter()
        .flat_map(|output| {
            output.as_str().into_iter().chain(
                output["outputs"]
                    .as_object()
                    .into_iter()
                    .flat_map(|outputs| outputs.values().filter_map(|path| path.as_str())),
            )
        })
        .collect();
    ensure!(
        !paths.is_empty(),
        "Nix returned no output paths for {attribute}"
    );
    let mut check = process::nix();
    check.arg("path-info").args(&paths);
    let available = process::output(&mut check, Duration::from_secs(30))
        .await
        .is_ok();
    let builder = if !available && matches!(attribute, "image" | "toplevel") {
        crate::builder::lease(project.keep_builder).await?
    } else {
        None
    };
    eprintln!("Building {attribute}…");
    let mut command = process::nix();
    command
        .args(["build", "--no-update-lock-file", "--out-link"])
        .arg(link)
        .arg(installable);
    if available {
        command.args(["--builders", "", "--max-jobs", "0"]);
    }
    let result = process::checked(&mut command)
        .await
        .with_context(|| format!("build Nix attribute {attribute}"));
    if let Some(builder) = builder {
        builder.finish().await.with_context(|| match &result {
            Ok(()) => "build succeeded, but builder cleanup failed".to_owned(),
            Err(error) => format!("{error:#}; builder cleanup also failed"),
        })?;
    }
    result?;
    fs::canonicalize(link).with_context(|| format!("resolve {}", link.display()))
}

pub async fn init(project: &Project) -> Result<()> {
    ensure!(
        !project.flake.exists(),
        "{} already exists; refusing to overwrite it",
        project.flake.display()
    );
    fs::create_dir_all(&project.flake)?;
    fs::write(
        project.flake.join("flake.nix"),
        include_str!("../nix/flake.nix"),
    )?;
    let uid = nix::unistd::Uid::current().as_raw();
    let guest = include_str!("../nix/guest.nix")
        .replace("@HOST_UID@", &if uid == 0 { 1000 } else { uid }.to_string());
    fs::write(project.flake.join("guest.nix"), guest)?;
    if project.state == project.root.join(".nemu") {
        let ignore = project.root.join(".gitignore");
        let current = fs::read_to_string(&ignore).unwrap_or_default();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ignore)?;
        for entry in ["/.nemu/", "/.nemu.lock"] {
            if !current.lines().any(|line| line == entry) {
                writeln!(file, "\n{entry}")?;
            }
        }
    }
    let mut command = process::nix();
    command
        .args(["flake", "lock"])
        .arg(format!("path:{}", project.flake.display()));
    process::checked(&mut command).await.context("flake files were created, but locking failed; run `nix flake lock` in the generated directory to retry")?;
    println!(
        "Created {}. Run `nemu up` to build and start it.",
        project.flake.display()
    );
    Ok(())
}
