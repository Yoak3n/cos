/**
 * @cos/boot — shared launcher for every binary (CLI demo, sidecar server):
 * loads the gitignored .env, mounts Loader + Timer + HMR, composes the base
 * cordis.yml with ordered overlay patch layers, named bundle layers, and the
 * profile + home user patch layers (DSH-style profiles/bundles), settles the
 * tree, and fails loud when a required service is missing.
 * @module @cos/boot
 */

import { existsSync, readFileSync } from 'node:fs'
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
  /** HMR module watch roots; pass [] to disable module watching. */
  watchRoots?: readonly string[]
  /** Profile user patch path (cwd/cordis.patch.yml by default). */
  userPatchPath?: string
  /** Home user patch path (X_COS_HOME/cordis.patch.yml by default), applied
   * last — the outer program or user owns the final say. */
  homePatchPath?: string
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
  /** Home patch path. */
  home: string | undefined
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

/** Read a bundle's bundle.yml declaration file. */
function readBundleDeclaration(dir: string): { requires: string[] } {
  const decl = join(dir, 'bundle.yml')
  if (!existsSync(decl)) return { requires: [] }
  const doc = parseYaml(readFileSync(decl, 'utf8')) as { bundle?: { requires?: unknown } }
  const requires = doc?.bundle?.requires
  if (!Array.isArray(requires)) return { requires: [] }
  return { requires: requires.map((entry) => String(entry)) }
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

/** Resolve one bundle specifier into a Bundle value. */
function resolveBundle(spec: string | Bundle, registry: Map<string, Bundle>): Bundle {
  if (typeof spec !== 'string') return spec
  const registered = registry.get(spec)
  if (registered !== undefined) return registered
  // Candidate directory: node_modules/<spec>, then scoped node_modules/@x/<y>,
  // then workspace packages/<name> — resolving without relying on a
  // package.json ./package.json subpath export.
  const cwd = process.cwd()
  const scoped = spec.startsWith('@')
  const firstSlash = spec.indexOf('/')
  const unscoped = spec.slice(firstSlash >= 0 ? firstSlash + 1 : 0)
  const candidates = [
    join(cwd, 'node_modules', spec),
    ...scoped ? [join(cwd, 'node_modules', spec.slice(0, firstSlash), unscoped)] : [],
    join(cwd, 'packages', unscoped),
  ]
  for (const dir of candidates) {
    const patch = join(dir, 'cordis.patch.yml')
    const decl = join(dir, 'bundle.yml')
    if (existsSync(patch) || existsSync(decl)) {
      return {
        name: spec,
        patches: parsePatchFile(patch),
        requires: readBundleDeclaration(dir).requires,
      }
    }
  }
  // Fall back to a local filesystem path.
  const dir = resolve(process.cwd(), spec)
  return {
    name: spec,
    patches: parsePatchFile(join(dir, 'cordis.patch.yml')),
    requires: readBundleDeclaration(dir).requires,
  }
}

/**
 * Boot the Loader tree and return only after the whole tree settles.
 * @param options - composition root, overlay layers, bundles, required services.
 */
export async function boot(options: BootOptions): Promise<ContextType> {
  const {
    configPath,
    overlays = [],
    bundles = [] as Array<string | Bundle>,
    extraPatches = [],
    watchRoots = ['.'],
    required = [],
    userPatchPath = defaultUserPatchPath(),
    homePatchPath = defaultHomePatchPath(),
  } = options
  try {
    process.loadEnvFile()
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
  }
  const ctx = new Context()
  ctx.baseUrl = pathToFileURL(dirname(configPath)).href + '/'
  await ctx.plugin(Loader)
  // HMR: module + config watchers; edit any file under packages/ and the tree
  // reloads without a restart (dev loop).
  await ctx.plugin(Timer)
  if (watchRoots.length > 0) {
    await ctx.plugin(Hmr, { root: [...watchRoots], ignored: [], debounce: 50 })
  }
  ctx.loader.builtins.include = Include
  // Aggregate bundles from inline values plus the COS_BUNDLES environment
  // specifier list, in order.
  const aggregate: Array<string | Bundle> = [...bundles]
  const bundleRegistry = new Map<string, Bundle>()
  const bundlePatches: PatchOptions[] = []
  const baseRowIds = readBaseRowIds(configPath)
  for (const spec of aggregate) {
    const bundle = resolveBundle(spec, bundleRegistry)
    if (typeof spec === 'string') bundleRegistry.set(spec, bundle)
    // A bundle's `requires` must name rows present in the base layer — the
    // bundle cannot be inserted onto a config root missing its dependencies.
    for (const req of bundle.requires) {
      if (!baseRowIds.has(req)) {
        await ctx.fiber.dispose()
        throw new Error(
          `bundle "${bundle.name}" requires base row "${req}", which is absent from ${configPath}; `
            + `mount the base row before this bundle`,
        )
      }
    }
    bundlePatches.push(...bundle.patches)
  }

  const overlayPatches = overlays.flatMap((file) => parsePatchFile(file))
  const userPatches = existsSync(userPatchPath) ? parsePatchFile(userPatchPath) : []
  const homePatches = existsSync(homePatchPath) ? parsePatchFile(homePatchPath) : []
  const patches = [...overlayPatches, ...bundlePatches, ...userPatches, ...homePatches, ...extraPatches]
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
  return ctx
}

/** Resolve overlay file paths from a comma-separated env value. */
export function overlaysFromEnv(env = process.env.COS_OVERLAYS ?? ''): string[] {
  return env.split(',').map((entry) => entry.trim()).filter((entry) => entry !== '')
    .map((entry) => (join(process.cwd(), entry)))
}