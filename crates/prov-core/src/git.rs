//! Typed wrapper around shelling out to `git`.
//!
//! Why shell out instead of `git2-rs` or `gitoxide`: the most expensive operations
//! (notes read/write, push/fetch, blame) all happen inside git hooks, where the
//! environment already has the user's full git config and credential helpers.
//! Inheriting that environment is essentially free; reimplementing it is not.
//! `gitoxide` also explicitly lacks hook/push/full-merge coverage as of 2026, and
//! `git2-rs` adds a C dependency that complicates static musl builds.
//!
//! This module is intentionally thin: each method is one `git` invocation.
//! Higher-level orchestration (e.g., notes-merge resolution, post-commit
//! diff-and-match) lives in `storage::notes` or in `prov-cli`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A git repo root resolved at construction time.
///
/// All operations run with `--git-dir` and `--work-tree` pointing at the root,
/// so the wrapper is robust to the caller's cwd.
#[derive(Debug, Clone)]
pub struct Git {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl Git {
    /// Resolve `git rev-parse --git-dir` from the given working directory.
    ///
    /// Returns `Err(GitError::NotARepo)` when not inside a git working tree.
    /// Capture-pipeline hooks rely on this to silently no-op when Claude Code
    /// is invoked outside any repo.
    pub fn discover<P: AsRef<Path>>(cwd: P) -> Result<Self, GitError> {
        let cwd = cwd.as_ref();
        let git_dir = run_capture_in(cwd, ["rev-parse", "--git-dir"])?;
        let work_tree = run_capture_in(cwd, ["rev-parse", "--show-toplevel"])?;

        let git_dir = PathBuf::from(git_dir.trim());
        let work_tree = PathBuf::from(work_tree.trim());

        // git_dir may be returned as a relative path; resolve it against cwd.
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            cwd.join(git_dir)
        };

        Ok(Self { git_dir, work_tree })
    }

    /// Construct directly from absolute paths. Primarily for tests.
    #[must_use]
    pub fn from_paths(git_dir: PathBuf, work_tree: PathBuf) -> Self {
        Self { git_dir, work_tree }
    }

    /// Absolute path of the `.git` directory.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Absolute path of the working tree root.
    #[must_use]
    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    /// Run `git <args>` in this repo and return stdout as `String`.
    ///
    /// Errors carry stderr for diagnostics. Use this for read operations where
    /// you need the output.
    pub fn capture<I, S>(&self, args: I) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command().args(args).output().map_err(|e| io(&e))?;
        check_success(&output)?;
        String::from_utf8(output.stdout).map_err(|e| GitError::NonUtf8(e.to_string()))
    }

    /// Run `git <args>` and return raw stdout bytes (for binary content like blobs).
    pub fn capture_bytes<I, S>(&self, args: I) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command().args(args).output().map_err(|e| io(&e))?;
        check_success(&output)?;
        Ok(output.stdout)
    }

    /// Run `git <args>` and discard stdout. Use for write operations where you
    /// only care about success/failure.
    pub fn run<I, S>(&self, args: I) -> Result<(), GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command().args(args).output().map_err(|e| io(&e))?;
        check_success(&output)
    }

    /// Run `git <args>` and pipe `stdin_bytes` into the child's stdin.
    /// Returns stdout. Used for `git hash-object --stdin`, `git notes add --stdin`, etc.
    pub fn capture_with_stdin<I, S>(&self, args: I, stdin_bytes: &[u8]) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io(&e))?;

        if let Some(mut sin) = child.stdin.take() {
            sin.write_all(stdin_bytes).map_err(|e| io(&e))?;
        }
        let output = child.wait_with_output().map_err(|e| io(&e))?;
        check_success(&output)?;
        String::from_utf8(output.stdout).map_err(|e| GitError::NonUtf8(e.to_string()))
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree);
        cmd
    }
}

fn run_capture_in<P, I, S>(cwd: P, args: I) -> Result<String, GitError>
where
    P: AsRef<Path>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| io(&e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            return Err(GitError::NotARepo);
        }
        return Err(GitError::CommandFailed {
            status: output.status.code(),
            stderr: stderr.into_owned(),
        });
    }
    String::from_utf8(output.stdout).map_err(|e| GitError::NonUtf8(e.to_string()))
}

fn check_success(output: &Output) -> Result<(), GitError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(GitError::CommandFailed {
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn io(e: &std::io::Error) -> GitError {
    GitError::Io(e.to_string())
}

/// Errors raised by `git` invocations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// `git rev-parse --git-dir` failed because the path is not inside a git repo.
    /// Hooks treat this as a silent no-op signal.
    #[error("not inside a git repository")]
    NotARepo,
    /// A `git` invocation returned a non-zero exit code.
    #[error("git command failed (status {status:?}): {stderr}")]
    CommandFailed {
        /// Process exit status.
        status: Option<i32>,
        /// Captured stderr from the git invocation.
        stderr: String,
    },
    /// `git` produced non-UTF-8 output where text was expected.
    #[error("git output was not valid UTF-8: {0}")]
    NonUtf8(String),
    /// Failed to spawn the `git` process at all.
    #[error("git invocation I/O error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_test_repo() -> (TempDir, Git) {
        let dir = TempDir::new().expect("tempdir");
        // Use the bare `git` from PATH, isolating from the user's global config.
        let status = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg("-b")
            .arg("main")
            .arg(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git init");
        assert!(status.success());

        // Set local user config so commits work.
        for (k, v) in [("user.email", "test@example.com"), ("user.name", "Test")] {
            Command::new("git")
                .current_dir(dir.path())
                .args(["config", "--local", k, v])
                .status()
                .unwrap();
        }

        let git = Git::discover(dir.path()).expect("discover");
        (dir, git)
    }

    #[test]
    fn discover_in_fresh_repo() {
        let (_dir, git) = init_test_repo();
        assert!(git.git_dir().exists());
        assert!(git.work_tree().exists());
    }

    #[test]
    fn discover_outside_repo_returns_not_a_repo() {
        let dir = TempDir::new().unwrap();
        match Git::discover(dir.path()) {
            Err(GitError::NotARepo) => {}
            other => panic!("expected NotARepo, got {other:?}"),
        }
    }

    #[test]
    fn capture_runs_command() {
        let (_dir, git) = init_test_repo();
        let branch = git.capture(["symbolic-ref", "--short", "HEAD"]).unwrap();
        assert_eq!(branch.trim(), "main");
    }

    #[test]
    fn capture_with_stdin_pipes_input() {
        let (_dir, git) = init_test_repo();
        let sha = git
            .capture_with_stdin(["hash-object", "--stdin", "-w"], b"hello prov\n")
            .unwrap();
        assert_eq!(sha.trim().len(), 40); // SHA-1 hex
        let content = git.capture_bytes(["cat-file", "blob", sha.trim()]).unwrap();
        assert_eq!(&content, b"hello prov\n");
    }

    #[test]
    fn run_propagates_failure() {
        let (_dir, git) = init_test_repo();
        match git.run(["this-is-not-a-command"]) {
            Err(GitError::CommandFailed { .. }) => {}
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }
}
