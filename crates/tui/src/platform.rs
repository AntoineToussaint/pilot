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

use std::future::Future;

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

/// Async wait for a graceful-shutdown signal — SIGTERM or Ctrl-C on
/// unix, Ctrl-Break on Windows. Resolves once. Used by
/// `pilot server start`'s outer task to trigger a clean stop.
pub fn wait_for_shutdown_signal() -> impl Future<Output = ()> + Send {
    async {
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
}
