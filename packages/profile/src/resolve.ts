/**
 * @cos/profile/resolve — two-anchor bundle package resolution (DSH-aligned).
 *
 * A bundle name resolves first from the cos installation (the harness's own
 * package), then from the profile directory — the same contract as DSH: an
 * in-box bundle always comes from the running installation, never from a
 * profile-local copy. Resolution does not depend on the package exporting
 * `./package.json`.
 *
 * A resolved bundle package may declare its patch layer either DSH-style
 * (`"dsh": { "bundle": { "patch": "./cordis.patch.yml" } }` in package.json)
 * or cos-dir-style (`cordis.patch.yml` / `bundle.yml` next to the manifest).
 * @module @cos/profile/resolve
 */

import { existsSync, readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { basename, join, resolve } from 'node:path'
import { parse as parseYaml } from 'yaml'
import { manifestBundlePatch, readProfileManifest } from './manifest.ts'

/**
 * Resolve a package's root directory from one anchor without depending on the
 * package exporting `./package.json` (`require.resolve` would need that):
 * probe the require resolution paths for a directory holding the named
 * manifest. This is Node's own node_modules lookup order, so the result
 * matches what the Loader would import from the same anchor, and
 * `existsSync` follows the symlinks pnpm's isolated layout uses.
 */
export function packageDirFromAnchor(anchor: string, packageName: string): string | undefined {
  // resolve.paths returns null only for builtins, which no bundle name is.
  /* v8 ignore next */
  for (const searchPath of createRequire(anchor).resolve.paths(packageName) ?? []) {
    const candidate = join(searchPath, packageName)
    if (existsSync(join(candidate, 'package.json'))) return candidate
  }
  return undefined
}

/**
 * Resolve one bundle package's directory: installation anchor first, then the
 * profile directory.
 * @param binName - the diagnostic prefix on the thrown error.
 * @param packageName - the bundle's package name from `dsh.profile.bundles`.
 * @param installAnchor - absolute path of a file inside the cos app package (its package.json).
 * @param profileDir - the profile directory (second anchor).
 * @returns the bundle package's absolute directory.
 */
export function resolveBundleDir(
  binName: string, packageName: string, installAnchor: string, profileDir: string,
): string {
  for (const anchor of [installAnchor, join(profileDir, 'package.json')]) {
    const dir = packageDirFromAnchor(anchor, packageName)
    if (dir !== undefined) return dir
  }
  throw new Error(
    `${binName}: cannot resolve profile bundle ${JSON.stringify(packageName)} from the cos installation or ${profileDir}; `
    + `run 'pnpm plugin --profile ${basename(profileDir)} add <package>' if its dependency is not installed`,
  )
}

/**
 * The absolute patch file path of a bundle directory, DSH-manifest style or
 * cos-dir style (`cordis.patch.yml`). The file may not exist yet for a
 * dir-style bundle whose directory only carries `bundle.yml`.
 * @param dir - the bundle package directory.
 */
export function bundlePatchPath(dir: string): string {
  let rel: string | undefined
  try {
    rel = manifestBundlePatch(readProfileManifest('cos', dir))
  } catch {
    // No manifest (pure composition dir) — dir-style below.
  }
  return resolve(dir, rel ?? 'cordis.patch.yml')
}

/**
 * Whether a directory looks like a bundle: a manifest-declared patch layer,
 * or cos-dir-style files (`cordis.patch.yml` / `bundle.yml`).
 */
export function isBundleDir(dir: string): boolean {
  if (!existsSync(dir)) return false
  if (existsSync(join(dir, 'cordis.patch.yml')) || existsSync(join(dir, 'bundle.yml'))) return true
  try {
    return manifestBundlePatch(readProfileManifest('cos', dir)) !== undefined
  } catch {
    return false
  }
}

/** The `requires` declaration of a bundle directory (`bundle.yml`), dir-style only. */
export function readBundleDeclaration(dir: string): string[] {
  try {
    const doc = parseYaml(readFileSync(join(dir, 'bundle.yml'), 'utf8')) as { bundle?: { requires?: unknown } }
    const requires = doc?.bundle?.requires
    if (!Array.isArray(requires)) return []
    return requires.map((entry) => String(entry))
  } catch {
    return []
  }
}