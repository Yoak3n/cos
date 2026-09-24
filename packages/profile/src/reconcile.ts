/**
 * @cos/profile/reconcile — keep `dsh.profile.bundles` in sync with the
 * installed state after a `pnpm plugin` run (DSH-aligned).
 *
 * Reconciling by installed state, not by dependency diff, means `update`
 * activates a package that gained its `dsh.bundle` declaration in a newer
 * version.
 * @module @cos/profile/reconcile
 */

import type { ProfileManifest } from './manifest.ts'
import { readProfileManifest, writeProfileManifest } from './manifest.ts'
import { resolveBundleDir } from './resolve.ts'

/**
 * Whether a resolved dependency exports a profile patch, i.e. is a bundle.
 * @param binName - the diagnostic prefix on thrown errors.
 * @param packageName - the dependency's package name.
 * @param installAnchor - the cos installation anchor (first resolution anchor).
 * @param profileDir - the profile directory (second resolution anchor).
 * @returns true when the package manifest declares a bundle patch.
 */
export function exportsPatch(
  binName: string, packageName: string, installAnchor: string, profileDir: string,
): boolean {
  let dir: string
  try {
    dir = resolveBundleDir(binName, packageName, installAnchor, profileDir)
  } catch {
    return false // pnpm reported success yet the package is unresolvable — treat as plain
  }
  const manifest = readProfileManifest(binName, dir)
  return manifest.dsh?.bundle?.patch !== undefined || manifest.cos?.bundle?.patch !== undefined
}

/**
 * Reconcile `dsh.profile.bundles` against the installed state: pnpm has
 * already written the real installed names (so a git/path/tarball/alias spec
 * on the command line reconciles by its true package name) and materialized
 * the packages. A dependency that resolves to a bundle-declaring package joins
 * the layer stack (appended in dependency order); a dependency-listed name
 * that no longer does — removed, or the installed version dropped the
 * declaration — leaves it. In-box bundles from the profile template are not
 * dependencies and are never touched. Warns once per newly-added bundle-less
 * dependency (a plain library is fine; the warning is orientation).
 * @param binName - the diagnostic prefix on thrown errors.
 * @param before - the manifest read before pnpm ran.
 * @param profileDir - the profile directory.
 * @param installAnchor - the cos installation anchor.
 */
export function reconcilePlugins(
  binName: string, before: ProfileManifest, profileDir: string, installAnchor: string,
): void {
  const after = readProfileManifest(binName, profileDir)
  const beforeDeps = new Set(Object.keys(before.dependencies ?? {}))
  const dependencies = Object.keys(after.dependencies ?? {})
  const plugins = after.dsh?.profile?.bundles ?? after.cos?.profile?.bundles ?? []
  let changed = false
  for (const packageName of dependencies) {
    const isBundle = exportsPatch(binName, packageName, installAnchor, profileDir)
    if (isBundle && !plugins.includes(packageName)) {
      plugins.push(packageName)
      changed = true
    } else if (!isBundle && !beforeDeps.has(packageName)) {
      process.stderr.write(
        `${binName}: warning: ${packageName} declares no dsh.bundle — installed as a plain dependency, not a profile layer `
        + '(a later update that gains one activates it automatically)\n',
      )
    }
  }
  const dependencySet = new Set(dependencies)
  for (const packageName of [...plugins]) {
    // Only dependency-managed entries are subject to removal; template
    // bundles are not dependencies.
    const wasDependency = beforeDeps.has(packageName) || dependencySet.has(packageName)
    const stillBundle = dependencySet.has(packageName) && exportsPatch(binName, packageName, installAnchor, profileDir)
    if (wasDependency && !stillBundle) {
      plugins.splice(plugins.indexOf(packageName), 1)
      changed = true
    }
  }
  if (!changed) return
  after.dsh = { ...after.dsh, profile: { ...after.dsh?.profile, bundles: plugins } }
  writeProfileManifest(profileDir, after)
}