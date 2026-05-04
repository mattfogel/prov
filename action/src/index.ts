/**
 * Entry point for the Prov PR intent timeline Action.
 *
 * Wiring:
 *   1. Pull inputs + GitHub context.
 *   2. Bail with an info log if we're not running on a pull_request event
 *      (the timeline is meaningless without base/head refs and an issue
 *      number to comment on).
 *   3. Download + verify the prov binary.
 *   4. Run the timeline renderer against the diff.
 *   5. Upsert the sticky comment.
 *
 * Errors propagate via `core.setFailed` so the workflow run is marked
 * red — silent failure here would leave reviewers thinking the timeline
 * just had nothing to say.
 */

import * as core from '@actions/core';
import { context } from '@actions/github';

import { downloadProv } from './download';
import { renderTimeline } from './timeline';
import { newCommentClient, upsertStickyComment } from './github';

async function run(): Promise<void> {
  // Default the output to empty string up front so the early-exit paths
  // (non-PR event, empty body) emit a present-but-empty value rather than
  // an absent key. Consumers writing `outputs.comment-id == ''` then match.
  core.setOutput('comment-id', '');
  try {
    const token = core.getInput('github-token', { required: true });
    const version = core.getInput('prov-version');
    const baseRefInput = core.getInput('base-ref');
    const headRefInput = core.getInput('head-ref');
    const skipVerification = core.getBooleanInput('skip-verification');

    const pr = context.payload.pull_request;
    if (!pr) {
      core.info(
        'No pull_request payload — Prov PR intent timeline only runs on pull_request events. Skipping.',
      );
      return;
    }

    const baseRef = baseRefInput || `origin/${pr.base.ref}`;
    const headRef = headRefInput || 'HEAD';
    const issueNumber = pr.number;

    core.info(`Posting Prov PR intent timeline for PR #${issueNumber} (${baseRef}..${headRef}).`);

    const provPath = await downloadProv({ version, token, skipVerification });

    const body = await renderTimeline({ provPath, baseRef, headRef });
    if (body.trim().length === 0) {
      core.warning('prov pr-timeline produced empty output. Skipping comment post.');
      return;
    }

    const client = newCommentClient(token);
    const commentId = await upsertStickyComment({
      client,
      owner: context.repo.owner,
      repo: context.repo.repo,
      issueNumber,
      body,
    });
    core.setOutput('comment-id', String(commentId));
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    core.setFailed(message);
  }
}

void run();
