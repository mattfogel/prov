//! `prov uninstall` — reverse `prov install`.
//!
//! Removes the prov-managed `# >>> prov` / `# <<< prov` block from any hook
//! script, strips prov hook entries from `.claude/settings.json`, unsets the
//! git config keys `prov install` wrote, and removes any prov fetch refspec
//! across all remotes. Notes ref and cache file are preserved unless
//! `--purge` is passed (then `.git/prov.db` and `.git/prov-staging/` are
//! deleted; the notes ref is intentionally left alone — local provenance
//! belongs to the user).
//!
//! Idempotent: re-running `prov uninstall` against a partially-uninstalled
//! repo, or against a never-installed repo, is a no-op.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context};
use clap::Parser;
use serde_json::{Map, Value};

use prov_core::git::Git;

use super::common::CACHE_FILENAME;
use super::install::{claude_settings_path, HOOK_BLOCK_BEGIN, HOOK_BLOCK_END};

#[derive(Parser, Debug)]
pub struct Args {
    /// Also delete `.git/prov.db` and `.git/prov-staging/`. Notes ref is preserved.
    #[arg(long)]
    pub purge: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        prov_core::git::GitError::NotARepo => anyhow!("not in a git repo"),
        other => other.into(),
    })?;

    let hook_path = git.git_dir().join("hooks").join("post-commit");
    uninstall_hook(&hook_path).with_context(|| format!("uninstalling {}", hook_path.display()))?;

    uninstall_claude_settings(&git).context("removing prov entries from .claude/settings.json")?;

    unset_git_config(&git);
    remove_prov_fetch_refspecs(&git);

    if args.purge {
        let cache_path = git.git_dir().join(CACHE_FILENAME);
        if cache_path.exists() {
            fs::remove_file(&cache_path)
                .with_context(|| format!("removing {}", cache_path.display()))?;
        }
        let staging_path = git.git_dir().join("prov-staging");
        if staging_path.exists() {
            fs::remove_dir_all(&staging_path)
                .with_context(|| format!("removing {}", staging_path.display()))?;
        }
    }

    println!("prov: uninstalled from {}", git.work_tree().display());
    if !args.purge {
        println!("  cache + staging preserved (use `prov uninstall --purge` to delete)");
    }
    Ok(())
}

// -------- hook --------

fn uninstall_hook(path: &Path) -> anyhow::Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let cleaned = remove_hook_block(&existing);
    if hook_is_empty_shell(&cleaned) {
        fs::remove_file(path)?;
    } else {
        fs::write(path, cleaned)?;
    }
    Ok(())
}

/// Strip the `# >>> prov` … `# <<< prov` block from a hook script if present.
fn remove_hook_block(src: &str) -> String {
    let Some(start) = src.find(HOOK_BLOCK_BEGIN) else {
        return src.to_string();
    };
    let Some(end_offset) = src[start..].find(HOOK_BLOCK_END) else {
        return src.to_string();
    };
    let absolute_end = start + end_offset + HOOK_BLOCK_END.len();
    let trailing = src[absolute_end..]
        .strip_prefix('\n')
        .unwrap_or(&src[absolute_end..]);
    let mut out = String::new();
    out.push_str(&src[..start]);
    out.push_str(trailing);
    out
}

/// True when the hook contains only a shebang or whitespace — no user content
/// remained after the prov block was removed, so the file may be deleted.
fn hook_is_empty_shell(s: &str) -> bool {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .all(|l| l.starts_with("#!"))
}

// -------- .claude/settings.json --------

fn uninstall_claude_settings(git: &Git) -> anyhow::Result<()> {
    let path = claude_settings_path(git);
    let existing = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut root: Map<String, Value> = serde_json::from_str(&existing)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    let removed_any = strip_prov_entries(&mut root);
    if !removed_any {
        return Ok(());
    }

    if root.is_empty() {
        fs::remove_file(&path)
            .with_context(|| format!("removing {}", path.display()))?;
    } else {
        fs::write(
            &path,
            serde_json::to_string_pretty(&Value::Object(root))? + "\n",
        )?;
    }
    Ok(())
}

/// Walk a settings JSON `Map`, dropping every hook entry whose command starts
/// with `prov hook`. Returns `true` if anything was removed (so the caller
/// only rewrites the file when needed).
fn strip_prov_entries(root: &mut Map<String, Value>) -> bool {
    let Some(hooks_value) = root.get_mut("hooks") else {
        return false;
    };
    let Some(hooks_obj) = hooks_value.as_object_mut() else {
        return false;
    };

    let mut removed = false;
    let event_keys: Vec<String> = hooks_obj.keys().cloned().collect();
    for event in event_keys {
        let Some(arr_value) = hooks_obj.get_mut(&event) else {
            continue;
        };
        let Some(arr) = arr_value.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|cmd| cmd.starts_with("prov hook"))
        });
        if arr.len() != before {
            removed = true;
        }
        if arr.is_empty() {
            hooks_obj.remove(&event);
        }
    }

    if hooks_obj.is_empty() {
        root.remove("hooks");
    }
    removed
}

// -------- git config --------

fn unset_git_config(git: &Git) {
    // Unset is best-effort: keys that aren't set return non-zero, which we ignore.
    for key in [
        "notes.displayRef",
        "notes.rewrite.amend",
        "notes.rewrite.rebase",
        "notes.mergeStrategy",
    ] {
        let _ = git.run(["config", "--local", "--unset", key]);
    }
}

fn remove_prov_fetch_refspecs(git: &Git) {
    let prov_refspec = "refs/notes/prompts:refs/notes/origin/prompts";
    let Ok(remotes_raw) = git.capture(["remote"]) else {
        return;
    };
    for remote in remotes_raw.lines().filter(|l| !l.is_empty()) {
        let key = format!("remote.{remote}.fetch");
        let _ = git.run([
            "config",
            "--local",
            "--unset",
            &key,
            &format!("^{}$", regex_escape(prov_refspec)),
        ]);
    }
}

/// Quote regex metacharacters in a literal string so `git config --unset`'s
/// value-pattern matches the exact refspec.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '\\' | '^' | '$' | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_hook_block_strips_prov_section() {
        let src = "#!/bin/sh\necho user\n# >>> prov\necho prov\n# <<< prov\necho tail\n";
        let cleaned = remove_hook_block(src);
        assert_eq!(cleaned, "#!/bin/sh\necho user\necho tail\n");
    }

    #[test]
    fn remove_hook_block_returns_input_when_no_block() {
        let src = "#!/bin/sh\necho hi\n";
        assert_eq!(remove_hook_block(src), src);
    }

    #[test]
    fn hook_is_empty_shell_treats_shebang_only_as_empty() {
        assert!(hook_is_empty_shell("#!/bin/sh\n"));
        assert!(hook_is_empty_shell("#!/bin/bash\n\n"));
        assert!(!hook_is_empty_shell("#!/bin/sh\necho hi\n"));
    }

    #[test]
    fn strip_prov_entries_removes_only_prov_commands() {
        let mut root: Map<String, Value> = serde_json::from_str(
            r#"{
                "hooks": {
                    "PostToolUse": [
                        {"command": "echo user"},
                        {"command": "prov hook post-tool-use"}
                    ]
                }
            }"#,
        )
        .unwrap();
        let removed = strip_prov_entries(&mut root);
        assert!(removed);
        let arr = root["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["command"], "echo user");
    }

    #[test]
    fn strip_prov_entries_drops_empty_event_keys() {
        let mut root: Map<String, Value> = serde_json::from_str(
            r#"{"hooks":{"Stop":[{"command":"prov hook stop"}]}}"#,
        )
        .unwrap();
        let removed = strip_prov_entries(&mut root);
        assert!(removed);
        // Hooks object becomes empty → root drops it entirely.
        assert!(!root.contains_key("hooks"));
    }

    #[test]
    fn strip_prov_entries_idempotent_when_nothing_to_remove() {
        let mut root: Map<String, Value> =
            serde_json::from_str(r#"{"hooks":{"Stop":[{"command":"echo other"}]}}"#).unwrap();
        let removed = strip_prov_entries(&mut root);
        assert!(!removed);
    }

    #[test]
    fn regex_escape_quotes_metachars() {
        assert_eq!(
            regex_escape("refs/notes/prompts:refs/notes/origin/prompts"),
            r"refs\/notes\/prompts:refs\/notes\/origin\/prompts"
        );
    }
}
