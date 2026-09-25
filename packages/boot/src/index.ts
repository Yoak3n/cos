/**
 * @cos/boot — shared launcher for every binary (CLI demo, sidecar server):
 * loads the gitignored .env, mounts Loader + Timer + HMR, composes the base
 * cordis.yml with ordered overlay patch layers, named bundle layers, and the
 * profile + home user patch layers (DSH-style profiles/bundles), settles the
 * tree, and fails loud when a required service is missing.
 *
 * Profile mode (`--profile <name>` / `BootOptions.profile`): DSH-aligned. The
 * profile directory under `<home>/profiles/<name>` supplies the bundle layers
 * (`dsh.profile.bundles`, two-anchor resolved: cos installation first, then
 * the profile) and its own `cordis.patch.yml`; the loader's `baseUrl` is
 * anchored at the profile so row names resolve from the profile's
 * pnpm-managed `node_modules`, backed by the healed flat fallback
 * `<home>/profiles/node_modules` for in-box packages.
 * @module @cos/boot
 */

import { existsSync, readFileSync, statSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { Context } from 'cordis'
import type { Context as ContextType } from 'cordis'
import Loader from '@cordisjs/plugin-loader'
import Include from '@cordisjs/plugin-include'
import type { PatchOptions } from '@cordisjs/plugin-include'
import Timer from '@cordisjs/plugin-timer'
import Hmr from '@cordisjs/plugin-hmr'
import { parse as parseYaml } from 'yaml'
import {
  DEFAULT_PROFILE_BUNDLES,
  PROFILE_PATCH_FILENAME,
  PROFILE_TEMPLATES,
  bundlePatchPath,
  healProfilesModuleFallback,
  initProfile,
  isBundleDir,
  manifestBundles,
  readBundleDeclaration,
  readProfileManifest,
  resolveBundleDir,
  resolveCosHome,
  resolveProfileDir,
} from '@cos/profile'
import { installErrorGuard } from './error-guard'

export interface BootOptions {
  /** Base composition file (absolute path to a cordis.yml). */
  configPath: string
  /** Overlay patch files applied in order on top of the base (absolute paths). */
  overlays?: readonly string[]
  /**
   * Named bundle layers applied after overlays, before the profile/home user
   * patches. Each entry is a bundle specifier (package name, path to a bundle
   * directory, or an alias registered via registerBundle) or an inline Bundle.
   */
  bundles?: readonly (string | Bundle)[]
  /** Extra programmatic patches applied after the user patch layers. */
  extraPatches?: readonly PatchOptions[]
  /** Services the launcher requires after settle (fail-loud). */
  required?: readonly string[]
  /**
   * 核心插件清单（包名或目录名）：其未捕获错误立即 panic。其余插件的
   * 未捕获错误走隔离 + 滑窗熔断（决策 6，见 error-guard.ts）；默认
   * DEFAULT_CORE_PLUGINS（@diver/backend、@diver/native-bridge）。
   */
  corePlugins?: readonly string[]
  /** HMR module watch roots; pass [] to disable module watching. */
  watchRoots?: readonly string[]
  /**
   * In-process plugin registry keyed by mount name (e.g. `'@cos/llm'`). When
   * present, the loader resolves those names from this map instead of via
   * `import()`, so a self-contained bundle (e.g. a Node SEA) needs no
   * node_modules at runtime. Names missing from the map fall back to normal
   * module resolution.
   */
  plugins?: Readonly<Record<string, unknown>>
  /**
   * Open direct plugin paths: mount name -> package directory or entry
   * file. The loader resolves these names to their sources directly — no
   * node_modules install needed (open plugins live at
   * `../cos-plugins/<name>` and are resolved by path). Values may be absolute
   * or relative to the process cwd.
   */
  pluginPaths?: Readonly<Record<string, string>>
  /**
   * Open plugin root: a directory where mounted row names resolve by
   * their unscoped package name (`@scope/memory` → `<root>/memory`). A
   * convenient alternative to an explicit pluginPaths map (used by the SEA /
   * CLI form: `--plugin-root ../cos-plugins`).
   */
  pluginRoot?: string
  /** Profile user patch path (cwd/cordis.patch.yml by default). */
  userPatchPath?: string
  /** Home user patch path (X_COS_HOME/cordis.patch.yml by default), applied
   * last — the outer program or user owns the final say. */
  homePatchPath?: string
  /**
   * DSH-aligned profile mode: boot the profile `<home>/profiles/<profile>`.
   * The profile's bundle layers and its own `cordis.patch.yml` join the
   * composition, and the loader resolves row names from the profile
   * directory. When absent, boots the flat (installation-level) tree.
   */
  profile?: string
  /** Cos home; defaults to COS_HOME || ~/.cos. */
  home?: string
  /**
   * Absolute path of the cos app's package.json — the installation anchor for
   * two-anchor bundle resolution and the healed flat module fallback.
   * Defaults to `<cwd>/package.json` when present.
   */
  installAnchor?: string
}

/** A resolved local bundle: patch rows plus the base rows it requires. */
export interface Bundle {
  /** Stable name used in diagnostics. */
  name: string
  /** Ordered include patches contributed by this bundle. */
  patches: PatchOptions[]
  /** Base rows this bundle requires to exist (validated, presentability). */
  requires: string[]
}

/** Default profile patch path (cwd cordis.patch.yml). */
export function defaultUserPatchPath(): string {
  return join(process.cwd(), 'cordis.patch.yml')
}

/** Default home patch path (~/.cos/cordis.patch.yml). */
export function defaultHomePatchPath(): string {
  const home = process.env.USERPROFILE || process.env.HOME || ''
  return join(home, '.cos', 'cordis.patch.yml')
}

/** Command-line invocation parsed into BootOptions (plus the positional config path). */
export interface CliOptions {
  /** Positional config path (default <cwd>/cordis.yml). */
  configPath: string
  /** Bundles to compose, in order. */
  bundles: string[]
  /** Overlay patch files, in order. */
  overlays: string[]
  /** User (profile) patch path. */
  patch: string | undefined
  /** Home patch file path (flat-mode: `--home`). */
  home: string | undefined
  /** Cos home directory (`--cos-home`; BootOptions.home), profile mode. */
  cosHome: string | undefined
  /** DSH-style profile name to boot (BootOptions.profile). */
  profile: string | undefined
  /** Open plugin root dir (`--plugin-root`). */
  pluginRoot: string | undefined
  /** Harness (engine) dir for the bundled runtime (`--harness`). */
  harness: string | undefined
  /** Agent prompt (CLI application flag). */
  prompt: string | undefined
  /** Agent provider/model (CLI application flags). */
  provider: string | undefined
  model: string | undefined
  /** Remaining unknown flags. */
  rest: string[]
}

function splitList(value: string | undefined): string[] {
  if (value === undefined || value === '') return []
  return value.split(',').map((entry) => entry.trim()).filter((entry) => entry !== '')
}

/** Parse argv into composition options; unknown flags are left in rest. */
export function parseCliArgs(argv: string[]): CliOptions {
  const out: CliOptions = {
    configPath: join(process.cwd(), 'cordis.yml'),
    bundles: [],
    overlays: [],
    patch: undefined,
    home: undefined,
    cosHome: undefined,
    profile: undefined,
    pluginRoot: undefined,
    harness: undefined,
    prompt: undefined,
    provider: undefined,
    model: undefined,
    rest: [],
  }
  let positional: string | undefined
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index]
    const value = argv[index + 1]
    if (flag === '--bundles') { out.bundles.push(...splitList(value)); index += 1 }
    else if (flag === '--overlays') { out.overlays.push(...splitList(value)); index += 1 }
    else if (flag === '--patch') { out.patch = value; index += 1 }
    else if (flag === '--home') { out.home = value; index += 1 }
    else if (flag === '--cos-home') { out.cosHome = value; index += 1 }
    else if (flag === '--profile') { out.profile = value; index += 1 }
    else if (flag === '--plugin-root') { out.pluginRoot = value; index += 1 }
    else if (flag === '--harness') { out.harness = value; index += 1 }
    else if (flag === '--config') { out.configPath = value ?? out.configPath; index += 1 }
    else if (flag === '--prompt') { out.prompt = value; index += 1 }
    else if (flag === '--provider') { out.provider = value; index += 1 }
    else if (flag === '--model') { out.model = value; index += 1 }
    else if (flag.startsWith('-')) {
      // Consume an immediately-following value so it is not mistaken for a
      // positional config path; an unknown flag's value is not meaningfully
      // an application argument.
      out.rest.push(flag)
      if (value !== undefined && !value.startsWith('-')) index += 1
    } else positional ??= flag
  }
  if (positional !== undefined) out.configPath = resolve(process.cwd(), positional)
  return out
}

/** Build BootOptions from parsed CLI args, filling user/home patch defaults.
 * Overlay layers come from the COS_OVERLAYS environment variable first (a
 * project-shell default), then any explicit --overlays CLI flags on top. */
export function bootOptionsFromCli(cli: CliOptions, extra: Partial<BootOptions> = {}): BootOptions {
  return {
    configPath: cli.configPath,
    bundles: [...cli.bundles],
    overlays: [
      ...overlaysFromEnv(),
      ...cli.overlays.map((entry) => resolve(process.cwd(), entry)),
    ],
    ...(cli.patch === undefined ? {} : { userPatchPath: resolve(process.cwd(), cli.patch) }),
    ...(cli.home === undefined ? {} : { homePatchPath: resolve(process.cwd(), cli.home) }),
    ...(cli.cosHome === undefined ? {} : { home: resolve(process.cwd(), cli.cosHome) }),
    ...(cli.profile === undefined ? {} : { profile: cli.profile }),
    ...(cli.pluginRoot === undefined ? {} : { pluginRoot: resolve(process.cwd(), cli.pluginRoot) }),
    ...extra,
  }
}

/** Parse one overlay/bundle patch file into include patch items. */
function parsePatchFile(file: string): PatchOptions[] {
  if (!existsSync(file)) return []
  const raw = readFileSync(file, 'utf8')
  const doc = parseYaml(raw) as unknown
  // null / empty / comment-only documents are valid (bundle base layers).
  if (doc === null || doc === undefined) return []
  const list = Array.isArray(doc)
    ? doc
    : typeof doc === 'object' && Array.isArray((doc as { patches?: unknown }).patches)
      ? (doc as { patches: unknown }).patches
      : null
  if (list === null) {
    throw new Error(`patch file ${file} must be a YAML list of patches (or { patches: [...] })`)
  }
  return list as PatchOptions[]
}

/** Read the base composition's row ids (for bundle `requires` validation). */
function readBaseRowIds(configPath: string): Set<string> {
  const ids = new Set<string>()
  try {
    const doc = parseYaml(readFileSync(configPath, 'utf8')) as Array<{ id?: unknown }>
    if (Array.isArray(doc)) for (const entry of doc) if (typeof entry?.id === 'string') ids.add(entry.id)
  } catch {
    // Base rows are advisory for requires validation; ignore read failures.
  }
  return ids
}

/**
 * Resolve one bundle specifier into a Bundle value. Package-style bundles
 * (DSH: a package.json `dsh.bundle.patch` declaration) and dir-style bundles
 * (`cordis.patch.yml` / `bundle.yml` in the package directory) are both
 * recognized; candidates are scanned from `node_modules/<spec>` (scoped too)
 * at the cwd and every ancestor — pnpm's hoisted linker places workspace
 * dependencies at the workspace root, which may sit above the process cwd —
 * then the top-level `bundles/<name>`, then the specifier as a path.
 */
function resolveBundle(spec: string | Bundle, registry: Map<string, Bundle>): Bundle {
  if (typeof spec !== 'string') return spec
  const registered = registry.get(spec)
  if (registered !== undefined) return registered
  const cwd = process.cwd()
  const scoped = spec.startsWith('@')
  const firstSlash = spec.indexOf('/')
  const unscoped = spec.slice(firstSlash >= 0 ? firstSlash + 1 : 0)
  const candidatesFor = (base: string) => [
    join(base, 'node_modules', spec),
    ...scoped ? [join(base, 'node_modules', spec.slice(0, firstSlash), unscoped)] : [],
  ]
  // cwd → ancestors: a workspace-root install hoists @scoped/packages there.
  let base = cwd
  for (;;) {
    for (const dir of candidatesFor(base)) {
      if (isBundleDir(dir)) {
        return {
          name: spec,
          patches: parsePatchFile(bundlePatchPath(dir)),
          requires: readBundleDeclaration(dir),
        }
      }
    }
    const parent = dirname(base)
    if (parent === base) break
    base = parent
  }
  // Re-check the top-level bundles/<name> at the cwd, then a local path.
  const localCandidates = [join(cwd, 'bundles', unscoped)]
  for (const dir of localCandidates) {
    if (isBundleDir(dir)) {
      return {
        name: spec,
        patches: parsePatchFile(bundlePatchPath(dir)),
        requires: readBundleDeclaration(dir),
      }
    }
  }
  const dir = resolve(process.cwd(), spec)
  return {
    name: spec,
    patches: parsePatchFile(bundlePatchPath(dir)),
    requires: readBundleDeclaration(dir),
  }
}

/** Read a package.json manifest, stripping a UTF-8 BOM if present (editors
 * such as Notepad/PowerShell may save with one; JSON.parse rejects it). */
function readManifest(file: string): { main?: string } | undefined {
  try {
    let raw = readFileSync(file, 'utf8')
    if (raw.charCodeAt(0) === 0xFEFF) raw = raw.slice(1)
    return JSON.parse(raw) as { main?: string }
  } catch {
    return undefined
  }
}

/**
 * Resolve a plugin row name from a plugin root directory: `@scope/pkg` maps
 * to `<root>/pkg` (unscoped), then the package entry is resolved from its
 * manifest. Returns the entry file path, or undefined when absent.
 */
function entryFromPluginRoot(root: string, name: string): string | undefined {
  const slash = name.indexOf('/')
  const pkg = slash >= 0 ? name.slice(slash + 1) : name
  const dir = join(resolve(process.cwd(), root), pkg)
  if (!existsSync(join(dir, 'package.json'))) return undefined
  const manifest = readManifest(join(dir, 'package.json'))
  if (manifest === undefined) return undefined
  const entry = resolve(dir, manifest.main ?? 'index.js')
  return existsSync(entry) ? entry : undefined
}

/**
 * Resolve a direct plugin path (BootOptions.pluginPaths) to its entry file:
 * a package directory (read `main` from its manifest) or a plain entry file.
 * @param value - the configured path, absolute or relative to the cwd.
 */
function entryFromPluginPath(value: string): string | undefined {
  const abs = resolve(process.cwd(), value)
  let stat
  try {
    stat = statSync(abs)
  } catch {
    return undefined
  }
  if (stat.isDirectory()) {
    const manifest = readManifest(join(abs, 'package.json'))
    if (manifest === undefined) return undefined
    const entry = resolve(abs, manifest.main ?? 'index.js')
    return existsSync(entry) ? entry : undefined
  }
  return abs
}

/**
 * Resolve one package from a profile anchor to its main entry file WITHOUT
 * realpath'ing: `require.resolve` follows symlinks to the real directory,
 * which breaks out-of-tree plugin resolution (their dependencies would resolve
 * from the real path's parent chain, missing the profile's node_modules and
 * the healed flat fallback). Instead we walk Node's own node_modules lookup
 * order and keep the symlink (junction) form, so the loaded module resolves
 * its own dependencies from the profile-relative chain.
 */
function resolveProfilePackage(
  profileRequire: ReturnType<typeof createRequire>, name: string,
): string | undefined {
  // resolve.paths returns null only for builtins, which no plugin name is.
  /* v8 ignore next */
  for (const searchPath of profileRequire.resolve.paths(name) ?? []) {
    const dir = join(searchPath, name)
    const manifestPath = join(dir, 'package.json')
    if (!existsSync(manifestPath)) continue
    const manifest = readManifest(manifestPath)
    if (manifest === undefined) continue
    const entry = resolve(dir, manifest.main ?? 'index.js')
    if (existsSync(entry)) return entry
  }
  return undefined
}

/**
 * Boot the Loader tree and return only after the whole tree settles.
 * @param options - composition root, overlay layers, bundles, required services.
 */
/** Collect row ids explicitly disabled by any override patch layer. */
function collectDisabledIds(layers: ReadonlyArray<readonly PatchOptions[]>): Set<string> {
  const ids = new Set<string>()
  for (const layer of layers) {
    for (const patch of layer) {
      if (patch === null || typeof patch !== 'object' || Array.isArray(patch)) continue
      const entry = patch as Record<string, unknown>
      if (entry['disabled'] && typeof entry['id'] === 'string') {
        ids.add(entry['id'])
      }
    }
  }
  return ids
}

/**
 * Drop insert rows whose id is disabled by a later override layer.
 *
 * `@cordisjs/plugin-include` builds its entry index from the base tree only;
 * rows inserted in the same patch batch are not yet addressable, so a later
 * `{ id, disabled: true }` would miss them. Filtering inserts here keeps the
 * DSH “disable = profile patch” contract without requiring a loader change.
 */
function filterDisabledInserts(
  patches: readonly PatchOptions[],
  disabled: ReadonlySet<string>,
): PatchOptions[] {
  if (disabled.size === 0) return patches.map((patch) => patch)
  return patches.map((patch) => {
    if (patch === null || typeof patch !== 'object' || Array.isArray(patch)) return patch
    const entry = patch as Record<string, unknown>
    const insert = entry['insert']
    if (!Array.isArray(insert)) return patch
    const kept = insert.filter((row) => {
      const id = (row as { id?: unknown } | null)?.id
      return !(typeof id === 'string' && disabled.has(id))
    })
    return { ...entry, insert: kept } as PatchOptions
  })
}

export async function boot(options: BootOptions): Promise<ContextType> {
  const {
    configPath,
    overlays = [],
    bundles = [] as Array<string | Bundle>,
    extraPatches = [],
    watchRoots = ['.'],
    required = [],
    corePlugins,
    plugins,
    pluginPaths,
    pluginRoot,
    userPatchPath: userPatchOption,
    homePatchPath: homePatchOption,
    profile,
    home,
    installAnchor,
  } = options
  try {
    process.loadEnvFile()
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
  }

  // ── DSH-aligned profile mode ─────────────────────────────────────────────
  const anchor = installAnchor ?? (existsSync(join(process.cwd(), 'package.json'))
    ? join(process.cwd(), 'package.json')
    : undefined)
  const cosHome = home ?? resolveCosHome()
  let profileDir: string | undefined
  if (profile !== undefined) {
    profileDir = resolveProfileDir(profile, cosHome)
    // First use initializes an empty profile (mirrors DSH's template init);
    // the bundle dependencies are added with `pnpm plugin --profile <name> add …`.
    if (!existsSync(join(profileDir, 'package.json'))) {
      initProfile(profileDir, PROFILE_TEMPLATES[profile] ?? DEFAULT_PROFILE_BUNDLES)
      console.warn(
        `[cos] initialized empty profile ${profile} at ${profileDir} — `
        + `add bundle dependencies with 'pnpm plugin --profile ${profile} add <package>'`,
      )
    }
    // Heal the flat module fallback so in-box @cos/* packages resolve from
    // the profile through the ordinary parent-walk.
    if (anchor !== undefined) {
      try {
        healProfilesModuleFallback(anchor, cosHome)
      } catch (error) {
        console.warn(`[cos] failed to heal profile module fallback: ${String(error)}`)
      }
    }
  }

  const ctx = new Context()
  ctx.baseUrl = pathToFileURL(profileDir ?? dirname(configPath)).href + '/'
  await ctx.plugin(Loader)
  // HMR: module + config watchers; edit any file under packages/ and the tree
  // reloads without a restart (dev loop).
  await ctx.plugin(Timer)
  if (watchRoots.length > 0) {
    await ctx.plugin(Hmr, { root: [...watchRoots], ignored: [], debounce: 50 })
  }
  ctx.loader.builtins.include = Include
  // Replace the loader's module resolver with the in-process plugin registry
  // when one is supplied (SEA bundles), and/or a profile-anchored resolver so
  // out-of-tree plugins from the profile's node_modules load in dev and SEA
  // alike. Unregistered/unresolvable names fall back to the normal resolver.
  if (plugins !== undefined || pluginPaths !== undefined || pluginRoot !== undefined || profileDir !== undefined) {
    const rawInternal = ctx.loader.internal as Record<string, unknown> | undefined
    // The original internal's `import` needs its `this` (v1/v2 module
    // machinery); bind it before delegating.
    const rawImport = rawInternal?.['import'] as ((name: string, parent: string, options: object) => Promise<unknown>) | undefined
    const fallbackImport = rawImport === undefined ? undefined : rawImport.bind(rawInternal)
    const profileRequire = profileDir === undefined ? undefined : createRequire(join(profileDir, 'package.json'))
    const replacement: Record<string, unknown> = {
      import: async (name: string): Promise<unknown> => {
        const bundled = plugins?.[name]
        if (bundled !== undefined) return bundled
        const direct = pluginPaths?.[name]
        if (direct !== undefined) {
          const entry = entryFromPluginPath(direct)
          if (entry !== undefined) {
            try {
              return await import(pathToFileURL(entry).href)
            } catch (error) {
              console.error(`[cos] loader import failed for ${name} -> ${entry}: ${error}`)
              throw error
            }
          }
        }
        if (pluginRoot !== undefined) {
          const entry = entryFromPluginRoot(pluginRoot, name)
          if (entry !== undefined) {
            try {
              return await import(pathToFileURL(entry).href)
            } catch (error) {
              console.error(`[cos] loader import failed for ${name} -> ${entry}: ${error}`)
              throw error
            }
          }
        }
        if (profileRequire !== undefined) {
          const entry = resolveProfilePackage(profileRequire, name)
          if (entry !== undefined) {
            try {
              return await import(pathToFileURL(entry).href)
            } catch (error) {
              console.error(`[cos] loader import failed for ${name} -> ${entry}: ${error}`)
              throw error
            }
          }
        }
        if (fallbackImport !== undefined) return fallbackImport(name, ctx.baseUrl ?? import.meta.url, {})
        return import(name)
      },
    }
    // Replace `internal` while carrying over the original object's other
    // members (the HMR plugin reads `loader.internal.loadCache` from it).
    if (rawInternal !== undefined) {
      for (const key of Object.keys(rawInternal)) {
        if (key === 'import') continue
        replacement[key] = rawInternal[key]
      }
    }
    ctx.loader.internal = replacement as never
  }

  // ── Layer composition ────────────────────────────────────────────────────
  // Profile bundles first (dsh.profile.bundles, two-anchor), then the
  // explicit --bundles aggregate; both may be empty.
  const bundleRegistry = new Map<string, Bundle>()
  const bundlePatches: PatchOptions[] = []
  const baseRowIds = readBaseRowIds(configPath)
  const failRequires = (bundle: Bundle): void => {
    // A bundle's `requires` must name rows present in the base layer — the
    // bundle cannot be inserted onto a config root missing its dependencies.
    for (const req of bundle.requires) {
      if (!baseRowIds.has(req)) {
        throw new Error(
          `bundle "${bundle.name}" requires base row "${req}", which is absent from ${configPath}; `
          + `mount the base row before this bundle`,
        )
      }
    }
  }
  if (profileDir !== undefined && anchor !== undefined) {
    const manifest = readProfileManifest('cos', profileDir)
    for (const packageName of manifestBundles(manifest) ?? []) {
      const dir = resolveBundleDir('cos', packageName, anchor, profileDir)
      const bundle: Bundle = {
        name: packageName,
        patches: parsePatchFile(bundlePatchPath(dir)),
        requires: readBundleDeclaration(dir),
      }
      bundleRegistry.set(packageName, bundle)
      failRequires(bundle)
      bundlePatches.push(...bundle.patches)
    }
  }
  const aggregate: Array<string | Bundle> = [...bundles]
  for (const spec of aggregate) {
    const bundle = resolveBundle(spec, bundleRegistry)
    if (typeof spec === 'string') bundleRegistry.set(spec, bundle)
    failRequires(bundle)
    bundlePatches.push(...bundle.patches)
  }

  const overlayPatches = overlays.flatMap((file) => parsePatchFile(file))
  // In profile mode the profile's own cordis.patch.yml is the user layer; the
  // flat-mode cwd default is skipped unless a patch was explicitly requested.
  const effectiveUserPatch = userPatchOption ?? (profileDir === undefined ? defaultUserPatchPath() : undefined)
  const effectiveHomePatch = homePatchOption ?? (profileDir === undefined ? defaultHomePatchPath() : join(cosHome, PROFILE_PATCH_FILENAME))
  const profilePatches = profileDir !== undefined && existsSync(join(profileDir, PROFILE_PATCH_FILENAME))
    ? parsePatchFile(join(profileDir, PROFILE_PATCH_FILENAME))
    : []
  const userPatches = effectiveUserPatch !== undefined && existsSync(effectiveUserPatch)
    ? parsePatchFile(effectiveUserPatch)
    : []
  const homePatches = existsSync(effectiveHomePatch) ? parsePatchFile(effectiveHomePatch) : []
  // Profile/user/home layers may disable rows that bundles only insert in this
  // same batch — filter those inserts so disable actually takes effect.
  const disabledIds = collectDisabledIds([
    overlayPatches,
    profilePatches,
    userPatches,
    homePatches,
    extraPatches,
  ])
  const patches = [
    ...filterDisabledInserts(overlayPatches, disabledIds),
    ...filterDisabledInserts(bundlePatches, disabledIds),
    ...profilePatches,
    ...userPatches,
    ...homePatches,
    ...extraPatches,
  ]
  await ctx.loader.create({
    name: 'cordis:include',
    config: {
      path: pathToFileURL(configPath).href,
      ...(patches.length === 0 ? {} : { patches }),
    },
  })
  await ctx.loader.await()

  // Fail loud: a missing row (e.g. no llm provider) would otherwise surface
  // much later as a cryptic `undefined` service on first use.
  const missing = required.filter((name) => ctx.get(name) === undefined)
  if (missing.length > 0) {
    await ctx.fiber.dispose()
    throw new Error(`boot: services unavailable: ${missing.join(', ')} (check the mounted cordis.yml rows)`)
  }
  // 决策 6：boot 后未捕获错误分级兜底——非核心插件隔离+熔断，引擎/核心插件 panic。
  installErrorGuard({ ctx, ...(corePlugins === undefined ? {} : { corePlugins }) })
  return ctx
}

/** Resolve overlay file paths from a comma-separated env value. */
export function overlaysFromEnv(env = process.env.COS_OVERLAYS ?? ''): string[] {
  return env.split(',').map((entry) => entry.trim()).filter((entry) => entry !== '')
    .map((entry) => (join(process.cwd(), entry)))
}
