# Triggers — when is the change substantive enough to query?

Heuristics for deciding whether to call `prov log` before proposing an edit.
Use this when the user's ask sits in the gray zone between "trivial" and
"substantive."

## Default rule of thumb

Query provenance if **all three** are true:

1. The file already exists with non-trivial content (more than ~10 lines).
2. The change touches behavior, structure, or public surface — not pure
   cosmetics.
3. The user's request would change how the code *works*, not just how it
   *looks*.

If any of the three is false, skip the query.

## Substantive — query first

These changes warrant a `prov log` call before proposing code:

- **Refactor** — extract function, inline, rename across call sites,
  reorganize control flow, change a class hierarchy.
- **Rewrite** — replace a block with a different implementation of the same
  behavior.
- **Behavior change** — change a default, threshold, retry policy, validation
  rule, error strategy, timeout, cache key, or any value/condition that
  alters runtime behavior.
- **Public surface change** — modify a function signature, add/remove a
  parameter, change an exported type, alter an API contract.
- **Debugging an existing bug** — when the user asks "why does this happen"
  and the answer might be encoded in the original prompt's constraints
  (e.g., a deliberate trade-off the prompt called for).
- **Adding error handling around AI-written code** — the original error
  strategy may already exist; a new layer can conflict with retry middleware,
  fallback logic, or framework error handling.
- **Adding a new branch to existing logic** — branches compose with the
  control flow the prompt shaped; the prompt may have explicitly excluded
  the branch you're adding.

## Trivial — skip the query

These changes do not warrant a query:

- **Typo fixes** — single-character or single-word corrections.
- **Comment edits** — adding, removing, or rewording comments without
  touching code.
- **Formatting** — whitespace, indentation, line wrapping, import sorting.
- **Lint fixes** — applying a linter rule (unused import removal, prefer-
  const, etc.) where the rule itself dictates the change.
- **Pure rename of a single local variable** — when the rename doesn't
  cross file boundaries and doesn't change the public surface.
- **Adding a log line** — `console.log`, `tracing::debug!`, `print` for
  debugging, with no other change.
- **Removing dead code** the user has already identified as unused.
- **Greenfield writes** — new file, new function inserted alongside
  existing ones, new test case in a fresh test block.

## Gray zone — use judgment

When the change is genuinely ambiguous:

- **"Add a test for this function"** — querying the function's prompt may
  reveal what behavior the original turn intended to enforce, which informs
  the test cases. Worth a query if the function has visible business logic;
  skip for pure utility functions.
- **"Fix this bug"** — the bug fix may be trivial (off-by-one, null check)
  or it may require understanding the original constraint. Read the diff the
  user is asking you to fix; if the broken behavior is in AI-written code,
  query.
- **"Make this faster"** — performance changes often preserve behavior, but
  the original prompt may have called out a correctness/perf trade-off
  ("don't cache — staleness matters more than latency"). Worth a query.
- **Configuration changes** — the `paths:` glob excludes `*.json`,
  `*.yaml`, `*.toml`, etc. by default, but if the user asks about a
  config-adjacent code file (e.g., a TS file that builds config), the
  default substantive rules apply.

## Cost of querying when you didn't need to

Low. `prov log --only-if-substantial` returns empty quickly for short or
note-less files. Erring toward "query" is cheap; the cost is one extra
sub-50ms tool call.

## Cost of skipping when you should have queried

High. The agent may reintroduce a bug the original prompt explicitly
prevented, or rewrite a deliberate constraint as if it were an arbitrary
choice. The user typically doesn't know the constraint exists either —
that's why it lives in the prompt rather than a comment.

When in doubt, query.
