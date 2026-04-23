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

        // Fetch the branch.
        run_git_in(
            &bare_path,
            &["fetch", "origin", &format!("{branch}:{branch}")],
        )
        .await?;

        // Create worktree.
        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        run_git_in(
            &bare_path,
            &["worktree", "add", &wt_path.to_string_lossy(), branch],
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

        // Fetch the base branch's tip into FETCH_HEAD, WITHOUT updating the
        // local ref. Using `base:base` as the refspec fails with "refusing
        // to fetch into branch X checked out at <path>" when another
        // worktree (another pilot session) has that same base checked out.
        // FETCH_HEAD sidesteps the constraint — we just need the commit,
        // not a local branch.
        run_git_in(&bare_path, &["fetch", "origin", base_branch]).await?;

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // `git worktree add -b <new> <path> FETCH_HEAD` creates the branch
        // off the just-fetched tip without requiring `base` to exist as a
        // local ref.
        run_git_in(
            &bare_path,
            &[
                "worktree",
                "add",
                "-b",
                new_branch,
                &wt_path.to_string_lossy(),
                "FETCH_HEAD",
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
