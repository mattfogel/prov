//! SKILL content lints.
//!
//! Validates the on-disk shape of `skills/prov/`: frontmatter has the
//! required keys, the body fits under the 500-line cap, the two reference
//! docs exist and are linked from `SKILL.md`, and the manual smoke-test plan
//! is committed alongside.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root resolves from prov-cli manifest dir")
        .to_path_buf()
}

fn skill_dir() -> PathBuf {
    workspace_root().join("skills").join("prov")
}

/// Splits a SKILL.md-style file into (frontmatter, body). Frontmatter is the
/// content between the first two `---` markers; the body is everything after
/// the second marker. Returns `None` if the file lacks the expected delimiters.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    Some((frontmatter, body))
}

#[test]
fn skill_md_has_required_frontmatter() {
    let skill_path = skill_dir().join("SKILL.md");
    let raw = std::fs::read_to_string(&skill_path).unwrap_or_else(|e| {
        panic!("expected {} to exist: {e}", skill_path.display());
    });

    let (frontmatter, _body) = split_frontmatter(&raw).unwrap_or_else(|| {
        panic!(
            "SKILL.md at {} must start with `---\\n<frontmatter>\\n---\\n<body>`",
            skill_path.display()
        )
    });

    // Required keys: `name:` and `description:`. Both must be non-empty. We
    // don't pull in a YAML parser for this — line-prefix matching is enough
    // for the SKILL convention (single-line scalar values for these keys).
    let mut name_value: Option<&str> = None;
    let mut description_value: Option<&str> = None;
    for line in frontmatter.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name_value = Some(v.trim().trim_matches('"').trim_matches('\''));
        } else if let Some(v) = line.strip_prefix("description:") {
            description_value = Some(v.trim().trim_matches('"').trim_matches('\''));
        }
    }

    let name = name_value.expect("SKILL.md frontmatter missing `name:`");
    assert!(!name.is_empty(), "SKILL.md `name:` must be non-empty");
    assert_eq!(
        name, "prov",
        "SKILL.md `name:` must be `prov` (matches plugin name and binary)"
    );

    let description = description_value.expect("SKILL.md frontmatter missing `description:`");
    assert!(
        !description.is_empty(),
        "SKILL.md `description:` must be non-empty"
    );
    // Trigger-rich descriptions are the lever for skill activation; a one-line
    // description is too short to fire reliably across diverse phrasings.
    assert!(
        description.len() >= 200,
        "SKILL.md `description:` is the trigger surface — keep it long enough \
         (at least 200 chars) to cover varied user phrasings. Current: {} chars",
        description.len()
    );
}

#[test]
fn skill_md_body_is_under_500_lines() {
    let skill_path = skill_dir().join("SKILL.md");
    let raw = std::fs::read_to_string(&skill_path).expect("SKILL.md must exist");
    let (_frontmatter, body) = split_frontmatter(&raw).expect("SKILL.md must have frontmatter");
    let line_count = body.lines().count();
    assert!(
        line_count <= 500,
        "SKILL.md body is {line_count} lines; the cap is 500. Move long-form \
         content into references/ files instead."
    );
}

#[test]
fn skill_references_exist_and_are_linked() {
    let skill_path = skill_dir().join("SKILL.md");
    let body = std::fs::read_to_string(&skill_path).expect("SKILL.md must exist");

    for reference in ["references/querying.md", "references/triggers.md"] {
        let path = skill_dir().join(reference);
        assert!(
            path.exists(),
            "SKILL reference {} must exist on disk",
            path.display()
        );
        // The SKILL body should mention each reference by name so the agent
        // knows when to follow the link.
        let stem = Path::new(reference).file_name().unwrap().to_string_lossy();
        assert!(
            body.contains(&*stem),
            "SKILL.md must reference `{stem}` so the agent loads it when relevant"
        );
    }
}

#[test]
fn skill_smoke_test_plan_exists() {
    let smoke = skill_dir().join("tests").join("skill_smoke.md");
    assert!(
        smoke.exists(),
        "manual smoke test plan at {} must exist — it's the load-bearing \
         verification for the skill's trigger fidelity",
        smoke.display()
    );
}
