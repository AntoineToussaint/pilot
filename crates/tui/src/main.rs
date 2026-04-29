//! `pilot` — TUI client. Single binary, multiple modes:
//!
//!   pilot                         default: in-process daemon + TUI
//!   pilot --fresh                 wipe ~/.pilot/v2/state.db + force
//!                                  the setup screen (testing first-run)
//!   pilot --test                  throwaway tempdir repo + one fake
//!                                  workspace, no setup, no polling —
//!                                  for trying side panel + terminal
//!                                  pane end-to-end without GitHub
//!   pilot daemon start            standalone daemon (for remote access)
//!   pilot daemon stop             stop a running standalone daemon
//!   pilot daemon status           show daemon status
//!   pilot server api              foreground JSON HTTP API gateway
//!   pilot --connect <socket>      connect to an existing daemon
//!
//! `--fresh` can be combined with `--connect`. All arg parsing is
//! intentionally stupid — see `take_flag` — we have four flags and
//! don't need clap.

use pilot_v2_ipc::{Client, channel, socket};
use pilot_v2_server::lifecycle::{self, ServerStatus};
use pilot_v2_server::polling;
use pilot_v2_server::socket_service::SocketService;
use pilot_v2_server::{Server, ServerConfig};
use pilot_v2_tui::app;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// How often providers are polled. 60s matches v1's default and keeps
/// us well under GitHub's 5000-req/hr ceiling for a typical user.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pilot=info".into()),
        )
        .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let fresh = take_flag(&mut args, "--fresh");
    let test_mode = take_flag(&mut args, "--test");
    if fresh {
        wipe_state_db();
    }
    if test_mode {
        return run_test().await;
    }
    match args.first().map(String::as_str) {
        Some("server") => server_subcommand(&args[1..]).await,
        Some("--connect") => {
            let socket_path = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(lifecycle::socket_path);
            run_remote(&socket_path, fresh).await
        }
        _ => run_embedded(fresh).await,
    }
}

/// `pilot --test` boots against a throwaway tempdir repo + one
/// pre-seeded workspace. No setup screen, no provider polling, no
/// disk writes. The fixture (which owns the TempDir) is held in
/// scope for the whole TUI session — drop = `rm -rf` the tempdir.
async fn run_test() -> anyhow::Result<()> {
    // Use the seeded variant so the sidebar boots with one workspace
    // the user can immediately exercise (`b` for shell, `c` for
    // Claude). The "empty sidebar" empty-state is what real first-run
    // shows BEFORE the poller produces data — it's a different code
    // path. --test is explicitly for trying the panel + terminal,
    // and an empty sidebar there leaves the user with nothing to do.
    let fixture = pilot_v2_tui::test_mode::TestFixture::new_with_seeded_session()?;
    eprintln!("--test repo at {}", fixture.repo.path().display());

    // Spawn under the test tempdir so any agent we launch defaults
    // there. Best-effort — pilot still works if chdir fails.
    let _ = std::env::set_current_dir(fixture.repo.path());

    let (client, server) = channel::pair();
    let config = ServerConfig::with_store(fixture.store.clone());

    // No polling — test mode has no real provider. The bus still
    // carries terminal events.
    tokio::spawn(async move {
        if let Err(e) = Server::new(config).serve(server).await {
            tracing::error!("test-mode daemon exited: {e}");
        }
    });

    pilot_v2_tui::app::run_test_mode(client, fixture.store.clone()).await
    // `fixture` drops here → TempDir cleanup.
}

/// Remove a flag from `args` if present. Returns `true` if it was
/// found. Cheap argv parsing — we don't need clap for three flags.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    if let Some(pos) = args.iter().position(|a| a == flag) {
        args.remove(pos);
        true
    } else {
        false
    }
}

/// `--fresh`: clear v2's `~/.pilot/v2/state.db` so the next launch
/// behaves like a first run (empty sidebar + setup screen forced).
/// Wipes the entire DB, which means the saved setup config in the
/// kv table goes with it. **Never touches v1's `~/.pilot/state.db`**
/// — running v1 alongside v2 must remain safe.
fn wipe_state_db() {
    let path = pilot_v2_server::state_db_path();
    match std::fs::remove_file(&path) {
        Ok(()) => eprintln!("removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("--fresh: couldn't remove {}: {e}", path.display()),
    }
}

async fn run_embedded(force_setup: bool) -> anyhow::Result<()> {
    let (client, server) = channel::pair();
    let config = ServerConfig::from_user_config();

    // Reattach surviving sessions BEFORE the TUI's first Subscribe so
    // the initial Snapshot includes them. With the tmux backend this
    // is how Claude/codex sessions persist across pilot restarts.
    pilot_v2_server::spawn_handler::recover_sessions(&config).await;

    // Server runs immediately so the TUI's setup-time Subscribe works.
    let serve_config = config.clone();
    tokio::spawn(async move {
        let daemon = Server::new(serve_config);
        if let Err(e) = daemon.serve(server).await {
            tracing::error!("daemon exited: {e}");
        }
    });

    // Polling waits for the user to confirm their integration choices
    // in the setup screen. The full PersistedSetup (enabled providers
    // PLUS per-provider role/type filters) flows through, so an
    // unchecked role drops those tasks at fetch time, not just hides
    // them in the UI.
    let polling_config = config.clone();
    pilot_v2_tui::app::run_with_setup_hook(client, force_setup, move |outcome| {
        let cfg = polling_config;
        let setup = pilot_v2_tui::setup_flow::outcome_to_persisted(outcome);
        tokio::spawn(async move {
            let sources = polling::sources_for(&setup).await;
            polling::spawn(cfg, sources, POLL_INTERVAL);
        });
    })
    .await
}

async fn run_remote(socket_path: &std::path::Path, force_setup: bool) -> anyhow::Result<()> {
    if !socket_path.exists() {
        anyhow::bail!(
            "no daemon socket at {}. Start one with `pilot daemon start`.",
            socket_path.display()
        );
    }
    let client = socket::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!("connect {}: {e}", socket_path.display()))?;
    run_client(client, force_setup).await
}

/// Shared client-side flow: hand the IPC client to the live render
/// loop. The loop sends `Subscribe`, enters raw mode, and runs until
/// the user quits or the daemon closes the stream. `force_setup`
/// keeps the setup screen visible even if everything is detected —
/// used by `--fresh` so devs can verify the first-run UX.
async fn run_client(client: Client, force_setup: bool) -> anyhow::Result<()> {
    if force_setup {
        app::run_force_setup(client).await
    } else {
        app::run(client).await
    }
}

async fn server_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("start") => server_start().await,
        Some("stop") => server_stop(),
        Some("status") => server_status(),
        Some("api") => server_api(args.get(1)).await,
        _ => {
            eprintln!("usage: pilot server [start|stop|status|api [addr:port]]");
            std::process::exit(2);
        }
    }
}

async fn server_start() -> anyhow::Result<()> {
    if let ServerStatus::Running { pid } = lifecycle::status() {
        anyhow::bail!("daemon already running (pid {pid})");
    }
    lifecycle::ensure_runtime_dir()?;
    let socket = lifecycle::socket_path();
    let pid_file = lifecycle::pid_path();

    // One config for the whole process — same store, same bus — so the
    // poller's SessionUpserted events reach every connected TUI rather
    // than just the one whose connection happened to spin up its own.
    let config = ServerConfig::from_user_config();
    pilot_v2_server::spawn_handler::recover_sessions(&config).await;
    let sources = polling::default_sources().await;
    polling::spawn(config.clone(), sources, POLL_INTERVAL);

    let factory_config = config.clone();
    let service = SocketService::new(socket.clone(), pid_file, move || factory_config.clone());
    let shutdown = service.shutdown_handle();

    // SIGTERM + SIGINT → graceful shutdown via the notify handle.
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = ctrl_c => {},
        }
        shutdown.notify_one();
    });

    println!("pilot-server listening on {}", socket.display());
    service.run().await?;
    println!("pilot-server stopped");
    Ok(())
}

fn server_stop() -> anyhow::Result<()> {
    if !lifecycle::request_stop()? {
        println!("no daemon running");
        return Ok(());
    }
    println!("sent SIGTERM to daemon");
    Ok(())
}

fn server_status() -> anyhow::Result<()> {
    match lifecycle::status() {
        ServerStatus::Running { pid } => {
            println!(
                "running (pid {pid}) at {}",
                lifecycle::socket_path().display()
            );
        }
        ServerStatus::Stopped => println!("stopped"),
    }
    Ok(())
}

async fn server_api(addr_arg: Option<&String>) -> anyhow::Result<()> {
    let bind_addr = match addr_arg {
        Some(raw) => raw
            .parse::<SocketAddr>()
            .map_err(|e| anyhow::anyhow!("invalid API bind address {raw:?}: {e}"))?,
        None => std::env::var("PILOT_API_ADDR")
            .ok()
            .and_then(|raw| raw.parse::<SocketAddr>().ok())
            .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787)),
    };
    let token = std::env::var("PILOT_API_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());

    let config = ServerConfig::from_user_config();
    pilot_v2_server::spawn_handler::recover_sessions(&config).await;
    println!("pilot API listening on http://{bind_addr}");
    if token.is_some() {
        println!("pilot API bearer auth enabled via PILOT_API_TOKEN");
    } else {
        println!("pilot API bearer auth disabled; bound to localhost by default");
    }

    pilot_v2_server::api_gateway::serve(
        config,
        pilot_v2_server::api_gateway::GatewayOptions {
            bind_addr,
            bearer_token: token,
        },
    )
    .await?;
    Ok(())
}
