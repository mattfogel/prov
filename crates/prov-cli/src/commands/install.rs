//! `prov install` — idempotent project-scope installer.
//!
//! Wires Prov into the current repo:
//! - Sets `notes.displayRef`, disables git's built-in note rewriting
//!   (`notes.rewrite.amend=false`, `notes.rewrite.rebase=false`), and selects
//!   `notes.mergeStrategy=manual` (U10 owns the resolver).
//! - Installs `.git/hooks/post-commit` inside a `# >>> prov` / `# <<< prov`
//!   delimiter block so prov composes with any user-authored hook content.
//! - Adds prov's Claude Code hook entries to `.claude/settings.json`.
//! - Initializes `<git-dir>/prov.db` and runs an initial reindex.
//!
//! `--plugin` is a stub (pre-v1: no marketplace listing); it prints the
//! current-best install path and exits without modifying the repo.
//! `--enable-push <REMOTE>` opts into team mode by adding the notes-tracking
//! fetch refspec for the named remote. The pre-push gate that R6 promises
//! ships in U8; until then `--enable-push` documents itself as "fetch only".
//!
//! All filesystem writes are idempotent: re-running `prov install` produces
//! the same on-disk state and reports "already installed" without
//! duplicating config, hook blocks, or settings entries.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use clap::Parser;
use serde_json::{Map, Value};

use prov_core::git::Git;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::{notes::NotesStore, NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

use super::common::CACHE_FILENAME;

/// Embedded post-commit hook template. Source: `githooks/post-commit`.
const POST_COMMIT_TEMPLATE: &str = include_str!("../../../../githooks/post-commit");

/// Embedded pre-push hook template. Source: `githooks/pre-push`. Owns the U8
/// secret-scanning gate that fires before notes (or, with
/// `prov.scanAllPushes`, any push) reach the wire.
const PRE_PUSH_TEMPLATE: &str = include_str!("../../../../githooks/pre-push");

/// Embedded plugin/hooks/hooks.json so `--plugin`-less installs can mirror the
/// plugin's hook entries into project-scope `.claude/settings.json`.
const PLUGIN_HOOKS_JSON: &str = include_str!("../../../../plugin/hooks/hooks.json");

/// Sentinel pair that scopes prov's chained content inside any shared hook.
pub const HOOK_BLOCK_BEGIN: &str = "# >>> prov";
/// End of the prov-managed block.
pub const HOOK_BLOCK_END: &str = "# <<< prov";

#[derive(Parser, Debug)]
pub struct Args {
    /// Print the (currently pre-v1) plugin install instructions and exit
    /// without modifying the repo.
    #[arg(long)]
    pub plugin: bool,
    /// Enable team-mode sync at install time (configures the fetch refspec for
    /// the named remote). Defaults to local-only — sync is opt-in per-repo.
    #[arg(long, value_name = "REMOTE")]
    pub enable_push: Option<String>,
}

pub fn run(args: Args) -> anyhow::Result<()> {
    if args.plugin {
        print_plugin_instructions();
        return Ok(());
    }

    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        prov_core::git::GitError::NotARepo => anyhow!("not in a git repo"),
        other => other.into(),
    })?;

    configure_git(&git).context("setting prov's git config")?;

    let hook_path = git.git_dir().join("hooks").join("post-commit");
    install_hook(&hook_path, POST_COMMIT_TEMPLATE)
        .with_context(|| format!("installing {}", hook_path.display()))?;

    let pre_push_path = git.git_dir().join("hooks").join("pre-push");
    install_hook(&pre_push_path, PRE_PUSH_TEMPLATE)
        .with_context(|| format!("installing {}", pre_push_path.display()))?;

    install_claude_settings(&git).context("merging prov entries into .claude/settings.json")?;

    if let Some(remote) = args.enable_push.as_deref() {
        configure_remote_refspec(&git, remote)
            .with_context(|| format!("configuring fetch refspec for remote {remote}"))?;
    }

    let cache_path = git.git_dir().join(CACHE_FILENAME);
    initialize_cache(&git, &cache_path).context("initializing the prov SQLite cache")?;

    println!("prov: installed in {}", git.work_tree().display());
    println!("  hooks:    {}", hook_path.display());
    println!("  hooks:    {}", pre_push_path.display());
    println!("  cache:    {}", cache_path.display());
    println!("  settings: {}", claude_settings_path(&git).display());
    if let Some(remote) = args.enable_push {
        println!(
            "  push:     fetch refspec configured for `{remote}` — pre-push secret gate active"
        );
    } else {
        println!("  push:     local-only (use `prov install --enable-push <remote>` to opt in)");
    }
    Ok(())
}

fn print_plugin_instructions() {
    println!("Claude Code plugin install (pre-v1):");
    println!();
    println!("  Marketplace listing: not yet published.");
    println!();
    println!("  Until then, install the prov binary (cargo / Homebrew / curl|sh)");
    println!("  and run `prov install` inside each repo to wire hooks and config.");
}

// -------- git config --------

fn configure_git(git: &Git) -> anyhow::Result<()> {
    // The mergeStrategy default lives at `notes.mergeStrategy`; per-ref overrides
    // (`notes.<ref>.mergeStrategy`) are not used in v1 because U10 ships a single
    // resolver across both `prompts` and `prompts-private`.
    set_config(git, "notes.displayRef", NOTES_REF_PUBLIC)?;
    set_config(git, "notes.rewrite.amend", "false")?;
    set_config(git, "notes.rewrite.rebase", "false")?;
    set_config(git, "notes.mergeStrategy", "manual")?;
    Ok(())
}

fn set_config(git: &Git, key: &str, value: &str) -> anyhow::Result<()> {
    git.run(["config", "--local", key, value])
        .with_context(|| format!("git config --local {key} {value}"))?;
    Ok(())
}

fn configure_remote_refspec(git: &Git, remote: &str) -> anyhow::Result<()> {
    let refspec = "refs/notes/prompts:refs/notes/origin/prompts";
    let key = format!("remote.{remote}.fetch");

    // Idempotency: read existing refspecs and skip if already present.
    let existing = git
        .capture(["config", "--local", "--get-all", &key])
        .unwrap_or_default();
    if existing.lines().any(|l| l.trim() == refspec) {
        return Ok(());
    }
    git.run(["config", "--local", "--add", &key, refspec])?;
    Ok(())
}

// -------- post-commit hook --------

fn install_hook(path: &Path, prov_body: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let prov_block = render_hook_block(prov_body);
    let new_contents = match fs::read_to_string(path) {
        Ok(existing) => merge_hook_block(&existing, &prov_block),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => format!("#!/bin/sh\n{prov_block}"),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    atomic_write(path, new_contents.as_bytes())?;
    set_executable(path)?;
    Ok(())
}

fn render_hook_block(body: &str) -> String {
    // Strip a leading `#!/bin/sh` from the embedded template body — when we
    // chain into an existing hook, only the head of the file should carry the
    // shebang. The standalone-write path adds a fresh shebang above the block.
    let body = body
        .lines()
        .skip_while(|l| l.starts_with("#!"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{HOOK_BLOCK_BEGIN}\n{body}\n{HOOK_BLOCK_END}\n")
}

/// Insert or replace the prov-managed block in an existing hook script.
fn merge_hook_block(existing: &str, prov_block: &str) -> String {
    if let (Some(start), Some(end)) = (
        existing.find(HOOK_BLOCK_BEGIN),
        existing.find(HOOK_BLOCK_END),
    ) {
        let end_with_newline = end + HOOK_BLOCK_END.len();
        let trailing = existing[end_with_newline..]
            .strip_prefix('\n')
            .unwrap_or(&existing[end_with_newline..]);
        let mut out = String::new();
        out.push_str(&existing[..start]);
        out.push_str(prov_block);
        out.push_str(trailing);
        return out;
    }
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(prov_block);
    out
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    let mode = perms.mode();
    perms.set_mode(mode | 0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

// -------- .claude/settings.json --------

pub(crate) fn claude_settings_path(git: &Git) -> PathBuf {
    git.work_tree().join(".claude").join("settings.json")
}

fn install_claude_settings(git: &Git) -> anyhow::Result<()> {
    let plugin_hooks: Value = serde_json::from_str(PLUGIN_HOOKS_JSON)
        .context("embedded plugin/hooks/hooks.json failed to parse")?;
    let plugin_hooks_obj = plugin_hooks
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("embedded plugin hooks JSON missing top-level `hooks` object"))?
        .clone();

    let path = claude_settings_path(git);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut root: Map<String, Value> = match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| {
            format!(
                "{} is not valid JSON; remove or fix it before running `prov install`",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let hooks_value = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = hooks_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` in {} must be an object", path.display()))?;

    for (event, plugin_entries) in &plugin_hooks_obj {
        let plugin_entries_arr = plugin_entries
            .as_array()
            .ok_or_else(|| anyhow!("plugin hook event `{event}` must be a JSON array"))?;
        let user_entries = hooks_obj
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow!("`hooks.{event}` in {} must be an array", path.display()))?;

        // Drop any prior prov-owned entries before re-inserting, so re-running
        // install doesn't duplicate. We match either the v1 entry shape
        // (`entry.hooks[].command` per Claude Code's schema) or the legacy
        // shape we shipped briefly with `command` at the entry top level — the
        // legacy form was rejected by Claude Code at session start, so this
        // self-heals an already-broken settings.json on re-install.
        user_entries.retain(|entry| !is_prov_owned_entry(entry));
        for entry in plugin_entries_arr {
            user_entries.push(entry.clone());
        }
    }

    write_pretty_json(&path, &Value::Object(root))?;
    Ok(())
}

/// True if the settings.json hook-entry block is owned by prov, in either the
/// current Claude Code schema (commands nested under `entry.hooks[]`) or the
/// legacy shape we briefly shipped (command at entry top level).
pub(crate) fn is_prov_owned_entry(entry: &Value) -> bool {
    if entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|cmd| cmd.starts_with("prov hook"))
    {
        return true;
    }
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .any(|cmd| cmd.starts_with("prov hook"))
}

fn write_pretty_json(path: &Path, value: &Value) -> anyhow::Result<()> {
    let mut buf = serde_json::to_vec_pretty(value)?;
    buf.push(b'\n');
    atomic_write(path, &buf)?;
    Ok(())
}

/// Write `bytes` to `path` atomically: write to `<path>.tmp`, fsync, then
/// rename into place. A killed mid-write process leaves at most a `.tmp` file
/// rather than truncating the real target.
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp_path = match path.file_name() {
        Some(name) => {
            let mut tmp = name.to_os_string();
            tmp.push(".tmp");
            path.with_file_name(tmp)
        }
        None => return Err(anyhow!("invalid target path: {}", path.display())),
    };
    {
        let mut f = fs::File::create(&tmp_path)
            .with_context(|| format!("creating {}", tmp_path.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} → {}", tmp_path.display(), path.display()))?;
    Ok(())
}

// -------- cache initialization --------

fn initialize_cache(git: &Git, cache_path: &Path) -> anyhow::Result<()> {
    let mut cache =
        Cache::open(cache_path).with_context(|| format!("opening {}", cache_path.display()))?;
    let public = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    let private = NotesStore::new(git.clone(), NOTES_REF_PRIVATE);
    let _ = cache.reindex_from(&public)?;
    let _ = cache.overlay_from(&private)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_hook_block_strips_shebang() {
        let body = "#!/bin/sh\necho hi\n";
        let block = render_hook_block(body);
        assert!(block.starts_with(HOOK_BLOCK_BEGIN));
        assert!(block.contains("echo hi"));
        assert!(!block.contains("#!/bin/sh"));
        assert!(block.contains(HOOK_BLOCK_END));
    }

    #[test]
    fn merge_hook_block_appends_to_user_hook() {
        let existing = "#!/bin/sh\necho user-hook\n";
        let prov = "# >>> prov\necho prov\n# <<< prov\n";
        let merged = merge_hook_block(existing, prov);
        assert!(merged.contains("echo user-hook"));
        assert!(merged.contains("echo prov"));
    }

    #[test]
    fn merge_hook_block_replaces_prior_prov_block() {
        let existing = "#!/bin/sh\necho user\n# >>> prov\necho old\n# <<< prov\necho tail\n";
        let prov = "# >>> prov\necho new\n# <<< prov\n";
        let merged = merge_hook_block(existing, prov);
        assert!(merged.contains("echo user"));
        assert!(merged.contains("echo new"));
        assert!(!merged.contains("echo old"));
        assert!(merged.contains("echo tail"));
    }

    #[test]
    fn merge_hook_block_idempotent() {
        let prov = "# >>> prov\necho prov\n# <<< prov\n";
        let first = merge_hook_block("#!/bin/sh\n", prov);
        let second = merge_hook_block(&first, prov);
        assert_eq!(first, second);
    }
}
