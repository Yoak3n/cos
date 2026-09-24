/**
 * @cos/profile/fallback — the flat module fallback the profile loader relies
 * on, maintained by the launcher (DSH-aligned).
 *
 * `<home>/profiles/node_modules` holds one symlink per package in the cos
 * app's resolvable dependency CLOSURE (BFS over `dependencies` from the app
 * manifest), each resolved from its own real location. Node's parent-directory
 * walk from any profile finds this directory after the profile's own
 * `node_modules`, so every in-box plugin resolves without pnpm ever managing
 * it — the exact "bundles come from the installation" contract. The closure
 * (not just direct dependencies) is required for out-of-tree plugins: their
 * peer dependencies name Service Definition packages that the app reaches only
 * through its Service Provider packages. Symlinked packages resolve their own
 * dependencies from their real directories (Node's default symlink-following),
 * so each package needs only its one flat link.
 *
 * Idempotent: correct links are kept and moved installations are re-pointed;
 * a stale link to a vanished package stays until its name is reused (dangling
 * links are invisible to resolution).
 * @module @cos/profile/fallback
 */

import {
  existsSync, lstatSync, mkdirSync, readFileSync, readlinkSync, symlinkSync, unlinkSync,
} from 'node:fs'
import { dirname, join } from 'node:path'
import { PROFILES_DIR, PROFILES_MODULE_FALLBACK, resolveCosHome } from './home.ts'
import type { ProfileManifest } from './manifest.ts'
import { packageDirFromAnchor } from './resolve.ts'

/** Ensure `link` is a symlink to `target`, replacing a wrong or dangling link; a real directory throws. */
function ensureSymlink(link: string, target: string): void {
  let stat
  try {
    stat = lstatSync(link)
  } catch {
    // Missing link (first run) — created below. Any other lstat failure on a
    // path we just created the parent of would resurface on symlinkSync.
    stat = undefined
  }
  if (stat !== undefined) {
    if (!stat.isSymbolicLink()) {
      throw new Error(`cos: ${link} exists and is not a symlink; remove it so cos can manage the installation fallback`)
    }
    if (readlinkSync(link) === target) return
    // unlink deletes the reparse point itself on Windows too; rmSync treats a
    // junction as a directory and throws EISDIR unless recursive.
    unlinkSync(link)
  }
  try {
    symlinkSync(target, link, 'junction')
  } catch (error) {
    // Concurrent launches heal the same fallback; losing the race to a
    // process writing the identical link is success, anything else is not.
    if ((error as NodeJS.ErrnoException).code !== 'EEXIST'
      || !lstatSync(link).isSymbolicLink() || readlinkSync(link) !== target) {
      throw error
    }
  }
}

/**
 * Maintain the flat module fallback `$COS_HOME/profiles/node_modules`.
 * @param installAnchor - absolute path of the cos app's package.json.
 * @param home - the cos home; defaults to {@link resolveCosHome}.
 */
export function healProfilesModuleFallback(installAnchor: string, home: string = resolveCosHome()): void {
  const profilesDir = join(home, PROFILES_DIR)
  const modulesDir = join(profilesDir, PROFILES_MODULE_FALLBACK)
  mkdirSync(modulesDir, { recursive: true })
  const appManifest = JSON.parse(readFileSync(installAnchor, 'utf8')) as ProfileManifest
  const links = new Map<string, string>()
  if (appManifest.name !== undefined) links.set(appManifest.name, dirname(installAnchor))
  // BFS over the resolvable dependency graph; the visited set is the link
  // map itself (first resolution wins, matching Node's own nearest-wins).
  const queue: { anchor: string; manifest: ProfileManifest }[] = [{ anchor: installAnchor, manifest: appManifest }]
  for (let next = queue.shift(); next !== undefined; next = queue.shift()) {
    // Peer dependencies participate: plugins import cordis and the @cos/*
    // service packages directly even when they are not listed as regular
    // dependencies of every intermediate package.
    /* v8 ignore next -- a real app manifest always declares dependencies */
    for (const dep of [...Object.keys(next.manifest.dependencies ?? {}), ...Object.keys(next.manifest.peerDependencies ?? {})]) {
      if (links.has(dep)) continue
      const dir = packageDirFromAnchor(next.anchor, dep)
      // A declared-but-uninstalled dependency cannot be a loader-visible
      // plugin; skip it rather than fail the whole boot.
      if (dir === undefined) continue
      links.set(dep, dir)
      const manifestPath = join(dir, 'package.json')
      let manifest: ProfileManifest
      try {
        manifest = JSON.parse(readFileSync(manifestPath, 'utf8')) as ProfileManifest
      } catch {
        continue
      }
      queue.push({ anchor: manifestPath, manifest })
    }
  }
  for (const [packageName, target] of links) {
    const link = join(modulesDir, packageName)
    mkdirSync(dirname(link), { recursive: true })
    ensureSymlink(link, target)
  }
}