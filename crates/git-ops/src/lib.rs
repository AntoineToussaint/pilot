//! # pilot-git-ops
//!
//! Git worktree management. Maintains a base directory with bare clones,
//! creates worktrees per-branch for parallel work.

use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git command failed: {0}")]
    Command(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A handle to a created worktree.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
}

/// Manages git worktrees under a base directory.
///
/// Layout:
/// ```text
/// base_dir/
///   repos/
///     owner/repo.git          (bare clone)
///   worktrees/
///     owner-repo-branch/      (worktree checkout)
/// ```
pub struct WorktreeManager {
    base_dir: PathBuf,
}

impl WorktreeManager {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Default base dir: ~/.pilot/
    pub fn default_base() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self::new(PathBuf::from(home).join(".pilot"))
    }

    fn bare_clone_path(&self, owner: &str, repo: &str) -> PathBuf {
        self.base_dir.join("repos").join(owner).join(format!("{repo}.git"))
    }

    fn worktree_path(&self, owner: &str, repo: &str, branch: &str) -> PathBuf {
        let safe_branch = branch.replace('/', "-");
        self.base_dir
            .join("worktrees")
            .join(format!("{owner}-{repo}-{safe_branch}"))
    }

    /// Ensure a bare clone exists, then create a worktree for the branch.
    /// Idempotent: returns existing worktree if already checked out.
    pub async fn checkout(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Worktree, GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        let wt_path = self.worktree_path(owner, repo, branch);

        // Return early if worktree already exists.
        if wt_path.exists() {
            let name = wt_path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| branch.to_string());
            return Ok(Worktree {
                name,
                path: wt_path,
                branch: branch.into(),
            });
        }

        // Ensure bare clone exists.
        if !bare_path.exists() {
            if let Some(parent) = bare_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let url = format!("git@github.com:{owner}/{repo}.git");
            run_git(&["clone", "--bare", &url, &bare_path.to_string_lossy()]).await?;
        }

        // Try to refresh the remote-tracking ref. We tolerate failure here:
        // if the remote branch was deleted (common right after merge), the
        // fetch drops the remote-tracking ref and we fall back to the local
        // branch below. Target a remote-tracking ref (not refs/heads/*) so
        // we don't collide with another worktree holding the same branch.
        let _ = run_git_in(
            &bare_path,
            &[
                "fetch",
                "origin",
                &format!("+{branch}:refs/remotes/origin/{branch}"),
            ],
        )
        .await;

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Prefer the fresh remote-tracking ref; fall back to the local
        // ref when the remote branch was deleted (e.g. auto-delete after
        // merge). Worst case, `-B` uses whichever commit we have.
        let start_point = if ref_exists(&bare_path, &format!("refs/remotes/origin/{branch}")).await
        {
            format!("refs/remotes/origin/{branch}")
        } else if ref_exists(&bare_path, &format!("refs/heads/{branch}")).await {
            format!("refs/heads/{branch}")
        } else {
            return Err(GitError::Command(format!(
                "branch '{branch}' not found locally or on origin"
            )));
        };
        run_git_in(
            &bare_path,
            &[
                "worktree",
                "add",
                &wt_path.to_string_lossy(),
                "-B",
                branch,
                &start_point,
            ],
        )
        .await?;

        let name = wt_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| branch.to_string());
        Ok(Worktree {
            name,
            path: wt_path,
            branch: branch.into(),
        })
    }

    /// Create a worktree on a *new* branch off `base_branch`.
    /// Used when the user spins up a local task with no PR yet.
    /// Idempotent: returns the existing worktree if it's already there.
    pub async fn checkout_new_branch(
        &self,
        owner: &str,
        repo: &str,
        new_branch: &str,
        base_branch: &str,
    ) -> Result<Worktree, GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        let wt_path = self.worktree_path(owner, repo, new_branch);

        if wt_path.exists() {
            let name = wt_path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| new_branch.to_string());
            return Ok(Worktree {
                name,
                path: wt_path,
                branch: new_branch.into(),
            });
        }

        if !bare_path.exists() {
            if let Some(parent) = bare_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let url = format!("git@github.com:{owner}/{repo}.git");
            run_git(&["clone", "--bare", &url, &bare_path.to_string_lossy()]).await?;
        }

        // Fetch the base's tip. Tolerate failure: if the base was deleted
        // remotely we fall back to the local ref below.
        let _ = run_git_in(
            &bare_path,
            &[
                "fetch",
                "origin",
                &format!("+{base_branch}:refs/remotes/origin/{base_branch}"),
            ],
        )
        .await;

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let start_point = if ref_exists(
            &bare_path,
            &format!("refs/remotes/origin/{base_branch}"),
        )
        .await
        {
            format!("refs/remotes/origin/{base_branch}")
        } else if ref_exists(&bare_path, &format!("refs/heads/{base_branch}")).await {
            format!("refs/heads/{base_branch}")
        } else {
            return Err(GitError::Command(format!(
                "base branch '{base_branch}' not found locally or on origin"
            )));
        };

        run_git_in(
            &bare_path,
            &[
                "worktree",
                "add",
                "-b",
                new_branch,
                &wt_path.to_string_lossy(),
                &start_point,
            ],
        )
        .await?;

        let name = wt_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| new_branch.to_string());
        Ok(Worktree {
            name,
            path: wt_path,
            branch: new_branch.into(),
        })
    }

    /// List all active worktrees.
    pub async fn list(&self) -> Result<Vec<Worktree>, GitError> {
        let wt_dir = self.base_dir.join("worktrees");
        let mut result = Vec::new();
        if !wt_dir.exists() {
            return Ok(result);
        }
        let mut entries = tokio::fs::read_dir(&wt_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                result.push(Worktree {
                    path: entry.path(),
                    branch: name.rsplit_once('-').map(|(_, b)| b).unwrap_or(&name).into(),
                    name,
                });
            }
        }
        Ok(result)
    }

    /// Remove a worktree.
    pub async fn remove(&self, owner: &str, repo: &str, branch: &str) -> Result<(), GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        let wt_path = self.worktree_path(owner, repo, branch);
        if wt_path.exists() {
            run_git_in(
                &bare_path,
                &["worktree", "remove", &wt_path.to_string_lossy(), "--force"],
            )
            .await?;
        }
        Ok(())
    }
}

/// Cheap existence check for a git ref. Uses `show-ref --verify --quiet`;
/// exit 0 = ref exists, non-zero = missing or ambiguous.
async fn ref_exists(bare_path: &Path, ref_name: &str) -> bool {
    Command::new("git")
        .current_dir(bare_path)
        .args(["show-ref", "--verify", "--quiet", ref_name])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn run_git(args: &[&str]) -> Result<String, GitError> {
    let started = std::time::Instant::now();
    tracing::info!("git {}", args.join(" "));
    let output = Command::new("git").args(args).output().await?;
    let elapsed = started.elapsed();
    if output.status.success() {
        tracing::info!("git {} ok ({elapsed:?})", args.join(" "));
        Ok(String::from_utf8_lossy(&output.stdout).into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        tracing::error!(
            "git {} failed ({elapsed:?}): {}",
            args.join(" "),
            stderr.trim()
        );
        Err(GitError::Command(stderr))
    }
}

async fn run_git_in(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let started = std::time::Instant::now();
    tracing::info!("git (in {}) {}", cwd.display(), args.join(" "));
    let output = Command::new("git").current_dir(cwd).args(args).output().await?;
    let elapsed = started.elapsed();
    if output.status.success() {
        tracing::info!(
            "git (in {}) {} ok ({elapsed:?})",
            cwd.display(),
            args.join(" ")
        );
        Ok(String::from_utf8_lossy(&output.stdout).into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        tracing::error!(
            "git (in {}) {} failed ({elapsed:?}): {}",
            cwd.display(),
            args.join(" "),
            stderr.trim()
        );
        Err(GitError::Command(stderr))
    }
}
