use std::{
    fs::{self, File},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use crate::{config, process, qmp::Qmp, state};

const DEFAULT_SOURCE: &str = "github:NixOS/nixpkgs/b18a4b905f8d028dc4476412e6d6891728695379";
const PORT: u16 = 31122;
const REGISTRATION: &str = "/etc/nix/nemu/machines";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    id: Uuid,
    source: String,
    source_path: PathBuf,
    spec: Option<Spec>,
    #[serde(default)]
    persistent: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Spec {
    guest: PathBuf,
    runner: PathBuf,
    executable: String,
    host_key: String,
}

struct Builder {
    directory: PathBuf,
    record: Record,
    _lock: File,
}

fn directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is required to locate Nemu's builder")?;
    Ok(PathBuf::from(home).join("Library/Application Support/nemu/builder"))
}

fn source() -> String {
    std::env::var("NEMU_BUILDER_NIXPKGS").unwrap_or_else(|_| DEFAULT_SOURCE.into())
}

impl Builder {
    fn open() -> Result<Self> {
        ensure!(
            cfg!(target_os = "macos"),
            "Linux uses Nix natively and does not need a builder VM"
        );
        let directory = directory()?;
        fs::create_dir_all(
            directory
                .parent()
                .context("builder directory has no parent")?,
        )?;
        ensure!(
            !directory.is_symlink(),
            "builder directory must not be a symlink"
        );
        let lock = state::lock(&directory)?;
        let path = directory.join("state.json");
        let record = if directory.exists() {
            let record: Record = serde_json::from_slice(
                &fs::read(&path)
                    .context("refusing builder directory without a valid ownership record")?,
            )?;
            ensure!(record.version == 1, "unsupported builder state version");
            record
        } else {
            fs::create_dir(&directory)?;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
            let record = Record {
                version: 1,
                id: Uuid::new_v4(),
                source: source(),
                source_path: PathBuf::new(),
                spec: None,
                persistent: false,
            };
            state::save_json(&path, &record)?;
            record
        };
        Ok(Self {
            directory,
            record,
            _lock: lock,
        })
    }

    fn name(&self) -> String {
        format!("nemu-builder-{}", self.record.id)
    }
    fn spec(&self) -> Result<&Spec> {
        self.record
            .spec
            .as_ref()
            .context("builder artifacts have not been fetched; run `nemu setup`")
    }
    fn save(&self) -> Result<()> {
        state::save_json(&self.directory.join("state.json"), &self.record)
    }

    async fn control(&self) -> Result<Option<Qmp>> {
        let qmp = Qmp::connect(&self.directory.join("run/qmp.sock"), &self.name()).await?;
        ensure!(
            qmp.is_some() || !state::pid_alive(&self.directory.join("run/qemu.pid"))?,
            "builder PID is alive but QMP is unavailable; refusing to control an unidentified process"
        );
        Ok(qmp)
    }

    fn nix_expression(&self, operation: &str, attribute: &str) -> Result<tokio::process::Command> {
        let mut cmd = process::nix();
        cmd.args([operation, "-f"])
            .arg(self.directory.join("builder.nix"))
            .arg("--argstr")
            .arg("nixpkgsPath")
            .arg(&self.record.source_path)
            .args(["--argstr", "hostSystem", &config::host()?, attribute]);
        Ok(cmd)
    }

    async fn prepare(&mut self) -> Result<()> {
        let wanted = source();
        if wanted != self.record.source {
            ensure!(
                self.control().await?.is_none(),
                "builder version changed; run `nemu builder stop` before switching NEMU_BUILDER_NIXPKGS"
            );
            self.record.source = wanted;
            self.record.source_path = PathBuf::new();
            self.record.spec = None;
            self.save()?;
        }
        if self.record.source_path.as_os_str().is_empty() || !self.record.source_path.is_dir() {
            let mut cmd = process::nix();
            cmd.args(["flake", "metadata", "--json", &self.record.source]);
            let metadata: serde_json::Value =
                serde_json::from_str(&process::output(&mut cmd, Duration::from_secs(120)).await?)?;
            self.record.source_path = PathBuf::from(
                metadata["path"]
                    .as_str()
                    .context("Nix did not return a source path")?,
            );
            let mut root = process::command("nix-store");
            root.arg("--add-root")
                .arg(self.directory.join("source"))
                .args(["--indirect", "--realise"])
                .arg(&self.record.source_path);
            process::checked(&mut root).await?;
            self.save()?;
        }
        let expression = self.directory.join("builder.nix");
        let content = include_str!("../nix/builder.nix");
        let changed = fs::read_to_string(&expression).ok().as_deref() != Some(content);
        if changed {
            ensure!(
                self.control().await?.is_none(),
                "builder launcher changed; run `nemu builder stop` first"
            );
            fs::write(&expression, content)?;
            self.record.spec = None;
            self.save()?;
        }
        if self
            .record
            .spec
            .as_ref()
            .is_none_or(|spec| !spec.runner.is_dir())
        {
            eprintln!("Fetching stock Linux builder from {}…", self.record.source);
            let mut eval = self.nix_expression("eval", "spec")?;
            eval.arg("--json");
            let spec: Spec =
                serde_json::from_str(&process::output(&mut eval, Duration::from_secs(120)).await?)?;
            let mut cached = process::nix();
            cached
                .args([
                    "path-info",
                    "--recursive",
                    "--store",
                    "https://cache.nixos.org",
                ])
                .arg(&spec.guest);
            process::output(&mut cached, Duration::from_secs(120)).await.context("this builder's Linux closure is not fully available in cache.nixos.org; choose a cached NEMU_BUILDER_NIXPKGS revision")?;
            let mut build = self.nix_expression("build", "runner")?;
            build.arg("--out-link").arg(self.directory.join("runner"));
            process::checked(&mut build).await?;
            self.record.spec = Some(spec);
            self.save()?;
        }
        let key = self.directory.join("key");
        if !key.exists() {
            let mut generate = process::command("ssh-keygen");
            generate
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&key);
            process::checked(&mut generate).await?;
        }
        let mut public = process::command("ssh-keygen");
        public.args(["-y", "-f"]).arg(&key);
        let public = process::output(&mut public, Duration::from_secs(10)).await?;
        fs::create_dir_all(self.directory.join("keys"))?;
        fs::write(self.directory.join("key.pub"), &public)?;
        fs::write(self.directory.join("keys/builder_ed25519.pub"), public)?;
        let mut add = process::command("nix-store");
        add.arg("--add").arg(self.directory.join("keys"));
        let public_keys = process::output(&mut add, Duration::from_secs(30)).await?;
        let mut root = process::command("nix-store");
        root.arg("--add-root")
            .arg(self.directory.join("keys-public"))
            .args(["--indirect", "--realise", public_keys.trim()]);
        process::output(&mut root, Duration::from_secs(30)).await?;
        fs::write(
            self.directory.join("known_hosts"),
            format!("{} {}", self.name(), self.spec()?.host_key),
        )?;
        Ok(())
    }

    fn machines(&self) -> Result<String> {
        Ok(format!(
            "ssh-ng://builder@127.0.0.1:{PORT} {}-linux /etc/nix/nemu/key 4 1 kvm,big-parallel - {}\n",
            std::env::consts::ARCH,
            STANDARD.encode(self.spec()?.host_key.as_bytes())
        ))
    }

    fn registered(&self) -> Result<bool> {
        Ok(
            fs::read_to_string(REGISTRATION).ok().as_deref() == Some(self.machines()?.as_str())
                && fs::read(self.directory.join("key.pub")).ok()
                    == fs::read("/etc/nix/nemu/key.pub").ok()
                && fs::read_to_string("/etc/nix/nix.conf")
                    .unwrap_or_default()
                    .lines()
                    .any(|line| line.trim() == "!include nemu.conf")
                && fs::read_to_string("/etc/nix/nemu.conf")
                    .unwrap_or_default()
                    .lines()
                    .any(|line| {
                        line.starts_with("builders = ") && line.ends_with("@/etc/nix/nemu/machines")
                    }),
        )
    }

    fn ssh(&self) -> Result<tokio::process::Command> {
        let mut cmd = process::command("ssh");
        cmd.args([
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=2",
            "-o",
        ])
        .arg(format!(
            "UserKnownHostsFile=\"{}\"",
            self.directory
                .join("known_hosts")
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        ))
        .arg("-o")
        .arg(format!("HostKeyAlias={}", self.name()))
        .arg("-i")
        .arg(self.directory.join("key"))
        .arg("-p")
        .arg(PORT.to_string())
        .arg("builder@127.0.0.1");
        Ok(cmd)
    }

    async fn start(&self) -> Result<()> {
        if self.control().await?.is_none() {
            let reservation = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, PORT))
                .context("Nemu builder port 31122 is already in use")?;
            ensure!(
                self.directory
                    .join("run/qmp.sock")
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    < 104,
                "builder state path exceeds Unix socket limit"
            );
            for name in ["qmp.sock", "qemu.pid", "certs/ca-certificates.crt"] {
                let path = self.directory.join("run").join(name);
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
            fs::create_dir_all(self.directory.join("run"))?;
            let mut launch = process::command(
                self.spec()?
                    .runner
                    .join("bin")
                    .join(&self.spec()?.executable),
            );
            launch
                .current_dir(&self.directory)
                .env(
                    "KEYS",
                    fs::canonicalize(self.directory.join("keys-public"))?,
                )
                .env("TMPDIR", self.directory.join("run"))
                .env("USE_TMPDIR", "1")
                .env("NIX_DISK_IMAGE", self.directory.join("disk.qcow2"))
                .env_remove("QEMU_OPTS")
                .env_remove("QEMU_NET_OPTS")
                .env_remove("QEMU_KERNEL_PARAMS")
                .args([
                    "-name",
                    &self.name(),
                    "-qmp",
                    "unix:qmp.sock,server=on,wait=off",
                    "-daemonize",
                    "-pidfile",
                    "qemu.pid",
                ])
                .stdin(Stdio::null());
            if cfg!(target_arch = "aarch64") {
                launch.args(["-machine", "virt,gic-version=3,accel=hvf"]);
            } else {
                launch.args(["-accel", "hvf"]);
            }
            drop(reservation);
            eprintln!("Starting Nemu's Linux builder…");
            process::checked(&mut launch).await?;
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let mut qmp = self
                .control()
                .await?
                .context("builder exited before readiness; inspect its run/serial.log")?;
            ensure!(
                qmp.execute("query-status").await?["running"] == true,
                "builder is not running"
            );
            let mut check = self.ssh()?;
            check.arg("nix-store --version");
            match process::output(&mut check, Duration::from_secs(5)).await {
                Ok(_) => return Ok(()),
                Err(error) if Instant::now() >= deadline => {
                    return Err(error).context(
                        "builder SSH readiness timed out; builder retained for diagnosis",
                    );
                }
                Err(_) => sleep(Duration::from_millis(500)).await,
            }
        }
    }

    async fn stop(&self, force: bool) -> Result<()> {
        let Some(mut qmp) = self.control().await? else {
            return Ok(());
        };
        if force {
            qmp.execute("quit").await?;
        } else {
            let mut shutdown = self.ssh()?;
            shutdown.arg("shutdown -h now");
            process::checked(&mut shutdown).await?;
        }
        drop(qmp);
        let deadline = Instant::now() + Duration::from_secs(120);
        while state::pid_alive(&self.directory.join("run/qemu.pid"))? {
            ensure!(
                Instant::now() < deadline,
                "builder shutdown timed out; retained for diagnosis"
            );
            sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    }

    async fn discard(&mut self) -> Result<()> {
        self.stop(true).await?;
        for name in ["disk.qcow2", "runner", "source", "keys-public"] {
            let path = self.directory.join(name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove {}", path.display()));
                }
            }
        }
        let run = self.directory.join("run");
        ensure!(
            !run.is_symlink(),
            "builder runtime directory must not be a symlink"
        );
        if run.exists() {
            fs::remove_dir_all(&run)?;
        }
        self.record.source_path = PathBuf::new();
        self.record.spec = None;
        self.save()?;
        eprintln!("Builder stopped and deleted; registration retained.");
        Ok(())
    }
}

pub struct Lease {
    builder: Builder,
    keep: bool,
}

impl Lease {
    pub async fn finish(mut self) -> Result<()> {
        if !self.keep && !self.builder.record.persistent {
            self.builder.discard().await?;
        }
        Ok(())
    }
}

pub async fn lease(keep: bool) -> Result<Option<Lease>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    let mut builder = Builder::open()?;
    let prepared = async {
        builder.prepare().await?;
        ensure!(
            builder.registered()?,
            "Nemu's builder needs one-time Nix daemon registration. Run `nemu setup` in a terminal; it will request administrator approval. Existing builders are preserved."
        );
        builder.start().await
    }
    .await;
    if let Err(error) = prepared {
        if !keep && !builder.record.persistent {
            builder.discard().await.with_context(|| {
                format!("builder preparation failed: {error:#}; cleanup also failed")
            })?;
        }
        return Err(error);
    }
    Ok(Some(Lease { builder, keep }))
}

pub async fn setup(keep: bool) -> Result<()> {
    if !cfg!(target_os = "macos") {
        println!("Linux builds run natively; no builder VM registration is needed.");
        return Ok(());
    }
    let mut builder = Builder::open()?;
    builder.prepare().await?;
    if !builder.registered()? {
        let mut config = process::nix();
        config.args(["config", "show", "builders"]);
        let existing = process::output(&mut config, Duration::from_secs(30)).await?;
        let mut entries: Vec<_> = existing
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty() && *entry != "@/etc/nix/nemu/machines")
            .collect();
        ensure!(
            entries.iter().all(|entry| !entry.contains(['\r', '\n'])),
            "cannot safely preserve multiline Nix builder configuration"
        );
        entries.push("@/etc/nix/nemu/machines");
        let setting = format!("builders = {}", entries.join("; "));
        eprintln!(
            "Administrator approval is required to register Nemu's builder. This adds /etc/nix/nemu.conf and its include, installs a dedicated key under /etc/nix/nemu/, and restarts the Nix daemon. Existing builders and credentials are preserved."
        );
        let mut install = process::command("sudo");
        install
            .args([
                "/bin/sh",
                "-eu",
                "-c",
                include_str!("../nix/register-builder.sh"),
                "nemu-setup",
            ])
            .arg(builder.directory.join("key"))
            .arg(builder.machines()?)
            .arg(setting);
        process::checked(&mut install).await.context(
            "builder registration needs administrator approval; rerun `nemu setup` in a terminal",
        )?;
        ensure!(
            builder.registered()?,
            "builder registration verification failed"
        );
    }
    builder.start().await?;
    if !keep && !builder.record.persistent {
        builder.discard().await?;
    }
    println!(
        "Nemu builder configured; future `nemu build`, `up`, and `rebuild` commands manage it automatically."
    );
    Ok(())
}

pub async fn start() -> Result<()> {
    let mut builder = Builder::open()?;
    builder.prepare().await?;
    builder.start().await?;
    builder.record.persistent = true;
    builder.save()?;
    println!(
        "Builder ready (Nix daemon registration: {}).",
        if builder.registered()? {
            "installed"
        } else {
            "run nemu setup"
        }
    );
    Ok(())
}

pub async fn status() -> Result<()> {
    if !cfg!(target_os = "macos") {
        println!("Linux builds run natively; no builder VM is needed.");
        return Ok(());
    }
    if !directory()?.exists() {
        println!("No Nemu builder created; run `nemu setup`.");
        return Ok(());
    }
    let builder = Builder::open()?;
    let status = match builder.control().await? {
        Some(mut qmp) => qmp.execute("query-status").await?["status"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned(),
        None => "stopped".into(),
    };
    println!(
        "Builder: {status}\nNixpkgs: {}\nState: {}",
        builder.record.source,
        builder.directory.display()
    );
    Ok(())
}

pub async fn stop(force: bool) -> Result<()> {
    if !cfg!(target_os = "macos") || !directory()?.exists() {
        println!("No Nemu builder running.");
        return Ok(());
    }
    let mut builder = Builder::open()?;
    builder.stop(force).await?;
    builder.record.persistent = false;
    builder.save()?;
    println!("Builder stopped; build cache retained.");
    Ok(())
}
