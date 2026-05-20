//! Cross-platform shims for the few OS-specific bits pilot needs.
//!
//! Pilot is Unix-first today (macOS + Linux) but the long-term plan
//! is a Windows port. Rather than scatter `cfg(unix)` blocks across
//! `main.rs`, `realm::model`, and `pilot-server::lifecycle`, the
//! platform-touching primitives live here. Each function has a unix
//! impl that does the real thing and a windows stub that returns an
//! error or `pending()`. When the Windows port lands, fill in the
//! windows arms and the rest of the code compiles unchanged.
//!
//! ## What's wrapped
//!
//! - [`redirect_stderr_to_file`] — point fd 2 at a file (`dup2` on
//!   unix; Windows would use `SetStdHandle` + `ReOpenFile`).
//! - [`detach_child_process`] — set up a `std::process::Command` so
//!   the spawned child outlives the parent (`setsid` on unix; Windows
//!   would use `CREATE_NEW_PROCESS_GROUP` + `DETACHED_PROCESS`).
//! - [`wait_for_shutdown_signal`] — async wait for SIGTERM / SIGINT
//!   (or Ctrl-Break on Windows). Resolves once.

/// Redirect process stderr (fd 2) to the given open file. Best-effort
/// — failures are silently ignored; the caller already has a fallback
/// (the tracing layer also writes to the file directly).
///
/// **Why:** native logging from below the Rust layer (libghostty-vt's
/// Zig `log.warn`, libgit2 stderr, agent CLIs that write to fd 2)
/// paints directly onto the user's terminal otherwise, corrupting
/// the alternate-screen frame ratatui just drew. Routing fd 2 into
/// `/tmp/pilot.log` keeps the screen clean.
pub fn redirect_stderr_to_file(file: &std::fs::File) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // Safety: `dup2` is sound here — `file.as_raw_fd()` is a
        // valid fd we own, fd 2 is always valid, and the call
        // doesn't expose any pointers. Done before any TUI subsystem
        // starts.
        unsafe {
            let _ = libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
        }
    }
    #[cfg(windows)]
    {
        // TODO(windows): SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle())
        let _ = file;
    }
}

/// Detach a child `Command` from the parent's session group so the
/// child survives the parent process exiting. Used by the
/// `Ctrl-Shift-D` detach flow that re-spawns pilot pinned to a
/// specific workspace.
///
/// On unix: `setsid()` via `pre_exec`. On Windows: `CREATE_NEW_PROCESS_GROUP`
/// + `DETACHED_PROCESS` flags (TODO).
pub fn detach_child_process(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Safety: `setsid()` only mutates the calling process's
        // session-id — no pointer hazards, no Rust-side state.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        // TODO(windows): CommandExt::creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        let _ = cmd;
    }
}

/// Fire a desktop notification with `title` + `body`. Best-effort —
/// returns immediately whether or not the OS surface is available
/// (no PII in error logs; we don't want a missing dependency to
/// generate noise on every notification).
///
/// Used to surface agent state changes that need the user's
/// attention even when pilot isn't the focused app — e.g. Claude
/// going to `Asking` while the user is reading email.
///
/// **macOS**: prefers `terminal-notifier` (if installed via brew —
/// gets its own notification icon instead of the generic AppleScript
/// plug). Falls back to `osascript -e 'display notification ...'` —
/// works out-of-the-box but the icon comes from Script Editor.
/// Pilot bundled as a `.app` would solve this properly; until then,
/// `terminal-notifier` is the easy escape hatch.
///
/// **Linux**: `notify-send` (libnotify). Present on every desktop
/// environment we'd realistically support. Skipped silently if
/// `notify-send` is missing.
///
/// **Windows**: stub (TODO: PowerShell `New-BurntToastNotification`).
pub fn notify_user(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        // Cache the `terminal-notifier` lookup so we don't spawn
        // `which` on every notification. `OnceLock` is `Sync`, safe
        // to share across the threads that fire notifications.
        use std::sync::OnceLock;
        static TERMINAL_NOTIFIER: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
        let tn = TERMINAL_NOTIFIER.get_or_init(|| which::which("terminal-notifier").ok());

        if let Some(tn_path) = tn {
            // `terminal-notifier` ships with its own bundle, so the
            // notification carries a proper app icon instead of the
            // Script Editor plug. We pass `-group pilot` so repeated
            // notifications collapse into a single stack rather than
            // piling up. `-sender` is intentionally omitted — without
            // a real pilot.app bundle id, spoofing one would surface
            // the wrong app's icon.
            let _ = std::process::Command::new(tn_path)
                .arg("-title")
                .arg(title)
                .arg("-message")
                .arg(body)
                .arg("-group")
                .arg("com.pilot.agent")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            return;
        }

        // Fallback: AppleScript. Always available; the icon will be
        // Script Editor's generic plug (cosmetic limitation; install
        // `brew install terminal-notifier` to get a better icon).
        // We escape double-quotes so the user's strings can't break
        // out of the AppleScript string literal.
        let safe_title = title.replace('"', "\\\"");
        let safe_body = body.replace('"', "\\\"");
        let script = format!(
            "display notification \"{safe_body}\" with title \"{safe_title}\""
        );
        // Detached + ignored — we don't care about exit status and
        // don't want to block. `spawn()` returns immediately;
        // dropping the child handle on a non-waited child is fine
        // for this fire-and-forget case (the OS reaps it).
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("notify-send")
            .arg(title)
            .arg(body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(windows)]
    {
        let _ = (title, body);
    }
}

/// Async wait for a graceful-shutdown signal — SIGTERM or Ctrl-C on
/// unix, Ctrl-Break on Windows. Resolves once. Used by
/// `pilot server start`'s outer task to trigger a clean stop.
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = ctrl_c => {},
        }
    }
    #[cfg(windows)]
    {
        // TODO(windows): tokio::signal::windows::{ctrl_c, ctrl_break}
        let _ = tokio::signal::ctrl_c().await;
    }
}
