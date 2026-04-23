//! `pilot` — TUI client. Single binary, multiple modes:
//!
//!   pilot                         default: in-process daemon + TUI
//!   pilot daemon start            standalone daemon (for remote access)
//!   pilot daemon stop             stop a running standalone daemon
//!   pilot daemon status           show daemon status
//!   pilot --connect <socket>      connect to an existing daemon
//!
//! Week-1 skeleton. The real TUI (component tree, render loop) lands
//! in Week 2. For now `pilot` opens an in-process channel to the
//! daemon library, sends `Subscribe`, prints the snapshot, and exits.

use pilot_v2_daemon::{Daemon, DaemonConfig};
use pilot_v2_ipc::{Command, channel};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pilot=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("daemon") => daemon_subcommand(&args[1..]).await,
        Some("--connect") => {
            anyhow::bail!("--connect not implemented yet (Week 4)");
        }
        _ => run_embedded().await,
    }
}

async fn run_embedded() -> anyhow::Result<()> {
    let (mut client, server) = channel::pair();

    // Daemon runs in-process on a background task.
    tokio::spawn(async move {
        let daemon = Daemon::new(DaemonConfig::from_user_config());
        if let Err(e) = daemon.serve(server).await {
            tracing::error!("daemon exited: {e}");
        }
    });

    // Subscribe and print whatever events arrive. Placeholder until
    // the real component-tree TUI lands.
    client.send(Command::Subscribe).ok();
    println!("pilot v2 (skeleton)");
    println!("See v2/DESIGN.md for the architecture plan.");
    println!("Subscribed. Waiting for Snapshot…");
    if let Some(ev) = client.recv().await {
        println!("{ev:?}");
    }
    Ok(())
}

async fn daemon_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("start") => anyhow::bail!("daemon start: not implemented yet (Week 4)"),
        Some("stop") => anyhow::bail!("daemon stop: not implemented yet (Week 4)"),
        Some("status") => anyhow::bail!("daemon status: not implemented yet (Week 4)"),
        _ => {
            eprintln!("usage: pilot daemon [start|stop|status]");
            std::process::exit(2);
        }
    }
}
