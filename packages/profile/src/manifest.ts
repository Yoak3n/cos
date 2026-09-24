/**
 * @cos/profile/manifest — profile and bundle package.json manifests.
 *
 * The `dsh` section is the primary namespace (DSH ecosystem compatibility: a
 * bundle written for the DSH ecosystem declares `dsh.bundle.patch` and works
 * in a cos profile untouched). The `cos` section is accepted as an alias for
 * packages that prefer a cos-owned namespace.
 * @module @cos/profile/manifest
 */

import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

/** The bundle half of the manifest section: what a bundle package exports. */
export interface DshBundleManifest {
  /** The patch layer this bundle exports, relative to its package root. */
  patch: string
}

/** The profile half of the manifest section: what a profile directory composes. */
export interface DshProfileManifest {
  /** Ordered bundle layer list (package names). */
  bundles?: string[]
}

/** The manifest section; either the `dsh` or the `cos` key may be present. */
export interface DshManifestSection {
  /** Bundle metadata consumed by the profile launcher. */
  bundle?: DshBundleManifest
  /** Profile metadata consumed by the profile launcher. */
  profile?: DshProfileManifest
}

/** The slice of package.json both profiles and bundles use. */
export interface ProfileManifest {
  name?: string
  version?: string
  private?: boolean
  dependencies?: Record<string, string>
  peerDependencies?: Record<string, string>
  dsh?: DshManifestSection
  cos?: DshManifestSection
}

/**
 * The ordered bundle list of a manifest, from either namespace.
 * @returns the bundles list, or `undefined` when neither namespace declares one.
 */
export function manifestBundles(manifest: ProfileManifest): string[] | undefined {
  return manifest.dsh?.profile?.bundles ?? manifest.cos?.profile?.bundles
}

/** The bundle patch relative path of a manifest, from either namespace. */
export function manifestBundlePatch(manifest: ProfileManifest): string | undefined {
  return manifest.dsh?.bundle?.patch ?? manifest.cos?.bundle?.patch
}

/**
 * Read a profile's manifest.
 * @param binName - the diagnostic prefix on the thrown error.
 * @param dir - the profile directory.
 * @returns the parsed manifest.
 */
export function readProfileManifest(binName: string, dir: string): ProfileManifest {
  const path = join(dir, 'package.json')
  let raw: string
  try {
    raw = readFileSync(path, 'utf8')
  } catch (error) {
    throw new Error(`${binName}: failed to read profile manifest ${path}: ${String(error)}`)
  }
  // The field checks below validate the file data before trusting the parse type.
  const parsed = JSON.parse(raw) as ProfileManifest | null
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error(`${binName}: profile manifest ${path} must hold a JSON object`)
  }
  return parsed
}

/**
 * Write a profile's manifest back (2-space JSON, trailing newline).
 * @param dir - the profile directory.
 * @param manifest - the manifest value to persist.
 */
export function writeProfileManifest(dir: string, manifest: ProfileManifest): void {
  writeFileSync(join(dir, 'package.json'), JSON.stringify(manifest, undefined, 2) + '\n')
}