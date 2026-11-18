use std::{
    fs,
    io::{IsTerminal, Read, Seek, SeekFrom},
    net::{Ipv4Addr, TcpListener},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde_json::json;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

use crate::{
    config, process,
    qmp::Qmp,
    state::{Guest, Project},
};

async fn control(project: &Project, guest: &Guest) -> Result<Option<Qmp>> {
    let qmp = Qmp::connect(&project.socket(), &guest.name()).await?;
    ensure!(
        qmp.is_some() || !project.pid_alive()?,
        "QEMU's recorded PID is alive but QMP is unavailable; refusing to start, delete, or signal an unidentified process"
    );
    Ok(qmp)
}

pub async fn running(project: &Project) -> Result<Guest> {
    let guest = project.load()?.context("no VM; run `nemu up`")?;
    let mut qmp = control(project, &guest)
        .await?
        .context("VM is stopped; run `nemu up`")?;
    ensure!(
        qmp.execute("query-status").await?["running"] == true,
        "VM is not running; inspect `nemu status` and `nemu logs`"
    );
    Ok(guest)
}

fn ssh(project: &Project, guest: &Guest) -> tokio::process::Command {
    let mut command = process::command("ssh");
    command
        .args([
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
        ])
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile=\"{}\"",
            project
                .state
                .join("known_hosts")
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        ))
        .arg("-o")
        .arg(format!("HostKeyAlias={}", guest.name()))
        .arg("-i")
        .arg(project.state.join("key"))
        .arg("-p")
        .arg(guest.config.ssh_port.to_string());
    command
}

async fn guest_output(project: &Project, guest: &Guest, script: &str) -> Result<String> {
    let mut command = ssh(project, guest);
    command
        .arg(format!("{}@127.0.0.1", guest.config.user))
        .arg(script);
    process::output(&mut command, Duration::from_secs(10)).await
}

pub fn interactive(
    project: &Project,
    guest: &Guest,
    args: Option<Vec<String>>,
) -> Result<std::process::Command> {
    let cwd = std::env::current_dir()?;
    let relative = cwd.strip_prefix(&project.root).unwrap_or(Path::new(""));
    let directory = Path::new(&guest.config.project_mount).join(relative);
    let directory = directory
        .to_str()
        .context("working directory must be UTF-8")?;
    let invocation = match args.as_ref() {
        Some(args) => args
            .iter()
            .map(|arg| process::quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
        None => "\"$SHELL\" -l".to_owned(),
    };
    let mut command = ssh(project, guest);
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        command.arg("-t");
    } else {
        command.arg("-T");
    }
    command
        .arg(format!("{}@127.0.0.1", guest.config.user))
        .arg(format!(
            "cd {} && exec {invocation}",
            process::quote(directory)
        ));
    Ok(command.into_std())
}

pub async fn up(project: &Project) -> Result<()> {
    prepare(project, true).await
}

pub async fn build(project: &Project) -> Result<()> {
    prepare(project, false).await
}

async fn prepare(project: &Project, boot: bool) -> Result<()> {
    let config = config::load(project).await?;
    let mut existing = project.load()?;
    if let Some(guest) = existing.as_mut() {
        let active = control(project, guest).await?.is_some();
        if guest.config != config && !active && !project.disk().exists() {
            guest.config = config.clone();
            guest.image = PathBuf::new();
        }
        ensure!(
            guest.config == config,
            "Nix launch configuration changed; run `nemu rebuild` to apply it"
        );
        if active && boot {
            ready(project, guest).await?;
            println!("VM already running; artifacts are available.");
            return Ok(());
        }
    }
    project.prepare()?;
    let mut guest = existing.unwrap_or_else(|| Guest {
        version: 1,
        id: Uuid::new_v4(),
        project: project.root.clone(),
        flake: project.flake.clone(),
        config,
        tools: PathBuf::new(),
        image: PathBuf::new(),
    });
    project.save(&guest)?;
    if !guest.tool("qemu-img").is_file() {
        guest.tools = config::build(project, "tools", &project.state.join("tools")).await?;
        project.save(&guest)?;
    }
    if !boot {
        let link = if project.disk().exists() {
            "built-image"
        } else {
            "image"
        };
        let image = config::build(project, "image", &project.state.join(link)).await?;
        let image = find_image(&image)?;
        if !project.disk().exists() {
            guest.image = image.clone();
            project.save(&guest)?;
        }
        println!("Image built: {}", image.display());
        return Ok(());
    }
    if !guest.image.is_file() {
        let image = config::build(project, "image", &project.state.join("image")).await?;
        guest.image = find_image(&image)?;
        project.save(&guest)?;
    }
    keys(project, &guest).await?;
    disk(project, &guest).await?;
    launch(project, &guest).await?;
    ready(project, &guest).await.with_context(|| format!("guest was launched but is not ready; it has been left running for diagnosis. Use `nemu logs` or `nemu stop --force` (state: {})", project.state.display()))?;
    println!("VM ready. Project: {}", guest.config.project_mount);
    for port in &guest.config.ports {
        println!("http://127.0.0.1:{} → guest TCP {}", port.host, port.guest);
    }
    Ok(())
}

fn find_image(output: &Path) -> Result<PathBuf> {
    if output.is_file() {
        return Ok(output.to_owned());
    }
    let mut images = Vec::new();
    for entry in fs::read_dir(output)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "qcow2") {
            images.push(path);
        }
    }
    ensure!(
        images.len() == 1,
        "image output {} must contain exactly one qcow2 file",
        output.display()
    );
    Ok(images.remove(0))
}

async fn keys(project: &Project, guest: &Guest) -> Result<()> {
    let bootstrap = project.state.join("bootstrap");
    fs::create_dir_all(&bootstrap)?;
    fs::set_permissions(&bootstrap, fs::Permissions::from_mode(0o700))?;
    for path in [
        project.state.join("key"),
        bootstrap.join("ssh_host_ed25519_key"),
    ] {
        if !path.exists() {
            let mut command = process::command("ssh-keygen");
            command
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&path);
            process::checked(&mut command).await?;
        }
        let mut command = process::command("ssh-keygen");
        command.args(["-y", "-f"]).arg(&path);
        let public = process::output(&mut command, Duration::from_secs(5)).await?;
        if path == project.state.join("key") {
            fs::write(bootstrap.join("authorized_keys"), public)?;
        } else {
            fs::write(
                project.state.join("known_hosts"),
                format!("{} {}\n", guest.name(), public.trim()),
            )?;
        }
    }
    Ok(())
}

async fn disk(project: &Project, guest: &Guest) -> Result<()> {
    if project.disk().exists() {
        return Ok(());
    }
    let mut info = process::command(guest.tool("qemu-img"));
    info.args(["info", "--output=json"]).arg(&guest.image);
    let info: serde_json::Value =
        serde_json::from_str(&process::output(&mut info, Duration::from_secs(10)).await?)?;
    ensure!(info["format"] == "qcow2", "Nix image must be qcow2");
    let bytes = guest.config.disk_gi_b * 1024 * 1024 * 1024;
    ensure!(
        info["virtual-size"]
            .as_u64()
            .context("image has no virtual size")?
            <= bytes,
        "diskGiB is smaller than the base image"
    );
    let temporary = project.state.join("disk.new.qcow2");
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let mut command = process::command(guest.tool("qemu-img"));
    command
        .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(&guest.image)
        .arg(&temporary)
        .arg(bytes.to_string());
    process::checked(&mut command).await?;
    fs::rename(temporary, project.disk())?;
    Ok(())
}

fn qemu_value(path: &Path) -> Result<String> {
    let value = path.to_str().context("QEMU paths must be UTF-8")?;
    ensure!(
        !value.contains(['\n', '\r']),
        "QEMU paths cannot contain line breaks"
    );
    Ok(value.replace(',', ",,"))
}

async fn launch(project: &Project, guest: &Guest) -> Result<()> {
    ensure!(
        project.socket().as_os_str().as_encoded_bytes().len() < 104,
        "state path is too long for a Unix socket; select a shorter --state-dir"
    );
    let mut reservations = Vec::new();
    for port in
        std::iter::once(guest.config.ssh_port).chain(guest.config.ports.iter().map(|p| p.host))
    {
        reservations.push(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| format!("host port {port} is already in use; choose another port in the Nix configuration"))?);
    }
    ensure!(
        control(project, guest).await?.is_none(),
        "VM is already running"
    );
    for name in ["qmp.sock", "qemu.pid"] {
        let path = project.state.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let arm = guest.config.system.starts_with("aarch64");
    let executable = if arm {
        "qemu-system-aarch64"
    } else {
        "qemu-system-x86_64"
    };
    let firmware = guest.tools.join("share/qemu").join(if arm {
        "edk2-aarch64-code.fd"
    } else {
        "edk2-x86_64-code.fd"
    });
    ensure!(
        firmware.is_file(),
        "missing UEFI firmware {}",
        firmware.display()
    );
    let accelerator = if cfg!(target_os = "macos") {
        "hvf"
    } else {
        "kvm"
    };
    let machine = if arm { "virt,gic-version=3" } else { "q35" };
    let mut command = process::command(guest.tool(executable));
    command
        .arg("-name")
        .arg(guest.name())
        .args([
            "-machine",
            &format!("{machine},accel={accelerator}"),
            "-cpu",
            "host",
        ])
        .arg("-smp")
        .arg(format!(
            "{},sockets={},cores={},threads=1",
            guest.config.cpus,
            guest.config.numa_nodes,
            guest.config.cpus / guest.config.numa_nodes
        ))
        .arg("-m")
        .arg(format!("{}M", guest.config.memory_mi_b));
    for node in 0..guest.config.numa_nodes {
        let cores = guest.config.cpus / guest.config.numa_nodes;
        command
            .arg("-object")
            .arg(format!(
                "memory-backend-ram,id=node{node},size={}M",
                guest.config.memory_mi_b / u64::from(guest.config.numa_nodes)
            ))
            .arg("-numa")
            .arg(format!(
                "node,nodeid={node},cpus={}-{},memdev=node{node}",
                node * cores,
                (node + 1) * cores - 1
            ));
    }
    let mut network = format!(
        "user,id=net0,hostfwd=tcp:127.0.0.1:{}-:22",
        guest.config.ssh_port
    );
    for port in &guest.config.ports {
        network.push_str(&format!(
            ",hostfwd=tcp:127.0.0.1:{}-:{}",
            port.host, port.guest
        ));
    }
    command
        .arg("-bios")
        .arg(firmware)
        .arg("-drive")
        .arg(format!(
            "if=virtio,format=qcow2,file={}",
            qemu_value(&project.disk())?
        ))
        .arg("-netdev")
        .arg(network)
        .args([
            "-device",
            "virtio-net-pci,netdev=net0",
            "-device",
            "virtio-rng-pci",
        ]);
    let mut shares = vec![
        ("project".to_owned(), project.root.clone(), false),
        (
            "bootstrap".to_owned(),
            project.state.join("bootstrap"),
            true,
        ),
    ];
    for (index, share) in guest.config.shares.iter().enumerate() {
        let path = project.root.join(&share.host);
        let path = fs::canonicalize(&path)
            .with_context(|| format!("resolve share host path {}", path.display()))?;
        ensure!(
            path.is_dir(),
            "share host path {} is not a directory",
            path.display()
        );
        shares.push((format!("share{index}"), path, share.read_only));
    }
    for (id, path, readonly) in shares {
        command
            .arg("-fsdev")
            .arg(format!(
                "local,id={id},path={},security_model=none,readonly={}",
                qemu_value(&path)?,
                if readonly { "on" } else { "off" }
            ))
            .arg("-device")
            .arg(format!("virtio-9p-pci,fsdev={id},mount_tag=nemu_{id}"));
    }
    command
        .args(["-display", "none", "-monitor", "none"])
        .arg("-serial")
        .arg(format!(
            "file:{}",
            project.state.join("serial.log").display()
        ))
        .arg("-qmp")
        .arg(format!(
            "unix:{},server=on,wait=off",
            qemu_value(&project.socket())?
        ))
        .arg("-daemonize")
        .arg("-pidfile")
        .arg(project.state.join("qemu.pid"))
        .stdin(Stdio::null());
    drop(reservations);
    eprintln!("Starting {}…", guest.name());
    timeout(Duration::from_secs(30), process::checked(&mut command))
        .await
        .context("QEMU launch timed out; inspect VM state before retrying")??;
    Ok(())
}

async fn ready(project: &Project, guest: &Guest) -> Result<()> {
    let deadline = Instant::now() + project.timeout;
    let check = format!(
        "test -d {} && test -w {}",
        process::quote(&guest.config.project_mount),
        process::quote(&guest.config.project_mount)
    );
    loop {
        let mut qmp = control(project, guest)
            .await?
            .context("QEMU exited before readiness; inspect `nemu logs`")?;
        ensure!(
            qmp.execute("query-status").await?["running"] == true,
            "QEMU is not running"
        );
        let result = guest_output(project, guest, &check).await;
        match result {
            Ok(_) => break,
            Err(error) if Instant::now() >= deadline => {
                return Err(error)
                    .context("SSH / writable project mount did not become ready before timeout");
            }
            Err(_) => sleep(Duration::from_millis(500)).await,
        }
    }
    checks(project, guest).await
}

async fn checks(project: &Project, guest: &Guest) -> Result<()> {
    for check in &guest.config.checks {
        guest_output(project, guest, &check.command)
            .await
            .with_context(|| format!("guest readiness check {:?} failed", check.name))?;
        eprintln!("OK: {}", check.name);
    }
    Ok(())
}

pub async fn status(project: &Project, as_json: bool) -> Result<()> {
    let Some(guest) = project.load()? else {
        println!(
            "{}",
            if as_json {
                "{\"status\":\"absent\"}"
            } else {
                "No VM created."
            }
        );
        return Ok(());
    };
    let state = match control(project, &guest).await? {
        Some(mut qmp) => qmp.execute("query-status").await?["status"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned(),
        None => "stopped".to_owned(),
    };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"status":state,"id":guest.id,"project":guest.project,"config":guest.config,"image":guest.image,"stateDirectory":project.state})
            )?
        );
    } else {
        println!(
            "{}: {state}\n{} CPUs · {} MiB RAM · {} GiB disk · {} NUMA node(s)\nProject: {} → {}\nSSH: 127.0.0.1:{}\nState: {}",
            guest.name(),
            guest.config.cpus,
            guest.config.memory_mi_b,
            guest.config.disk_gi_b,
            guest.config.numa_nodes,
            project.root.display(),
            guest.config.project_mount,
            guest.config.ssh_port,
            project.state.display()
        );
    }
    Ok(())
}

pub async fn stop(project: &Project, force: bool) -> Result<()> {
    let Some(guest) = project.load()? else {
        println!("No VM created.");
        return Ok(());
    };
    stop_guest(project, &guest, force).await
}

async fn stop_guest(project: &Project, guest: &Guest, force: bool) -> Result<()> {
    let Some(mut qmp) = control(project, guest).await? else {
        println!("VM already stopped.");
        return Ok(());
    };
    qmp.execute(if force { "quit" } else { "system_powerdown" })
        .await?;
    drop(qmp);
    let deadline = Instant::now() + project.timeout;
    loop {
        let socket = match Qmp::connect(&project.socket(), &guest.name()).await {
            Ok(socket) => socket,
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    )
                }) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        if socket.is_none() && !project.pid_alive()? {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "shutdown timed out; VM and disk retained. Inspect `nemu logs`, or explicitly use `nemu stop --force`"
        );
        sleep(Duration::from_millis(250)).await;
    }
    println!("VM stopped; disk retained.");
    Ok(())
}

pub async fn rebuild(project: &Project) -> Result<()> {
    let mut guest = project.load()?.context("no VM to rebuild; run `nemu up`")?;
    ensure!(
        project.disk().is_file(),
        "initial VM creation is incomplete; retry `nemu up`"
    );
    let next = config::load(project).await?;
    ensure!(
        next.system == guest.config.system && next.disk_gi_b == guest.config.disk_gi_b,
        "rebuild cannot change architecture or disk capacity; create a separate VM instead"
    );
    let tools = config::build(project, "tools", &project.state.join("tools-next")).await?;
    let system = config::build(project, "toplevel", &project.state.join("system")).await?;
    if control(project, &guest).await?.is_none() {
        launch(project, &guest).await?;
    }
    ready(project, &guest).await?;
    let ssh = ssh(project, &guest).into_std();
    let options = ssh
        .get_args()
        .map(|arg| process::quote(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let mut copy = process::nix();
    copy.args([
        "copy",
        "--no-check-sigs",
        "--to",
        &format!(
            "ssh-ng://{}@127.0.0.1:{}",
            guest.config.user, guest.config.ssh_port
        ),
    ])
    .arg(&system)
    .env("NIX_SSHOPTS", options);
    process::checked(&mut copy)
        .await
        .context("copy rebuilt system to guest")?;
    let mut activate = ssh_command(
        project,
        &guest,
        &format!(
            "sudo nix-env --profile /nix/var/nix/profiles/system --set {} && sudo {} boot",
            process::quote(&system.to_string_lossy()),
            process::quote(&system.join("bin/switch-to-configuration").to_string_lossy())
        ),
    );
    process::checked(&mut activate)
        .await
        .context("install next guest boot configuration")?;
    stop_guest(project, &guest, false).await?;
    guest.config = next;
    guest.tools = tools;
    project.save(&guest)?;
    let mut root = process::command("nix-store");
    root.arg("--add-root")
        .arg(project.state.join("tools"))
        .args(["--indirect", "--realise"])
        .arg(&guest.tools);
    process::checked(&mut root).await?;
    fs::remove_file(project.state.join("tools-next"))?;
    launch(project, &guest).await?;
    ready(project, &guest).await?;
    let booted = guest_output(project, &guest, "readlink -f /run/current-system").await?;
    ensure!(
        Path::new(booted.trim()) == system,
        "guest booted {}, not {}; retained for diagnosis",
        booted.trim(),
        system.display()
    );
    println!("Rebuilt and rebooted; writable disk and build cache preserved.");
    Ok(())
}

fn ssh_command(project: &Project, guest: &Guest, script: &str) -> tokio::process::Command {
    let mut command = ssh(project, guest);
    command
        .arg(format!("{}@127.0.0.1", guest.config.user))
        .arg(script);
    command
}

pub async fn doctor(project: &Project) -> Result<()> {
    let nix = process::output(process::nix().arg("--version"), Duration::from_secs(10)).await?;
    println!("{}\nHost: {}", nix.trim(), config::host()?);
    let desired = config::load(project).await?;
    println!("Nix configuration and lock: OK");
    let Some(guest) = project.load()? else {
        println!(
            "No guest yet. On macOS, run `nemu setup` once to register Nemu's downloaded Linux builder."
        );
        return Ok(());
    };
    ensure!(
        guest.config == desired,
        "running/stored configuration differs from the flake; run `nemu rebuild`"
    );
    if control(project, &guest).await?.is_none() {
        println!("Guest stopped; guest checks were not run.");
        return Ok(());
    }
    ready(project, &guest).await?;
    println!("{}", guest_output(project, &guest, "uname -srmo; printf 'Locked-memory limit (KiB): '; ulimit -l; grep -E 'MemTotal|HugePages|Hugepagesize' /proc/meminfo; df -h /; printf 'Current system: '; readlink -f /run/current-system").await?);
    println!("SSH, writable project mount, and configured readiness checks: OK");
    Ok(())
}

pub fn logs(project: &Project, lines: usize) -> Result<()> {
    project.load()?.context("no VM created")?;
    let mut file = fs::File::open(project.state.join("serial.log")).context("no serial log yet")?;
    let len = file.metadata()?.len();
    let offset = len.saturating_sub(1024 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let collected: Vec<_> = text.lines().collect();
    for line in collected.iter().skip(collected.len().saturating_sub(lines)) {
        println!("{line}");
    }
    Ok(())
}

pub async fn delete(project: &Project, yes: bool) -> Result<()> {
    let Some(guest) = project.load()? else {
        println!("No VM created.");
        return Ok(());
    };
    ensure!(
        control(project, &guest).await?.is_none(),
        "VM is running; stop it before deleting"
    );
    ensure!(
        yes,
        "deletion permanently removes {} (guest disk, caches, and keys). Repeat with --yes to confirm; project sources are preserved",
        project.state.display()
    );
    fs::remove_dir_all(&project.state).context("remove owned VM state")?;
    println!(
        "Deleted {}. Guest data cannot be recovered; project sources were not removed.",
        project.state.display()
    );
    Ok(())
}
