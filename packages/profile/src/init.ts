/**
 * @cos/profile/init — profile directory initialization (DSH-aligned).
 *
 * A profile is brought into existence by `pnpm plugin --profile <name> …`
 * (or by booting a profile that does not exist yet, which auto-initializes an
 * empty one). Existing files are never touched, so re-running is a no-op.
 * @module @cos/profile/init
 */

import { existsSync, mkdirSync, writeFileSync } from 'node:fs'
import { basename, join } from 'node:path'
import { PROFILE_PATCH_FILENAME } from './home.ts'
import type { ProfileManifest } from './manifest.ts'
import { writeProfileManifest } from './manifest.ts'

/** The bundle list an auto-init uses for a name with no shipped template. */
export const DEFAULT_PROFILE_BUNDLES: readonly string[] = []

/** Shipped profile templates auto-initialized on first use (empty: the
 * harness's own `cordis.yml` IS the base, unlike DSH's in-box base bundle). */
export const PROFILE_TEMPLATES: Record<string, readonly string[]> = {}

const PROFILE_PATCH_TEMPLATE = `# Your patch layer for this cos profile, applied after every bundle layer:
# a top-level YAML array of loader patch entries (id-targeted config
# overrides, disables, and insert lists).
[]
`

// The hoisted linker gives out-of-tree plugins a flat node_modules whose
// missing peers (cordis and friends) fall through to the healed
// profiles/node_modules installation fallback, so every plugin shares the
// installation's single cordis instance instead of a duplicate. pnpm ≥10
// reads its settings from pnpm-workspace.yaml, not .npmrc.
const PROFILE_PNPM_WORKSPACE = `packages:
  - .

nodeLinker: hoisted
autoInstallPeers: false
`

/**
 * Initialize a profile directory: manifest, empty user patch layer, and the
 * pnpm settings out-of-tree plugins need. Existing files are never touched,
 * so re-running is a no-op on an initialized profile.
 * @param dir - the profile directory from {@link resolveProfileDir}.
 * @param bundles - the initial `dsh.profile.bundles` layer list.
 */
export function initProfile(dir: string, bundles: readonly string[]): void {
  mkdirSync(dir, { recursive: true })
  const manifestPath = join(dir, 'package.json')
  if (!existsSync(manifestPath)) {
    const manifest: ProfileManifest & { private: boolean } = {
      name: `cos-profile-${basename(dir)}`,
      private: true,
      dependencies: {},
      dsh: { profile: { bundles: [...bundles] } },
    }
    writeProfileManifest(dir, manifest)
  }
  const patchPath = join(dir, PROFILE_PATCH_FILENAME)
  if (!existsSync(patchPath)) writeFileSync(patchPath, PROFILE_PATCH_TEMPLATE)
  const workspacePath = join(dir, 'pnpm-workspace.yaml')
  if (!existsSync(workspacePath)) writeFileSync(workspacePath, PROFILE_PNPM_WORKSPACE)
}