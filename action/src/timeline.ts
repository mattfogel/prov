/**
 * Thin wrapper that runs `prov pr-timeline --base <base> --head <head> --markdown`
 * and returns its stdout. The Rust binary is the single source of truth for
 * the comment shape — no parallel renderer here.
 */

import { exec, ExecOptions } from '@actions/exec';

export interface RenderOptions {
  /** Absolute path to the prov binary. */
  provPath: string;
  /** Diff base ref (e.g. the PR target branch). */
  baseRef: string;
  /** Diff head ref (defaults to HEAD on the checked-out tree). */
  headRef: string;
  /** Working directory the binary runs in. Defaults to the runner's checkout. */
  cwd?: string;
  /**
   * Spawner override. Tests inject a stub so we don't actually execve a
   * binary; production uses `@actions/exec`.
   */
  exec?: ExecFn;
}

/** Function shape compatible with `@actions/exec`'s default export. */
export type ExecFn = (
  commandLine: string,
  args?: string[],
  options?: ExecOptions,
) => Promise<number>;

/**
 * Run the timeline renderer and return the Markdown body it produced.
 *
 * Throws on non-zero exit, propagating the binary's stderr in the error
 * message so the workflow run surfaces a useful diagnostic.
 */
export async function renderTimeline(opts: RenderOptions): Promise<string> {
  let stdout = '';
  let stderr = '';
  const runner = opts.exec ?? exec;
  const code = await runner(
    opts.provPath,
    ['pr-timeline', '--base', opts.baseRef, '--head', opts.headRef, '--markdown'],
    {
      cwd: opts.cwd,
      ignoreReturnCode: true,
      silent: true,
      listeners: {
        stdout: (data: Buffer) => {
          stdout += data.toString('utf-8');
        },
        stderr: (data: Buffer) => {
          stderr += data.toString('utf-8');
        },
      },
    },
  );
  if (code !== 0) {
    throw new Error(
      `prov pr-timeline exited with code ${code}: ${stderr.trim() || '(no stderr)'}`,
    );
  }
  return stdout;
}
