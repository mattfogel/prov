import {
  authorMatches,
  STICKY_MARKER,
  SAFE_BODY_LIMIT,
  truncateBody,
  upsertStickyComment,
  type CommentClient,
  type ExistingComment,
} from '../src/github';

function buildClient(opts: {
  comments: ExistingComment[];
  authenticated?: { login: string } | null;
  perPage?: number;
}): {
  client: CommentClient;
  calls: {
    list: number;
    create: number;
    update: number;
    lastUpdate?: { id: number; body: string };
    lastCreate?: { body: string };
  };
} {
  const calls = { list: 0, create: 0, update: 0 } as {
    list: number;
    create: number;
    update: number;
    lastUpdate?: { id: number; body: string };
    lastCreate?: { body: string };
  };
  const perPage = opts.perPage ?? 100;
  const client: CommentClient = {
    rest: {
      issues: {
        async listComments(params) {
          calls.list += 1;
          const page = params.page ?? 1;
          const start = (page - 1) * perPage;
          const slice = opts.comments.slice(start, start + perPage);
          return { data: slice };
        },
        async createComment(params) {
          calls.create += 1;
          calls.lastCreate = { body: params.body };
          return { data: { id: 999 } };
        },
        async updateComment(params) {
          calls.update += 1;
          calls.lastUpdate = { id: params.comment_id, body: params.body };
          return { data: { id: params.comment_id } };
        },
      },
      users: {
        async getAuthenticated() {
          if (!opts.authenticated) {
            throw new Error('not authenticated');
          }
          return { data: opts.authenticated };
        },
      },
    },
  };
  return { client, calls };
}

describe('upsertStickyComment', () => {
  const owner = 'octo';
  const repo = 'app';
  const issueNumber = 42;
  const body = `${STICKY_MARKER}\n## PR Intent Timeline\n\nfresh body\n`;

  test('creates a new comment on first run', async () => {
    const { client, calls } = buildClient({
      comments: [],
      authenticated: { login: 'github-actions[bot]' },
    });
    const id = await upsertStickyComment({ client, owner, repo, issueNumber, body });
    expect(id).toBe(999);
    expect(calls.create).toBe(1);
    expect(calls.update).toBe(0);
    expect(calls.lastCreate?.body).toBe(body);
  });

  test('updates the existing bot-authored sticky comment on subsequent runs', async () => {
    const { client, calls } = buildClient({
      comments: [
        {
          id: 17,
          body: `${STICKY_MARKER}\n## PR Intent Timeline\n\nstale body\n`,
          user: { login: 'github-actions[bot]', type: 'Bot' },
        },
      ],
      authenticated: { login: 'github-actions[bot]' },
    });
    const id = await upsertStickyComment({ client, owner, repo, issueNumber, body });
    expect(id).toBe(17);
    expect(calls.create).toBe(0);
    expect(calls.update).toBe(1);
    expect(calls.lastUpdate?.id).toBe(17);
    expect(calls.lastUpdate?.body).toBe(body);
  });

  test('refuses to edit a marker-spoofing comment from a non-bot author', async () => {
    const { client, calls } = buildClient({
      comments: [
        {
          id: 5,
          body: `${STICKY_MARKER}\nspoofed by a contributor`,
          user: { login: 'evil-contributor', type: 'User' },
        },
      ],
      authenticated: { login: 'github-actions[bot]' },
    });
    const id = await upsertStickyComment({ client, owner, repo, issueNumber, body });
    expect(id).toBe(999);
    expect(calls.update).toBe(0);
    expect(calls.create).toBe(1);
  });

  test('falls back to github-actions[bot] when getAuthenticated fails', async () => {
    const { client, calls } = buildClient({
      comments: [
        {
          id: 21,
          body: `${STICKY_MARKER}\nold body`,
          user: { login: 'github-actions[bot]', type: 'Bot' },
        },
      ],
      authenticated: null,
    });
    const id = await upsertStickyComment({ client, owner, repo, issueNumber, body });
    expect(id).toBe(21);
    expect(calls.update).toBe(1);
  });
});

describe('authorMatches', () => {
  test('matches the resolved bot login exactly', () => {
    const c: ExistingComment = { id: 1, user: { login: 'my-app[bot]' } };
    expect(authorMatches(c, 'my-app[bot]')).toBe(true);
    expect(authorMatches(c, 'other[bot]')).toBe(false);
  });

  test('falls back to github-actions[bot] when no expected author resolved', () => {
    const c1: ExistingComment = { id: 1, user: { login: 'github-actions[bot]' } };
    const c2: ExistingComment = { id: 2, user: { login: 'someone' } };
    expect(authorMatches(c1, null)).toBe(true);
    expect(authorMatches(c2, null)).toBe(false);
  });

  test('rejects comments missing user metadata', () => {
    expect(authorMatches({ id: 1 }, null)).toBe(false);
    expect(authorMatches({ id: 1, user: null }, null)).toBe(false);
    expect(authorMatches({ id: 1, user: { login: '' } }, null)).toBe(false);
  });
});

describe('truncateBody', () => {
  test('passes bodies under the limit through unchanged', () => {
    const body = `${STICKY_MARKER}\nshort body`;
    expect(truncateBody(body)).toBe(body);
  });

  test('truncates oversized bodies and appends a footer noting omission', () => {
    const huge = `${STICKY_MARKER}\n` + 'line\n'.repeat(20000);
    const out = truncateBody(huge);
    expect(out.length).toBeLessThanOrEqual(65536);
    expect(out.length).toBeLessThan(huge.length);
    expect(out).toContain('Timeline truncated');
    expect(out).toContain('exceeded GitHub');
    expect(out.startsWith(STICKY_MARKER)).toBe(true);
  });

  test('truncation cut lands on a line boundary when one is nearby', () => {
    const huge = `${STICKY_MARKER}\n` + 'short-line-of-text\n'.repeat(10000);
    const out = truncateBody(huge);
    const cutIndex = out.indexOf('<details>');
    // The character just before the `\n\n<details>` separator must be a newline,
    // proving the trim landed on a line boundary not mid-token.
    expect(out[cutIndex - 1]).toBe('\n');
    expect(out[cutIndex - 2]).toBe('\n');
  });

  test('SAFE_BODY_LIMIT leaves room for the truncation footer', () => {
    expect(SAFE_BODY_LIMIT).toBeLessThan(65536);
    expect(65536 - SAFE_BODY_LIMIT).toBeGreaterThanOrEqual(512);
  });
});
