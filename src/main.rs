mod builder;
mod config;
mod process;
mod qmp;
mod state;
mod vm;

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Parser, Subcommand};
use state::Project;

#[derive(Parser)]
#[command(version, about = "Nix-defined development VMs, powered by QEMU")]
#[command(
    after_help = "NEMU_BUILDER_NIXPKGS overrides the pinned Nixpkgs reference supplying the macOS Linux builder. Stop the builder before changing it; the selected Linux closure must be available in cache.nixos.org."
)]
struct Cli {
    #[arg(short = 'C', long, default_value = ".", global = true)]
    project: PathBuf,
    #[arg(long, default_value = "nemu", global = true)]
    flake: PathBuf,
    #[arg(long, default_value = ".nemu", global = true)]
    state_dir: PathBuf,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..), global = true)]
    timeout: u64,
    #[arg(
        long,
        global = true,
        help = "Retain the Linux builder and its build cache after building"
    )]
    keep_builder: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Setup,
    Builder {
        #[command(subcommand)]
        command: BuilderAction,
    },
    Init,
    Build,
    Up,
    Exec {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    Shell,
    Status {
        #[arg(long)]
        json: bool,
    },
    Doctor,
    Logs {
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    Rebuild,
    Stop {
        #[arg(long)]
        force: bool,
    },
    Delete {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum BuilderAction {
    Start,
    Status,
    Stop {
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nemu: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Action::Setup => return builder::setup(cli.keep_builder).await,
        Action::Builder { command } => {
            return match command {
                BuilderAction::Start => builder::start().await,
                BuilderAction::Status => builder::status().await,
                BuilderAction::Stop { force } => builder::stop(*force).await,
            };
        }
        _ => {}
    }
    let mut project = Project::open(cli.project, cli.flake, cli.state_dir, cli.timeout)?;
    project.keep_builder = cli.keep_builder;
    let lock = project.lock()?;
    match cli.command {
        Action::Init => config::init(&project).await,
        Action::Build => vm::build(&project).await,
        Action::Up => vm::up(&project).await,
        Action::Exec { command } => {
            let guest = vm::running(&project).await?;
            let mut ssh = vm::interactive(&project, &guest, Some(command))?;
            drop(lock);
            use std::os::unix::process::CommandExt;
            Err(ssh.exec().into())
        }
        Action::Shell => {
            let guest = vm::running(&project).await?;
            let mut ssh = vm::interactive(&project, &guest, None)?;
            drop(lock);
            use std::os::unix::process::CommandExt;
            Err(ssh.exec().into())
        }
        Action::Status { json } => vm::status(&project, json).await,
        Action::Doctor => vm::doctor(&project).await,
        Action::Logs { lines } => vm::logs(&project, lines),
        Action::Rebuild => vm::rebuild(&project).await,
        Action::Stop { force } => vm::stop(&project, force).await,
        Action::Delete { yes } => vm::delete(&project, yes).await,
        Action::Setup | Action::Builder { .. } => unreachable!("handled before opening a project"),
    }
}
