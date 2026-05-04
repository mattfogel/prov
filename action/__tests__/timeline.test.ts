import { renderTimeline, type ExecFn } from '../src/timeline';

const STICKY_MARKER = '<!-- prov:pr-timeline -->';

const makeExec = (opts: {
  exitCode: number;
  stdout?: string;
  stderr?: string;
  capture?: { command: string; args?: string[]; cwd?: string };
}): ExecFn => {
  return async (commandLine, args, options) => {
    if (opts.capture) {
      opts.capture.command = commandLine;
      opts.capture.args = args;
      opts.capture.cwd = options?.cwd;
    }
    if (opts.stdout && options?.listeners?.stdout) {
      options.listeners.stdout(Buffer.from(opts.stdout, 'utf-8'));
    }
    if (opts.stderr && options?.listeners?.stderr) {
      options.listeners.stderr(Buffer.from(opts.stderr, 'utf-8'));
    }
    return opts.exitCode;
  };
};

describe('renderTimeline', () => {
  test('passes base/head/--markdown to the binary and returns stdout', async () => {
    const capture = { command: '' } as { command: string; args?: string[]; cwd?: string };
    const stdout = `${STICKY_MARKER}\n## PR Intent Timeline\n\nfake body\n`;
    const out = await renderTimeline({
      provPath: '/usr/local/bin/prov',
      baseRef: 'origin/main',
      headRef: 'HEAD',
      cwd: '/tmp/checkout',
      exec: makeExec({ exitCode: 0, stdout, capture }),
    });
    expect(out).toBe(stdout);
    expect(capture.command).toBe('/usr/local/bin/prov');
    expect(capture.args).toEqual([
      'pr-timeline',
      '--base',
      'origin/main',
      '--head',
      'HEAD',
      '--markdown',
    ]);
    expect(capture.cwd).toBe('/tmp/checkout');
  });

  test('throws with stderr context when the binary exits non-zero', async () => {
    const exec = makeExec({
      exitCode: 2,
      stderr: 'failed to compute diff between base and head',
    });
    await expect(
      renderTimeline({ provPath: 'prov', baseRef: 'main', headRef: 'HEAD', exec }),
    ).rejects.toThrow(/exited with code 2.*failed to compute diff/);
  });

  test('reports `(no stderr)` when the binary fails silently', async () => {
    const exec = makeExec({ exitCode: 1 });
    await expect(
      renderTimeline({ provPath: 'prov', baseRef: 'main', headRef: 'HEAD', exec }),
    ).rejects.toThrow(/exited with code 1.*no stderr/);
  });
});
