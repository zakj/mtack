mod app;
mod client;
mod config;
mod event;
mod input;
mod process;
mod protocol;
mod server;
mod terminal;
mod ui;

use clap::{Parser, Subcommand};
use config::Config;
use miette::IntoDiagnostic;
use server::Paths;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(about, version)]
struct Args {
    /// Path to config file [default: mtack.kdl in cwd or parents]
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start processes in the background
    Up,
    /// Attach to processes started with `up`
    Attach,
    /// Stop background processes
    Down,
    #[command(hide = true)]
    Server,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    let reaches_running = matches!(args.command, Some(Cmd::Attach | Cmd::Down));
    let (config_path, explicit_config) = match args.config {
        Some(path) => (path, true),
        None => match config::find_config_file(&std::env::current_dir().into_diagnostic()?) {
            Ok(path) => (path, false),
            // Outside any project, reach whatever is running rather than
            // failing; inside one, never redirect to another project.
            Err(not_found) if reaches_running => (only_running_config(not_found)?, true),
            Err(not_found) => return Err(not_found),
        },
    };
    // Servers are keyed by the resolved path.
    let config_path = match std::fs::canonicalize(&config_path) {
        Ok(path) => path,
        // A server can outlive its config (a branch switch, a removed
        // worktree); attach and down only need the path to find it.
        Err(_) if reaches_running => std::path::absolute(&config_path).into_diagnostic()?,
        Err(e) => {
            miette::bail!("failed to read {}: {e}", config_path.display());
        }
    };
    let paths = Paths::for_config(&config_path)?;
    // Servers are per config, so hints must name the same one.
    let config_flag = if explicit_config {
        format!(" -c {}", config_path.display())
    } else {
        String::new()
    };

    match args.command {
        None => {
            client::up(&config_path, &paths).await?;
            attach(&paths, &config_flag).await
        }
        Some(Cmd::Up) => {
            match client::up(&config_path, &paths).await? {
                client::Up::Started => {
                    eprintln!("mtack started; `mtack attach{config_flag}` to view");
                }
                client::Up::AlreadyRunning => {
                    eprintln!("mtack is already running for {}", config_path.display());
                }
            }
            Ok(())
        }
        Some(Cmd::Attach) => attach(&paths, &config_flag).await,
        Some(Cmd::Down) => {
            if !client::down(&paths).await? {
                eprintln!("{:?}", not_running(&paths));
            }
            Ok(())
        }
        Some(Cmd::Server) => serve(&config_path, &paths).await,
    }
}

fn only_running_config(not_found: miette::Report) -> miette::Result<PathBuf> {
    match server::running_configs()?.as_slice() {
        [config] => Ok(config.clone()),
        [] => Err(not_found),
        running => Err(with_running_help(not_found.to_string(), running)),
    }
}

fn not_running(paths: &Paths) -> miette::Report {
    let msg = format!("mtack is not running for {}", paths.config.display());
    // Paths::for_config already checked the directory, so listing can't fail
    // on its account.
    with_running_help(msg, &server::running_configs().unwrap_or_default())
}

fn with_running_help(msg: String, running: &[PathBuf]) -> miette::Report {
    if running.is_empty() {
        return miette::miette!("{msg}");
    }
    let list: String = running
        .iter()
        .map(|config| format!("\n  {}", config.display()))
        .collect();
    miette::miette!(
        help = format!("running (use -c to pick one):{list}"),
        "{msg}"
    )
}

async fn attach(paths: &Paths, config_flag: &str) -> miette::Result<()> {
    let Some(end) = client::attach(paths).await? else {
        return Err(not_running(paths));
    };
    match end {
        client::AttachEnd::Detached => eprintln!(
            "detached; `mtack attach{config_flag}` to return, `mtack down{config_flag}` to stop"
        ),
        client::AttachEnd::Exited => {}
        client::AttachEnd::Lost => {
            miette::bail!(
                "mtack server exited unexpectedly; see {}",
                paths.log.display()
            );
        }
    }
    Ok(())
}

async fn serve(config_path: &Path, paths: &Paths) -> miette::Result<()> {
    // Leave the launching terminal's session so closing it doesn't hang us up.
    nix::unistd::setsid().into_diagnostic()?;
    let (listener, _guard) = match server::bind(paths)? {
        server::Bound::Listening(listener, guard) => (listener, guard),
        server::Bound::AlreadyRunning => {
            println!("{}", client::BUSY);
            return Ok(());
        }
    };
    // Only the server holding the lock may clear the log it shares with any
    // server that lost the race to start.
    std::fs::File::create(&paths.log).into_diagnostic()?;
    let config = Config::load(config_path)?;
    println!("{}", client::READY);
    std::io::stdout().flush().into_diagnostic()?;

    app::App::new(&config).run(listener).await
}
