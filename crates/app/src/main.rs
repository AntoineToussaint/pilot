use anyhow::Result;
use tracing_subscriber::EnvFilter;

mod action;
mod app;
mod agent_state;
mod claude_hooks;
mod command;
mod reduce;
pub mod input;
pub mod keymap;
mod keys;
mod nav;
mod notify;
pub mod pane;
mod picker;
mod session_manager;
mod state;
mod terminal_manager;
mod ui;

#[tokio::main]
async fn main() -> Result<()> {
    // Redirect stderr to /dev/null to suppress libghostty-vt warnings
    // (e.g. "unimplemented mode: 7727") that leak into the TUI.
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if let Ok(devnull) = std::fs::File::open("/dev/null") {
            // SAFETY: dup2 duplicates devnull's fd into slot STDERR_FILENO
            // *before* we return. It only needs the source fd alive for the
            // duration of the call — `devnull` is dropped right after, which
            // closes the source fd but leaves the duplicate (stderr) open.
            let ret = unsafe { libc::dup2(devnull.as_raw_fd(), libc::STDERR_FILENO) };
            if ret == -1 {
                // Can't report to stderr (we were trying to redirect it) — log it
                // after the subscriber is set up; for now, just eat it silently.
            }
        }
    }

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
