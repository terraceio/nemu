use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{self, Config};

pub struct Project {
    pub root: PathBuf,
    pub flake: PathBuf,
    pub state: PathBuf,
    pub timeout: Duration,
    pub keep_builder: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guest {
    pub version: u32,
    pub id: Uuid,
    pub project: PathBuf,
    pub flake: PathBuf,
    pub config: Config,
    pub tools: PathBuf,
    pub image: PathBuf,
}

impl Guest {
    pub fn name(&self) -> String {
        format!("nemu-{}", self.id)
    }
    pub fn tool(&self, name: &str) -> PathBuf {
        self.tools.join("bin").join(name)
    }
}

impl Project {
    pub fn open(root: PathBuf, flake: PathBuf, state: PathBuf, timeout: u64) -> Result<Self> {
        let root = fs::canonicalize(root).context("resolve project directory")?;
        ensure!(root.is_dir(), "project must be a directory");
        let flake = resolve_future_path(&root.join(flake))?;
        let state = root.join(state);
        ensure!(!state.is_symlink(), "state directory must not be a symlink");
        let state = resolve_future_path(&state)?;
        ensure!(
            state.file_name().is_some(),
            "state directory must not be a filesystem root"
        );
        let parent = state.parent().context("state directory has no parent")?;
        ensure!(
            !root.starts_with(&state) && !flake.starts_with(&state),
            "state directory must not contain the project or its flake"
        );
        fs::create_dir_all(parent)?;
        ensure!(
            !state
                .join("qmp.sock")
                .as_os_str()
                .as_encoded_bytes()
                .contains(&0),
            "invalid state path"
        );
        Ok(Self {
            root,
            flake,
            state,
            timeout: Duration::from_secs(timeout),
            keep_builder: false,
        })
    }

    pub fn lock(&self) -> Result<File> {
        lock(&self.state)
    }

    pub fn prepare(&self) -> Result<()> {
        if self.state.exists() {
            ensure!(
                self.state.join("state.json").is_file(),
                "{} exists without a Nemu ownership record; refusing to use it",
                self.state.display()
            );
            self.load()?;
        } else {
            fs::create_dir(&self.state)?;
            fs::set_permissions(&self.state, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub fn load(&self) -> Result<Option<Guest>> {
        let path = self.state.join("state.json");
        if !self.state.exists() {
            return Ok(None);
        }
        let guest: Guest = serde_json::from_slice(&fs::read(&path).with_context(|| {
            format!(
                "read {}; refusing unmanaged or incomplete state",
                path.display()
            )
        })?)?;
        ensure!(
            guest.version == 1,
            "unsupported Nemu state version {}",
            guest.version
        );
        ensure!(
            guest.project == self.root && guest.flake == self.flake,
            "VM belongs to a different project or flake; use its original paths"
        );
        guest.config.validate()?;
        Ok(Some(guest))
    }

    pub fn save(&self, guest: &Guest) -> Result<()> {
        save_json(&self.state.join("state.json"), guest)
    }

    pub fn installable(&self, attribute: &str) -> Result<String> {
        ensure!(
            self.flake.join("flake.nix").is_file(),
            "no flake at {}; run `nemu init`",
            self.flake.display()
        );
        let path = self
            .flake
            .to_str()
            .context("Nix flake path must be UTF-8")?;
        ensure!(
            !path.contains(['#', '?']),
            "Nix flake path cannot contain # or ?"
        );
        Ok(format!("path:{path}#nemu.{}.{attribute}", config::host()?))
    }

    pub fn disk(&self) -> PathBuf {
        self.state.join("disk.qcow2")
    }
    pub fn socket(&self) -> PathBuf {
        self.state.join("qmp.sock")
    }

    pub fn pid_alive(&self) -> Result<bool> {
        pid_alive(&self.state.join("qemu.pid"))
    }
}

pub fn lock(directory: &Path) -> Result<File> {
    let mut name = directory.as_os_str().to_owned();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(PathBuf::from(name))
        .context("open VM lifecycle lock")?;
    file.try_lock()
        .context("another Nemu command is using this VM")?;
    Ok(file)
}

pub fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("json.new");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn pid_alive(path: &Path) -> Result<bool> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let pid: i32 = content
        .trim()
        .parse()
        .context("invalid QEMU pidfile; refusing to assume the VM is stopped")?;
    ensure!(pid > 1, "invalid QEMU PID");
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn resolve_future_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()));
    }
    let name = path
        .file_name()
        .context("path has an unresolved parent component")?;
    let parent = path.parent().context("path has no parent")?;
    Ok(resolve_future_path(parent)?.join(name))
}
