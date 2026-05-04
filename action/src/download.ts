/**
 * Download the prov binary from GitHub Releases for the runner's OS/arch
 * and verify it with Sigstore cosign keyless attestation before exposing
 * the path to the rest of the Action.
 *
 * SHA256 alone would not be enough: a release-asset-replacement attack
 * substitutes the checksum and the binary atomically. The cosign bundle
 * is fetched alongside the binary and verified by the bundled OIDC
 * identity (the release workflow signs each artifact with its own
 * GitHub Actions OIDC token).
 */

import * as fs from 'fs';
import * as path from 'path';

import * as core from '@actions/core';
import * as tc from '@actions/tool-cache';
import { getOctokit } from '@actions/github';

const REPO_OWNER = 'mattfogel';
const REPO_NAME = 'prov';

export interface DownloadOptions {
  /** Release tag to fetch, e.g. "v0.1.1". Empty / "latest" → newest published release. */
  version: string;
  /** Token used to call the GitHub Releases API. Public reads work with the default `GITHUB_TOKEN`. */
  token: string;
  /** Skip cosign verification. Off by default; turning this on exposes you to release-asset replacement. */
  skipVerification: boolean;
}

export interface ReleaseAsset {
  name: string;
  browser_download_url: string;
}

export interface ReleaseInfo {
  tag_name: string;
  assets: ReleaseAsset[];
}

export type Verifier = (binaryPath: string, bundlePath: string) => Promise<void>;

/**
 * Resolve the cargo-dist target triple for the runner. cargo-dist uses
 * `*-musl` for Linux so the binary is self-contained on minimal runners.
 */
export function computeTarget(
  platform: NodeJS.Platform = process.platform,
  arch: string = process.arch,
): string {
  if (platform === 'linux' && arch === 'x64') return 'x86_64-unknown-linux-musl';
  if (platform === 'linux' && arch === 'arm64') return 'aarch64-unknown-linux-musl';
  if (platform === 'darwin' && arch === 'x64') return 'x86_64-apple-darwin';
  if (platform === 'darwin' && arch === 'arm64') return 'aarch64-apple-darwin';
  throw new Error(
    `Unsupported runner platform/arch: ${platform}/${arch}. ` +
      `Prov releases ship for linux x64/arm64 and macOS x64/arm64. ` +
      `Open an issue at https://github.com/${REPO_OWNER}/${REPO_NAME}/issues if you need another target.`,
  );
}

/**
 * Pick the binary tarball for `target` from a release. cargo-dist asset
 * names look like `prov-v0.1.1-x86_64-unknown-linux-musl.tar.gz`; we
 * match on the triple substring to stay forgiving of minor naming
 * changes across cargo-dist versions.
 */
export function findBinaryAsset(release: ReleaseInfo, target: string): ReleaseAsset {
  const candidates = release.assets.filter(
    (a) => a.name.includes(target) && a.name.endsWith('.tar.gz'),
  );
  if (candidates.length === 0) {
    throw new Error(
      `No prov binary asset found for target ${target} in release ${release.tag_name}. ` +
        `Available assets: ${release.assets.map((a) => a.name).join(', ') || '(none)'}.`,
    );
  }
  if (candidates.length > 1) {
    throw new Error(
      `Multiple prov binary assets matched target ${target} in release ${release.tag_name}: ` +
        `${candidates.map((a) => a.name).join(', ')}. Refusing to guess.`,
    );
  }
  return candidates[0]!;
}

/**
 * Find the cosign bundle that signs `binaryAsset`. Supports both the
 * legacy `<asset>.cosign.bundle` naming and the newer `<asset>.sigstore`
 * convention so we don't break across cargo-dist / cosign upgrades.
 */
export function findBundleAsset(release: ReleaseInfo, binaryAsset: ReleaseAsset): ReleaseAsset {
  const candidates = release.assets.filter(
    (a) =>
      a.name === `${binaryAsset.name}.cosign.bundle` ||
      a.name === `${binaryAsset.name}.sigstore` ||
      a.name === `${binaryAsset.name}.bundle`,
  );
  if (candidates.length === 0) {
    throw new Error(
      `No cosign bundle found for ${binaryAsset.name} in release ${release.tag_name}. ` +
        `Set skip-verification: true to bypass (NOT recommended).`,
    );
  }
  return candidates[0]!;
}

/**
 * Fetch the release matching `version`, download + verify the binary,
 * extract it, and return the absolute path to the executable. Errors
 * surface to the caller; the Action treats them as run failures.
 */
export async function downloadProv(opts: DownloadOptions, verifier?: Verifier): Promise<string> {
  const octokit = getOctokit(opts.token);
  const tag = opts.version.trim();
  const release = await fetchRelease(octokit, tag);

  const target = computeTarget();
  const binaryAsset = findBinaryAsset(release, target);
  core.info(`Resolved prov ${release.tag_name} → ${binaryAsset.name}`);

  const binaryDownload = await tc.downloadTool(binaryAsset.browser_download_url);

  if (opts.skipVerification) {
    core.warning(
      'Cosign verification skipped (skip-verification: true). The downloaded binary is unverified.',
    );
  } else {
    const bundleAsset = findBundleAsset(release, binaryAsset);
    const bundleDownload = await tc.downloadTool(bundleAsset.browser_download_url);
    const verify = verifier ?? defaultVerifier;
    await verify(binaryDownload, bundleDownload);
    core.info('Cosign verification succeeded.');
  }

  const extractedDir = await tc.extractTar(binaryDownload);
  const provPath = locateProvBinary(extractedDir);
  fs.chmodSync(provPath, 0o755);
  return provPath;
}

async function fetchRelease(
  octokit: ReturnType<typeof getOctokit>,
  tag: string,
): Promise<ReleaseInfo> {
  if (tag === '' || tag === 'latest') {
    const { data } = await octokit.rest.repos.getLatestRelease({
      owner: REPO_OWNER,
      repo: REPO_NAME,
    });
    return { tag_name: data.tag_name, assets: data.assets };
  }
  const { data } = await octokit.rest.repos.getReleaseByTag({
    owner: REPO_OWNER,
    repo: REPO_NAME,
    tag,
  });
  return { tag_name: data.tag_name, assets: data.assets };
}

/**
 * Locate the `prov` executable inside the extracted tarball. cargo-dist
 * archives place the binary either at the root or inside a single
 * versioned directory; both shapes are handled.
 */
export function locateProvBinary(extractedDir: string): string {
  const direct = path.join(extractedDir, 'prov');
  if (fs.existsSync(direct)) return direct;

  for (const entry of fs.readdirSync(extractedDir, { withFileTypes: true })) {
    if (entry.isDirectory()) {
      const nested = path.join(extractedDir, entry.name, 'prov');
      if (fs.existsSync(nested)) return nested;
    }
  }
  throw new Error(`Extracted archive at ${extractedDir} does not contain a 'prov' binary.`);
}

/**
 * Default cosign verifier — delegates to sigstore-js. Isolated as a
 * standalone function so tests can substitute a stub.
 */
const defaultVerifier: Verifier = async (binaryPath, bundlePath) => {
  // Lazy-require so unit tests that supply their own verifier don't pay
  // the sigstore module load cost or need its native deps installed.
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const sigstore = require('sigstore');
  const artifact = fs.readFileSync(binaryPath);
  const bundle = JSON.parse(fs.readFileSync(bundlePath, 'utf-8'));
  await sigstore.verify(bundle, artifact);
};
