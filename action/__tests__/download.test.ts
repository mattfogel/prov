import {
  computeTarget,
  findBinaryAsset,
  findBundleAsset,
  locateProvBinary,
  type ReleaseInfo,
} from '../src/download';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

describe('computeTarget', () => {
  test.each([
    ['linux', 'x64', 'x86_64-unknown-linux-musl'],
    ['linux', 'arm64', 'aarch64-unknown-linux-musl'],
    ['darwin', 'x64', 'x86_64-apple-darwin'],
    ['darwin', 'arm64', 'aarch64-apple-darwin'],
  ])('resolves %s/%s → %s', (platform, arch, expected) => {
    expect(computeTarget(platform as NodeJS.Platform, arch)).toBe(expected);
  });

  test('throws a useful error on unsupported platforms', () => {
    expect(() => computeTarget('win32' as NodeJS.Platform, 'x64')).toThrow(/Unsupported runner/);
    expect(() => computeTarget('linux', 'mips' as string)).toThrow(/Unsupported runner/);
  });
});

const fakeRelease = (names: string[]): ReleaseInfo => ({
  tag_name: 'v0.1.1',
  assets: names.map((name) => ({
    name,
    browser_download_url: `https://example.com/${name}`,
  })),
});

describe('findBinaryAsset', () => {
  test('picks the tarball matching the target triple', () => {
    const r = fakeRelease([
      'prov-v0.1.1-x86_64-apple-darwin.tar.gz',
      'prov-v0.1.1-aarch64-apple-darwin.tar.gz',
      'prov-v0.1.1-x86_64-unknown-linux-musl.tar.gz',
    ]);
    const a = findBinaryAsset(r, 'aarch64-apple-darwin');
    expect(a.name).toBe('prov-v0.1.1-aarch64-apple-darwin.tar.gz');
  });

  test('errors clearly when no matching asset is present', () => {
    const r = fakeRelease(['prov-v0.1.1-x86_64-apple-darwin.tar.gz']);
    expect(() => findBinaryAsset(r, 'aarch64-apple-darwin')).toThrow(
      /No prov binary asset found for target aarch64-apple-darwin/,
    );
  });

  test('refuses to guess when multiple assets match', () => {
    const r = fakeRelease([
      'prov-v0.1.1-x86_64-apple-darwin.tar.gz',
      'prov-v0.1.1-x86_64-apple-darwin-debug.tar.gz',
    ]);
    expect(() => findBinaryAsset(r, 'x86_64-apple-darwin')).toThrow(
      /Multiple prov binary assets matched/,
    );
  });
});

describe('findBundleAsset', () => {
  const binary = {
    name: 'prov-v0.1.1-x86_64-unknown-linux-musl.tar.gz',
    browser_download_url: 'https://example.com/prov-v0.1.1-x86_64-unknown-linux-musl.tar.gz',
  };

  test('matches the legacy `.cosign.bundle` suffix', () => {
    const r = fakeRelease([binary.name, `${binary.name}.cosign.bundle`]);
    expect(findBundleAsset(r, binary).name).toBe(`${binary.name}.cosign.bundle`);
  });

  test('matches the newer `.sigstore` suffix', () => {
    const r = fakeRelease([binary.name, `${binary.name}.sigstore`]);
    expect(findBundleAsset(r, binary).name).toBe(`${binary.name}.sigstore`);
  });

  test('errors when no bundle is present', () => {
    const r = fakeRelease([binary.name]);
    expect(() => findBundleAsset(r, binary)).toThrow(/No cosign bundle found/);
  });
});

describe('locateProvBinary', () => {
  let dir: string;

  beforeEach(() => {
    dir = fs.mkdtempSync(path.join(os.tmpdir(), 'prov-locate-'));
  });

  afterEach(() => {
    fs.rmSync(dir, { recursive: true, force: true });
  });

  test('finds the binary at the archive root', () => {
    const binary = path.join(dir, 'prov');
    fs.writeFileSync(binary, '');
    expect(locateProvBinary(dir)).toBe(binary);
  });

  test('finds the binary one directory deep', () => {
    const nestedDir = path.join(dir, 'prov-v0.1.1-x86_64-unknown-linux-musl');
    fs.mkdirSync(nestedDir);
    const binary = path.join(nestedDir, 'prov');
    fs.writeFileSync(binary, '');
    expect(locateProvBinary(dir)).toBe(binary);
  });

  test('errors when no prov binary is present', () => {
    fs.writeFileSync(path.join(dir, 'README'), 'nothing useful');
    expect(() => locateProvBinary(dir)).toThrow(/does not contain a 'prov' binary/);
  });
});
