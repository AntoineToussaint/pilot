//! Per-profile root directory + helpers.
//!
//! Pilot used to hardcode `~/.pilot` everywhere — fine for a single
//! installation, but it made running a second instance impossible
//! (two daemons would corrupt each other's `state.db`, claim the
//! same daemon socket, fight over the same tmux sessions). This
//! module is the single chokepoint.
//!
//! ## Override
//!
//! Set `PILOT_HOME` to point at a different directory. Everything
//! pilot writes — state DB, worktrees, daemon socket, config,
//! hooks, tmux sessions — lives under it.
//!
//! ```bash
//! # Default: ~/.pilot
//! pilot
//!
//! # Side-by-side dev instance:
//! PILOT_HOME=~/.pilot-dev cargo run -p pilot-tui
//! ```
//!
//! ## What's shared between profiles
//!
//! - Your GitHub token (resolved via `gh auth token` or env vars).
//! - Your GitHub per-user rate-limit budget. Two instances polling
//!   in parallel still hit the same 5000/hr ceiling, so bump dev's
//!   `~/.pilot-dev/config.yaml::providers.github.poll_interval` to
//!   leave headroom.
//!
//! ## Tmux socket disambiguation
//!
//! [`tmux_socket_name`] derives a unique socket label from the home
//! dir's last component: `~/.pilot` → `pilot`, `~/.pilot-dev` →
//! `pilot-dev`. So `tmux -L pilot attach` shows the stable instance,
//! `tmux -L pilot-dev attach` shows the dev one — no cross-talk.

use std::path::PathBuf;

/// Profile root. Defaults to `$HOME/.pilot`; override with
/// `PILOT_HOME`.
///
/// **Why an env var, not a CLI flag.** The polling task, the daemon
/// socket-service, the spawn handler, and the config loader all
/// resolve paths independently — threading a `--profile` arg
/// through every entry point is a lot of plumbing for the same
/// outcome. `PILOT_HOME=path pilot` reads identically to a flag
/// and works for every subcommand (`pilot`, `pilot daemon start`,
/// `pilot server api`) without per-subcommand wiring.
pub fn home() -> PathBuf {
    if let Ok(dir) = std::env::var("PILOT_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pilot")
}

/// State-versioned subdirectory under `home()`. Today: `<home>/v2`.
/// Wrap so a future schema bump (v3) is one constant change.
pub fn state_root() -> PathBuf {
    home().join("v2")
}

/// SQLite state DB. `<home>/v2/state.db`.
pub fn state_db() -> PathBuf {
    state_root().join("state.db")
}

/// Worktree base. `<home>/v2/worktrees/`.
pub fn worktrees_root() -> PathBuf {
    state_root().join("worktrees")
}

/// User-editable config file. `<home>/config.yaml`. Lives at the
/// profile root (not the versioned subdir) so a schema bump
/// doesn't orphan the user's customizations.
pub fn config_yaml() -> PathBuf {
    home().join("config.yaml")
}

/// Daemon runtime artifacts (socket, pid). `<home>/run/`. Honors
/// the older `PILOT_RUNTIME_DIR` env var for back-compat — set both
/// to the same value if you want, or just unset it and let
/// `PILOT_HOME` win.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PILOT_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home().join("run")
}

/// User-supplied hook scripts. `<home>/hooks/`.
pub fn hooks_dir() -> PathBuf {
    home().join("hooks")
}

/// Tmux socket label for this profile. Derives a stable name from
/// the home dir's last component so two profiles don't collide
/// on a shared tmux server.
///
/// - `~/.pilot` → `"pilot"` (unchanged from before this module
///   existed — preserves backward compatibility with running
///   sessions on the default profile).
/// - `~/.pilot-dev` → `"pilot-dev"`.
/// - Anything weird (path with no usable last component) → `"pilot"`
///   fallback so callers don't have to handle Option.
pub fn tmux_socket_name() -> String {
    home()
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| {
            // `.pilot` → `pilot`, `.pilot-dev` → `pilot-dev`,
            // `pilot-something` → `pilot-something`.
            s.strip_prefix('.').unwrap_or(s).to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pilot".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `EnvGuard` saves + restores an env var across a test so two
    /// tests in the same process don't see each other's setup.
    /// `std::env::set_var` is global state — without scoping,
    /// parallel tests in the same module would race.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // Safety: tests in this module aren't running other
            // threads that read PILOT_HOME concurrently — the
            // `paths` module's functions are short, pure reads.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn home_honors_pilot_home_env() {
        let _g = EnvGuard::set("PILOT_HOME", "/tmp/pilot-test-xyz");
        assert_eq!(home(), PathBuf::from("/tmp/pilot-test-xyz"));
    }

    #[test]
    fn home_empty_pilot_home_falls_back_to_default() {
        // Empty string must NOT be treated as "use /": a fish-shell
        // user with `set -gx PILOT_HOME` and no value would silently
        // get pilot writing to the filesystem root. The check above
        // filters empties to the default branch.
        let _g1 = EnvGuard::set("PILOT_HOME", "");
        let _g2 = EnvGuard::set("HOME", "/tmp/test-home");
        assert_eq!(home(), PathBuf::from("/tmp/test-home/.pilot"));
    }

    #[test]
    fn home_uses_home_env_when_pilot_home_unset() {
        let _g1 = EnvGuard::unset("PILOT_HOME");
        let _g2 = EnvGuard::set("HOME", "/tmp/test-home");
        assert_eq!(home(), PathBuf::from("/tmp/test-home/.pilot"));
    }

    #[test]
    fn state_db_lives_under_state_root() {
        let _g = EnvGuard::set("PILOT_HOME", "/tmp/pilot-x");
        assert_eq!(state_db(), PathBuf::from("/tmp/pilot-x/v2/state.db"));
        assert_eq!(worktrees_root(), PathBuf::from("/tmp/pilot-x/v2/worktrees"));
    }

    #[test]
    fn config_lives_at_profile_root_not_state_root() {
        // Schema versioning isn't supposed to invalidate the user's
        // config. Living at `<home>/config.yaml` instead of
        // `<home>/v2/config.yaml` means a v2→v3 schema bump leaves
        // their customizations alone.
        let _g = EnvGuard::set("PILOT_HOME", "/tmp/pilot-x");
        assert_eq!(config_yaml(), PathBuf::from("/tmp/pilot-x/config.yaml"));
    }

    #[test]
    fn runtime_dir_honors_legacy_env_var() {
        // PILOT_RUNTIME_DIR existed before PILOT_HOME — keep it
        // working so users who already set it don't have to migrate.
        let _g1 = EnvGuard::set("PILOT_HOME", "/tmp/pilot-x");
        let _g2 = EnvGuard::set("PILOT_RUNTIME_DIR", "/var/run/pilot");
        assert_eq!(runtime_dir(), PathBuf::from("/var/run/pilot"));
    }

    #[test]
    fn runtime_dir_falls_back_to_pilot_home_when_legacy_unset() {
        let _g1 = EnvGuard::set("PILOT_HOME", "/tmp/pilot-x");
        let _g2 = EnvGuard::unset("PILOT_RUNTIME_DIR");
        assert_eq!(runtime_dir(), PathBuf::from("/tmp/pilot-x/run"));
    }

    #[test]
    fn tmux_socket_strips_leading_dot_on_default_profile() {
        // Backward compat: pre-PILOT_HOME, sessions were stored
        // under `tmux -L pilot`. Default profile must still resolve
        // to "pilot" so an in-flight session survives the upgrade.
        let _g1 = EnvGuard::set("PILOT_HOME", "/Users/test/.pilot");
        assert_eq!(tmux_socket_name(), "pilot");
    }

    #[test]
    fn tmux_socket_disambiguates_dev_profile() {
        let _g1 = EnvGuard::set("PILOT_HOME", "/Users/test/.pilot-dev");
        assert_eq!(tmux_socket_name(), "pilot-dev");
    }

    #[test]
    fn tmux_socket_handles_non_dotted_profile() {
        // Users sometimes point PILOT_HOME at a non-dotfile path
        // (e.g. for testing). No leading dot to strip; pass the name
        // through.
        let _g1 = EnvGuard::set("PILOT_HOME", "/tmp/sandbox/profile-a");
        assert_eq!(tmux_socket_name(), "profile-a");
    }

    #[test]
    fn tmux_socket_falls_back_when_path_has_no_name() {
        // PILOT_HOME=/ is degenerate but mustn't crash. Fallback
        // to "pilot" so callers don't have to handle None.
        let _g1 = EnvGuard::set("PILOT_HOME", "/");
        assert_eq!(tmux_socket_name(), "pilot");
    }
}
