/**
 * Sticky-comment upsert for the PR intent timeline.
 *
 * Algorithm:
 *   1. List PR comments.
 *   2. Filter to comments authored by the bot identity tied to the token.
 *   3. Find the one whose body starts with the `<!-- prov:pr-timeline -->`
 *      marker.
 *   4. If found → PATCH it. If not → POST a new comment.
 *
 * Filtering by author *and* marker prevents marker-spoofing: a contributor
 * with PR-comment access cannot pre-place a comment with the marker and
 * have the Action edit it.
 *
 * GitHub's hard cap on issue/PR comments is 65,536 chars; bodies above
 * that get truncated with a `<details>` footer noting the omission, so
 * the Action degrades gracefully on monorepo-scale PRs rather than
 * failing the workflow.
 */

import * as core from '@actions/core';
import { getOctokit } from '@actions/github';

/** HTML marker that scopes the upsert. Must match the Rust renderer's STICKY_MARKER constant. */
export const STICKY_MARKER = '<!-- prov:pr-timeline -->';

/** GitHub's documented upper bound for issue/PR comment bodies. */
export const GITHUB_COMMENT_MAX_CHARS = 65536;

/**
 * Cap on the rendered body we send to GitHub. Below the hard limit so
 * the truncation footer always fits regardless of multi-byte content.
 */
export const SAFE_BODY_LIMIT = 64000;

/** Octokit shape we depend on — narrowed for testability. */
export interface CommentClient {
  rest: {
    issues: {
      listComments(params: {
        owner: string;
        repo: string;
        issue_number: number;
        per_page?: number;
        page?: number;
      }): Promise<{ data: ExistingComment[] }>;
      createComment(params: {
        owner: string;
        repo: string;
        issue_number: number;
        body: string;
      }): Promise<{ data: { id: number } }>;
      updateComment(params: {
        owner: string;
        repo: string;
        comment_id: number;
        body: string;
      }): Promise<{ data: { id: number } }>;
    };
    users: {
      getAuthenticated(): Promise<{ data: { login: string } }>;
    };
  };
}

export interface ExistingComment {
  id: number;
  body?: string | null;
  user?: { login?: string | null; type?: string | null } | null;
}

export interface UpsertOptions {
  client: CommentClient;
  owner: string;
  repo: string;
  issueNumber: number;
  body: string;
}

/**
 * Upsert the sticky comment. Returns the comment id of whichever
 * comment was created or patched.
 */
export async function upsertStickyComment(opts: UpsertOptions): Promise<number> {
  const trimmed = truncateBody(opts.body);
  const expectedAuthor = await resolveBotLogin(opts.client);
  const existing = await findExistingComment(opts, expectedAuthor);

  if (existing) {
    const { data } = await opts.client.rest.issues.updateComment({
      owner: opts.owner,
      repo: opts.repo,
      comment_id: existing.id,
      body: trimmed,
    });
    core.info(`Updated existing prov timeline comment (id ${data.id}).`);
    return data.id;
  }

  const { data } = await opts.client.rest.issues.createComment({
    owner: opts.owner,
    repo: opts.repo,
    issue_number: opts.issueNumber,
    body: trimmed,
  });
  core.info(`Created prov timeline comment (id ${data.id}).`);
  return data.id;
}

/**
 * Walk PR comments looking for one authored by the bot AND starting
 * with the marker. Both conditions are required — the marker alone is
 * spoofable by any user with comment-write access.
 */
async function findExistingComment(
  opts: UpsertOptions,
  expectedAuthor: string | null,
): Promise<ExistingComment | null> {
  const perPage = 100;
  for (let page = 1; ; page += 1) {
    const { data } = await opts.client.rest.issues.listComments({
      owner: opts.owner,
      repo: opts.repo,
      issue_number: opts.issueNumber,
      per_page: perPage,
      page,
    });
    for (const c of data) {
      if (!authorMatches(c, expectedAuthor)) continue;
      const body = c.body ?? '';
      if (body.startsWith(STICKY_MARKER)) return c;
    }
    if (data.length < perPage) return null;
  }
}

export function authorMatches(comment: ExistingComment, expectedAuthor: string | null): boolean {
  const login = comment.user?.login ?? '';
  if (!login) return false;
  if (expectedAuthor && login === expectedAuthor) return true;
  // Fallback when we can't resolve a precise author (some token shapes
  // can't read /user): accept the default Actions bot identity. This
  // still rejects every human contributor.
  if (!expectedAuthor && login === 'github-actions[bot]') return true;
  return false;
}

/**
 * Resolve the login the token authenticates as. Returns null when the
 * token can't read `/user` (e.g., some GitHub App tokens) — callers
 * fall back to filtering on `github-actions[bot]`.
 */
export async function resolveBotLogin(client: CommentClient): Promise<string | null> {
  try {
    const { data } = await client.rest.users.getAuthenticated();
    return data.login;
  } catch (err) {
    core.debug(`Could not resolve authenticated identity: ${(err as Error).message}`);
    return null;
  }
}

/**
 * Truncate the body to fit under GitHub's 65,536-char limit while
 * preserving the marker and appending a `<details>` footer that names
 * the dropped tail.
 */
export function truncateBody(body: string): string {
  if (body.length <= SAFE_BODY_LIMIT) return body;
  const dropped = body.length - SAFE_BODY_LIMIT;
  const head = body.slice(0, SAFE_BODY_LIMIT);
  // Trim back to a complete line so the truncation seam doesn't break
  // mid-Markdown (e.g., inside a `<details>` block opened by the
  // renderer). Falls back to the raw cut when no newline is found.
  const lastNewline = head.lastIndexOf('\n');
  const safeHead = lastNewline > SAFE_BODY_LIMIT - 1024 ? head.slice(0, lastNewline) : head;
  return (
    `${safeHead}\n\n` +
    `<details><summary>Timeline truncated</summary>\n\n` +
    `The full timeline body exceeded GitHub's 65,536-character comment limit by ` +
    `${dropped.toLocaleString()} characters and was trimmed. ` +
    `Run \`prov pr-timeline --base <base> --head HEAD --markdown\` locally for the full output.\n` +
    `</details>\n`
  );
}

/**
 * Convenience wrapper for the production call site — constructs the
 * Octokit client from the input token. Tests construct their own client
 * and call `upsertStickyComment` directly.
 */
export function newCommentClient(token: string): CommentClient {
  return getOctokit(token) as unknown as CommentClient;
}
