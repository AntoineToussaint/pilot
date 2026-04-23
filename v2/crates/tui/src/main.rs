//! `pilot` — TUI client. Single binary, multiple modes:
//!
//!   pilot                         default: in-process daemon + TUI
//!   pilot daemon start            standalone daemon (for remote access)
//!   pilot daemon stop             stop a running standalone daemon
//!   pilot daemon status           show daemon status
//!   pilot --connect <socket>      connect to an existing daemon
//!
//! The component-tree TUI (sidebar, right pane, overlays) is built out
//! in Week 2 per `../../DESIGN.md`. The scaffolding in this file —
//! argv dispatch + `run_embedded` wiring the TUI to an in-process
//! daemon via `ipc::channel::pair` — is the permanent entry point
//! that the component tree plugs into.

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

    // Smoke-test entry point for the in-process wiring. The component
    // tree mounts here in Week 2 — this call site (Subscribe first,
    // then drive the render loop off `client.recv()`) stays.
    client.send(Command::Subscribe).ok();
    println!("pilot v2");
    println!("See v2/DESIGN.md for the architecture.");
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
