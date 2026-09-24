/**
 * @cos/profile/home — cos home and profile directory resolution.
 *
 * DSH-aligned: a profile is a directory under `<home>/profiles/<name>`
 * holding its own `package.json` (out-of-tree plugin dependencies plus the
 * profile manifest `dsh.profile` with its ordered `bundles` list) and a
 * `cordis.patch.yml` (the profile's own patch layer, applied after every
 * bundle layer). The cos home defaults to `COS_HOME` (the Tauri shell drives
 * `COS_HOME=<repo>/harness/.cos-home`), else `~/.cos`.
 * @module @cos/profile/home
 */

import { homedir } from 'node:os'
import { join } from 'node:path'

/** Directory under the cos home holding every profile. */
export const PROFILES_DIR = 'profiles'

/** The user patch layer inside a profile directory. */
export const PROFILE_PATCH_FILENAME = 'cordis.patch.yml'

/** The launcher-maintained flat module fallback sibling name. */
export const PROFILES_MODULE_FALLBACK = 'node_modules'

/**
 * Resolve the cos home: `COS_HOME`, else `~/.cos`.
 * @param env - the environment to read (overridable for tests).
 */
export function resolveCosHome(env: NodeJS.ProcessEnv = process.env): string {
  return env.COS_HOME || join(homedir(), '.cos')
}

/**
 * Resolve a profile's directory under the cos home.
 * @param name - the profile name (`--profile <name>`).
 * @param home - the cos home; defaults to {@link resolveCosHome}.
 * @returns the absolute profile directory (which may not exist yet).
 */
export function resolveProfileDir(name: string, home: string = resolveCosHome()): string {
  if (name === '' || name.includes('/') || name.includes('\\') || name === '.' || name === '..'
    // The launcher-maintained flat module fallback lives at this sibling path.
    || name === PROFILES_MODULE_FALLBACK) {
    throw new Error(`cos: invalid profile name ${JSON.stringify(name)}`)
  }
  return join(home, PROFILES_DIR, name)
}