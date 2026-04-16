use anyhow::Result;
use tracing_subscriber::EnvFilter;

mod action;
mod app;
mod agent_state;
pub mod keymap;
mod keys;
mod monitor;
mod nav;
mod notify;
pub mod pane;
mod picker;
mod ui;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // `pilot daemon` — run as background daemon (PTY server).
    if args.get(1).map(|s| s.as_str()) == Some("daemon") {
        let socket_path = args.get(2).map(std::path::PathBuf::from);
        // Daemon has its own logging.
        let log_file = std::fs::File::create("/tmp/pilot-daemon.log")?;
        tracing_subscriber::fmt()
            .with_writer(log_file)
            .with_ansi(false)
            .init();
        pilot_daemon::run_daemon(socket_path).await;
        return Ok(());
    }

    // Normal TUI mode.
    let log_file = std::fs::File::create("/tmp/pilot.log")?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("pilot=debug".parse()?))
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    tracing::info!("pilot starting");

    let config = pilot_config::Config::load()?;
    tracing::info!("Config loaded: {:?}", config);

    let mut app_state = app::App::new(config).await?;
    app::run(&mut app_state).await?;

    tracing::info!("pilot exiting");
    Ok(())
}
