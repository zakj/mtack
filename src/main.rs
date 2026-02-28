mod app;
mod config;
mod event;
mod input;
mod process;
mod terminal;
mod ui;

use clap::Parser;
use config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about, version)]
struct Args {
    /// Path to config file [default: mtack.kdl in cwd or parents]
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    let config = match args.config {
        Some(path) => {
            let input = std::fs::read_to_string(&path)
                .map_err(|e| miette::miette!("failed to read {}: {e}", path.display()))?;
            config::parse(&input)?
        }
        None => {
            let cwd = std::env::current_dir().map_err(|e| miette::miette!("{e}"))?;
            Config::load(&cwd)?
        }
    };

    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
        .map_err(|e| miette::miette!("{e}"))?;

    let mut app = app::App::new(&config);
    let result = app.run(&mut terminal).await;

    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
        .map_err(|e| miette::miette!("{e}"))?;
    ratatui::restore();

    result
}
