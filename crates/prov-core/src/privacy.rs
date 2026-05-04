//! Privacy-routing predicates shared by capture and backfill.
//!
//! A prompt routes to the local-only `refs/notes/prompts-private` ref when it
//! opts out via the `# prov:private` magic phrase on its first or last line.
//! Both the live hook (`prov hook user-prompt-submit`) and `prov backfill` must
//! honor this same predicate so a private prompt reconstructed from a transcript
//! never lands on the pushable public ref.

/// True when the prompt's first or last line is the magic phrase
/// `# prov:private` (case-insensitive). Restricted to first/last lines so a
/// paste of code that contains `# prov:private` inside a code block does not
/// silently flip the privacy bit.
#[must_use]
pub fn is_prov_private(prompt: &str) -> bool {
    let lines: Vec<&str> = prompt.lines().collect();
    if lines.first().is_some_and(|l| line_is_prov_private(l)) {
        return true;
    }
    lines.last().is_some_and(|l| line_is_prov_private(l))
}

fn line_is_prov_private(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix('#') else {
        return false;
    };
    rest.trim().eq_ignore_ascii_case("prov:private")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_or_last_line_marker_routes_private() {
        assert!(is_prov_private("# prov:private\nfoo bar"));
        assert!(is_prov_private("foo\n# prov:private"));
        assert!(is_prov_private("# Prov:Private\nfoo"));
        assert!(is_prov_private("# PROV:PRIVATE"));
        assert!(is_prov_private("foo\n# PROV:PRIVATE"));
    }

    #[test]
    fn marker_in_middle_does_not_route_private() {
        assert!(!is_prov_private("foo\n# prov:private\nbar"));
        assert!(!is_prov_private("write a parser for # prov:private syntax"));
    }

    #[test]
    fn empty_or_unmarked_prompts_are_public() {
        assert!(!is_prov_private(""));
        assert!(!is_prov_private("just a normal prompt"));
        assert!(!is_prov_private("# some other tag"));
    }
}
