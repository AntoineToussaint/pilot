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
//!
//! All arg parsing is intentionally stupid — see `take_flag`.

use pilot_ipc::{channel, socket};
use pilot_server::lifecycle::{self, ServerStatus};
use std::path::PathBuf;
use pilot_server::polling;
use pilot_server::socket_service::SocketService;
use pilot_server::{Server, ServerConfig};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// How often providers are polled. 60s keeps us well under GitHub's
/// 5000-req/hr ceiling for a typical user.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Where pilot's log file goes.
const LOG_PATH: &str = "/tmp/pilot.log";

/// Initialize tracing to write to `/tmp/pilot.log` instead of stderr.
fn init_tracing() -> anyhow::Result<()> {
    use std::fs::OpenOptions;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .map_err(|e| anyhow::anyhow!("open {LOG_PATH}: {e}"))?;

    // Route the OS stderr into the same log file so native logs from
    // below the Rust layer (libghostty-vt Zig log, libgit2 stderr,
    // agent CLI noise) don't paint over the alternate-screen frame.
    pilot_tui::platform::redirect_stderr_to_file(&file);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "pilot=info,pilot_gh=info,pilot_server=info".into());

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .init();

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let fresh = take_flag(&mut args, "--fresh");
    let test_mode = take_flag(&mut args, "--test");
    let preselect_workspace = take_value(&mut args, "--workspace");
    let preselect_session = take_value(&mut args, "--session");
    let preselect = preselect_workspace.map(|w| pilot_tui::realm::model::Preselect {
        workspace_key: pilot_core::SessionKey::from(w),
        session_id_raw: preselect_session,
    });
    if fresh {
        wipe_state_db();
    }
    if test_mode {
        return run_test(preselect).await;
    }
    match args.first().map(String::as_str) {
        Some("server") => server_subcommand(&args[1..]).await,
        Some("--connect") => {
            let socket_path = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(lifecycle::socket_path);
            run_remote(&socket_path, preselect).await
        }
        _ => run_embedded_realm(preselect).await,
    }
}

/// `--key value` and `--key=value` parser. Removes both the flag and
/// its value from `args`.
fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    if let Some(pos) = args.iter().position(|a| a == flag) {
        args.remove(pos);
        if pos < args.len() {
            return Some(args.remove(pos));
        }
        return None;
    }
    if let Some(pos) = args.iter().position(|a| a.starts_with(&prefix)) {
        let raw = args.remove(pos);
        return Some(raw[prefix.len()..].to_string());
    }
    None
}

#[cfg(test)]
mod argv_tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn take_flag_finds_and_removes() {
        let mut a = args(&["--fresh", "--workspace", "foo"]);
        assert!(take_flag(&mut a, "--fresh"));
        assert_eq!(a, args(&["--workspace", "foo"]));
    }

    #[test]
    fn take_flag_returns_false_when_absent() {
        let mut a = args(&["--workspace", "foo"]);
        assert!(!take_flag(&mut a, "--fresh"));
        assert_eq!(a, args(&["--workspace", "foo"]));
    }
}

/// `pilot --test` boots against a throwaway tempdir repo + one
/// pre-seeded workspace. No setup screen, no provider polling, no
/// disk writes. The fixture (which owns the TempDir) is held in
/// scope for the whole TUI session — drop = `rm -rf` the tempdir.
async fn run_test(preselect: Option<pilot_tui::realm::model::Preselect>) -> anyhow::Result<()> {
    let fixture = pilot_tui::test_mode::TestFixture::new_with_seeded_session()?;
    eprintln!("--test repo at {}", fixture.repo.path().display());

    // Spawn under the test tempdir so any agent we launch defaults
    // there. Best-effort — pilot still works if chdir fails.
    let _ = std::env::set_current_dir(fixture.repo.path());

    let (client, server) = channel::pair();
    let config = ServerConfig::with_store(fixture.store.clone());

    tokio::spawn(async move {
        if let Err(e) = Server::new(config).serve(server).await {
            tracing::error!("test-mode daemon exited: {e}");
        }
    });

    tokio::task::spawn_blocking(move || {
        let mut model = pilot_tui::realm::Model::new(client)?;
        if let Some(p) = preselect {
            model = model.with_preselect(p);
        }
        pilot_tui::realm::model::run_loop_with_model(model)
    })
    .await
    .map_err(|e| anyhow::anyhow!("realm task panicked: {e}"))?
    // `fixture` drops here → TempDir cleanup.
}

/// Remove a flag from `args` if present. Returns `true` if it was
/// found.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    if let Some(pos) = args.iter().position(|a| a == flag) {
        args.remove(pos);
        true
    } else {
        false
    }
}

/// `--fresh`: clear `~/.pilot/v2/state.db`. Wipes the entire DB,
/// which means the saved setup config in the kv table goes with it.
fn wipe_state_db() {
    let path = pilot_server::state_db_path();
    match std::fs::remove_file(&path) {
        Ok(()) => eprintln!("removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("--fresh: couldn't remove {}: {e}", path.display()),
    }
}

/// `pilot --connect <socket>` — connect to a standalone daemon over
/// a Unix socket and run the realm UI against it. The remote path
/// trusts the daemon's persisted setup (no first-run wizard, no
/// detection, no polling kickoff — all of that lives on the daemon
/// side).
async fn run_remote(
    socket_path: &std::path::Path,
    preselect: Option<pilot_tui::realm::model::Preselect>,
) -> anyhow::Result<()> {
    if !socket_path.exists() {
        anyhow::bail!(
            "no daemon socket at {}. Start one with `pilot server start`.",
            socket_path.display()
        );
    }
    let client = socket::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!("connect {}: {e}", socket_path.display()))?;

    tokio::task::spawn_blocking(move || {
        let mut model = pilot_tui::realm::Model::new(client)?;
        if let Some(p) = preselect {
            model = model.with_preselect(p);
        }
        pilot_tui::realm::model::run_loop_with_model(model)
    })
    .await
    .map_err(|e| anyhow::anyhow!("realm task panicked: {e}"))?
}

/// Realm-based default boot path. Spawns the daemon, runs detection
/// if no setup exists (kicks the wizard), kicks the polling loop on
/// completion, runs the realm UI on a blocking task.
async fn run_embedded_realm(
    preselect: Option<pilot_tui::realm::model::Preselect>,
) -> anyhow::Result<()> {
    let (client, server) = channel::pair();
    let config = ServerConfig::from_user_config();

    pilot_server::spawn_handler::recover_sessions(&config).await;
    pilot_server::spawn_handler::restore_persisted_sessions(&config).await;

    let serve_config = config.clone();
    tokio::spawn(async move {
        let daemon = Server::new(serve_config);
        if let Err(e) = daemon.serve(server).await {
            tracing::error!("daemon exited: {e}");
        }
    });

    // Two paths into the polling loop:
    //   1. Persisted setup exists → kick polling immediately.
    //   2. No persisted setup → run detection, hand the wizard to
    //      the realm `Model`, and wire the on-complete hook to fire
    //      polling once the user finishes.
    let polling_config_immediate = config.clone();
    let persisted = persisted_setup(&*config.store);
    let returning_sources: Vec<String> = persisted
        .as_ref()
        .map(|p| p.enabled_providers.iter().cloned().collect())
        .unwrap_or_default();
    // Capture for handing into Model below (the `if let` arm moves
    // it into the polling task).
    let persisted_for_model = persisted.clone();
    if let Some(persisted) = persisted {
        tokio::spawn(async move {
            let bus = polling_config_immediate.bus.clone();
            let sources = polling::sources_for(&persisted, bus).await;
            polling::spawn(polling_config_immediate, sources, POLL_INTERVAL);
        });
    }

    // Always pre-run detection + scope sources. Two reasons: (1)
    // first-run users need them to seed the wizard; (2) returning
    // users may press `,` mid-session to reopen the wizard for
    // adding repos / agents — we cache the inputs on the model so
    // that path doesn't need to re-run async detection from inside
    // a `spawn_blocking` task. Both calls are read-only + cheap-ish
    // (sub-second on a warm cache).
    let setup_report = pilot_tui::setup::detect_all().await;
    let setup_sources = std::sync::Arc::new(build_scope_sources().await);
    let needs_wizard = persisted_setup(&*config.store).is_none();
    let wizard_seed = if needs_wizard {
        Some((setup_report.clone(), setup_sources.clone()))
    } else {
        None
    };

    let polling_after_setup = config.clone();
    let store_for_save = config.store.clone();
    tokio::task::spawn_blocking(move || {
        let mut model = pilot_tui::realm::Model::new(client)?;
        // Returning user with persisted setup → mount the polling
        // modal up front so the first poll cycle has UI feedback.
        if !returning_sources.is_empty() {
            model.show_polling(returning_sources);
        }
        // Hook: when setup finishes, persist + kick polling + mount
        // the polling progress modal so the user sees the first
        // fetch happen.
        let polling_cfg = polling_after_setup.clone();
        let hook: Box<dyn FnOnce(pilot_tui::setup_flow::SetupOutcome) + Send> =
            Box::new(move |outcome| {
                let persisted =
                    pilot_tui::setup_flow::outcome_to_persisted(&outcome);
                pilot_tui::setup_flow::save_persisted(&*store_for_save, &persisted);
                let bus = polling_cfg.bus.clone();
                let cfg = polling_cfg.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("polling rt");
                    rt.block_on(async move {
                        let sources = polling::sources_for(&persisted, bus).await;
                        polling::spawn(cfg, sources, POLL_INTERVAL);
                        futures_util::future::pending::<()>().await;
                    });
                });
            });
        model = model.with_setup_complete_hook(hook);
        if let Some(p) = preselect {
            model = model.with_preselect(p);
        }
        // Cache so the in-session `,` reopens the wizard without
        // re-running detection. (start_setup_wizard caches too —
        // this populates the cache for returning users.)
        model.cache_setup_inputs(setup_report, setup_sources);
        // Cache persisted state for partial Settings flows ("Add a
        // repo for github" needs to know github is already enabled
        // and what scopes are already picked).
        if let Some(p) = persisted_for_model {
            model.cache_persisted_setup(p);
        }
        // Detect installed editors for the `E` shortcut. User
        // overrides come from `~/.pilot/config.yaml::editors`; the
        // builtins ship as defaults.
        let editors =
            pilot_tui::editors::discover_at_startup(load_user_editors());
        tracing::info!("detected {} editor(s)", editors.len());
        model.cache_editors(editors);
        if let Some((report, sources)) = wizard_seed {
            model.start_setup_wizard(report, sources);
        }
        pilot_tui::realm::model::run_loop_with_model(model)
    })
    .await
    .map_err(|e| anyhow::anyhow!("realm task panicked: {e}"))?
}

/// Build the scope sources used by the setup wizard. GitHub today;
/// Linear ships without a scope-discovery API so the wizard skips it.
async fn build_scope_sources() -> Vec<Box<dyn pilot_core::ScopeSource>> {
    use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
    let mut sources: Vec<Box<dyn pilot_core::ScopeSource>> = Vec::new();
    let chain = CredentialChain::new()
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]));
    if let Ok(cred) = chain.resolve("github").await
        && let Ok(client) = pilot_gh::GhClient::from_credential(cred).await
    {
        sources.push(Box::new(pilot_gh::GhScopes::new(std::sync::Arc::new(
            client,
        ))));
    }
    sources
}

fn persisted_setup(
    store: &dyn pilot_store::Store,
) -> Option<pilot_core::PersistedSetup> {
    pilot_tui::setup_flow::load_persisted(store)
}

/// Read the optional `editors:` list from `~/.pilot/config.yaml`.
/// Errors / missing file → empty vec (the builtins still apply).
fn load_user_editors() -> Vec<pilot_tui::editors::UserEditorEntry> {
    let path = match std::env::var_os("HOME") {
        Some(home) => std::path::PathBuf::from(home).join(".pilot/config.yaml"),
        None => return Vec::new(),
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        editors: Vec<pilot_tui::editors::UserEditorEntry>,
    }
    match serde_yaml::from_str::<Wrapper>(&raw) {
        Ok(w) => w.editors,
        Err(e) => {
            tracing::warn!("config.yaml editors parse failed: {e}");
            Vec::new()
        }
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

    let config = ServerConfig::from_user_config();
    pilot_server::spawn_handler::recover_sessions(&config).await;
    let sources = polling::default_sources(config.bus.clone()).await;
    polling::spawn(config.clone(), sources, POLL_INTERVAL);

    let factory_config = config.clone();
    let service = SocketService::new(socket.clone(), pid_file, move || factory_config.clone());
    let shutdown = service.shutdown_handle();

    tokio::spawn(async move {
        pilot_tui::platform::wait_for_shutdown_signal().await;
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
            println!("running (pid {pid}) at {}", lifecycle::socket_path().display());
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
    pilot_server::spawn_handler::recover_sessions(&config).await;
    println!("pilot API listening on http://{bind_addr}");
    if token.is_some() {
        println!("pilot API bearer auth enabled via PILOT_API_TOKEN");
    } else {
        println!("pilot API bearer auth disabled; bound to localhost by default");
    }

    pilot_server::api_gateway::serve(
        config,
        pilot_server::api_gateway::GatewayOptions {
            bind_addr,
            bearer_token: token,
        },
    )
    .await?;
    Ok(())
}
