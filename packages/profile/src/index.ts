/**
 * @cos/profile — DSH-aligned profile model for the cos harness.
 *
 * A profile is a directory under `<home>/profiles/<name>` holding its own
 * `package.json` (out-of-tree plugin dependencies plus the profile manifest
 * `dsh.profile` with its ordered `bundles` list), a `node_modules` pnpm
 * manages for those plugins, and a `cordis.patch.yml` (the profile's own user
 * patch layer, applied after every bundle layer). Module resolution is
 * two-anchor by construction: a bundle name resolves first from the cos
 * installation, then from the profile directory; the maintained flat fallback
 * `<home>/profiles/node_modules` (one symlink per package the installation
 * depends on) makes every in-box plugin Node-resolvable from any profile
 * through the ordinary parent-walk.
 *
 * Ported from the DeepSeek Harness profile machinery
 * (`@deepseek-ai/dsh-app-boot/profile` + the launcher's pnpm forwarder) and
 * adapted to the cos namespacing: the `dsh` manifest section stays the
 * primary namespace for ecosystem compatibility, `cos` is accepted as an
 * alias.
 * @module @cos/profile
 */

export * from './home.ts'
export * from './manifest.ts'
export * from './resolve.ts'
export * from './init.ts'
export * from './fallback.ts'
export * from './reconcile.ts'