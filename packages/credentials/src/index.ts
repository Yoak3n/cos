/**
 * @cos/credentials — unified credential seam (`ctx.credentials`). Consumers
 * (e.g. LLM adapters) resolve secrets here instead of reading process.env or
 * a secrets file directly, so the source of credentials, their storage, and
 * fail-fast diagnostics live in one place. A credential may be sourced from the
 * environment (an explicit `envKey`) or from a key in a YAML/JSON secrets file
 * configured on this plugin (an explicit `key`); production points the file at
 * any backend (env, file, vault, …) behind the same interface.
 * @module @cos/credentials
 */

import { Service } from 'cordis'
import type { Context } from 'cordis'
import { readFileSync } from 'node:fs'
import { isAbsolute, resolve } from 'node:path'
import { parse as parseYaml } from 'yaml'

/** Secrets-file plugin config — the deployment's chosen storage for file-backed keys. */
export interface CredentialsConfig {
  /** Path to the YAML/JSON secrets file (relative paths resolve against the cwd). */
  file?: string
}

/**
 * Declared credential source, resolved on first read. Exactly one of `envKey`
 * or `key` should be set; when both are absent the credential is unresolved.
 */
export interface CredentialSpec {
  /** Environment variable that holds the secret, when sourced from the env. */
  envKey?: string
  /** Dot-path into the configured secrets file, e.g. `deepseek.apiKey`. */
  key?: string
}

export interface RegisterOptions {
  /** Whether an empty value is an error. Defaults to true (fail loud). */
  required?: boolean
  /** Human-readable label naming the setting to fix, for diagnostics. */
  ref?: string
}

interface Entry {
  spec: CredentialSpec
  required: boolean
  ref: string
  cached?: string
}

declare module 'cordis' {
  interface Context {
    credentials: CredentialsService
  }
}

/** Resolve a dot-path (`deepseek.apiKey`) against a parsed document. */
function resolveKey(root: unknown, path: string): unknown {
  let cursor = root
  for (const segment of path.split('.')) {
    if (typeof cursor !== 'object' || cursor === null) return undefined
    cursor = (cursor as Record<string, unknown>)[segment]
  }
  return cursor
}

export class CredentialsService extends Service {
  static inject: string[] = []
  private readonly entries = new Map<string, Entry>()
  private secrets?: Record<string, unknown>
  private secretsError?: string
  private readonly file?: string

  constructor(ctx: Context, config: CredentialsConfig = {}) {
    super(ctx, 'credentials')
    this.file = config.file
  }

  /**
   * Declare one credential and its source. Resolves lazily on the first
   * `get()` so a boot can register many without failing on unrelated gaps.
   * @param ref - stable credential identity, e.g. `deepseek.apiKey`.
   * @param spec - the source (env variable name and/or secrets-file key).
   * @param options - required/ref for diagnostics.
   * @returns the disposer removing this registration.
   */
  provide(ref: string, spec: CredentialSpec, options: RegisterOptions = {}): () => void {
    this.entries.set(ref, {
      spec,
      required: options.required ?? true,
      ref: options.ref ?? ref,
    })
    return () => { this.entries.delete(ref) }
  }

  /** Drop cached secrets/values so the next get() re-reads the file. */
  invalidate(ref?: string): void {
    if (ref === undefined) {
      this.secrets = undefined
      this.secretsError = undefined
      for (const entry of this.entries.values()) entry.cached = undefined
      return
    }
    const entry = this.entries.get(ref)
    if (entry) entry.cached = undefined
    this.secrets = undefined
    this.secretsError = undefined
  }

  /** The configured secrets file path (as configured, before cwd resolution),
   * so writers (e.g. a settings panel) persist into exactly the file this
   * service reads. Undefined when no file source is configured. */
  get secretFile(): string | undefined {
    return this.file
  }

  /** Read one credential. Throws with a configuration-point diagnostic when
   * it is required and unresolvable. */
  get(ref: string): string {
    const entry = this.entries.get(ref)
    if (entry === undefined) {
      throw new Error(`credentials: no provider registered for "${ref}"`)
    }
    let value = entry.cached
    if (value === undefined) {
      if (entry.spec.envKey !== undefined) {
        value = process.env[entry.spec.envKey] ?? ''
        if (value !== '') entry.cached = value
      }
      if (entry.spec.key !== undefined && (value === undefined || value === '')) {
        const fromFile = this.readSecret(entry.spec.key)
        if (fromFile !== '') {
          value = fromFile
          entry.cached = fromFile
        }
      }
      const resolved = value ?? ''
      if (entry.required && resolved === '') {
        throw new Error(this.unresolvedMessage(entry))
      }
      return resolved
    }
    return value
  }

  /** Resolve one dot-path against the configured secrets file ('' when absent). */
  private readSecret(path: string): string {
    if (this.secrets === undefined) {
      if (this.file === undefined) {
        this.secretsError = 'no secrets file configured; set credentials.config.file'
        return ''
      }
      const file = isAbsolute(this.file) ? this.file : resolve(process.cwd(), this.file)
      try {
        const raw = readFileSync(file, 'utf8')
        const doc = parseYaml(raw) as unknown
        if (typeof doc === 'object' && doc !== null) {
          this.secrets = doc as Record<string, unknown>
        } else {
          this.secretsError = `secrets file ${file} must be a mapping`
          return ''
        }
      } catch (error) {
        this.secretsError = `cannot read secrets file ${file}: ${(error as Error).message}`
        return ''
      }
    }
    if (this.secretsError !== undefined) return ''
    const found = resolveKey(this.secrets, path)
    return typeof found === 'string' ? found : ''
  }

  /** Build the fail-loud diagnostic naming the source to fix. */
  private unresolvedMessage(entry: Entry): string {
    const parts: string[] = []
    if (entry.spec.envKey !== undefined) parts.push(`env ${entry.spec.envKey}`)
    if (entry.spec.key !== undefined) parts.push(`key "${entry.spec.key}" in the secrets file`)
    const sources = parts.length > 0 ? `; set ${parts.join(' or ')}` : ''
    const fileHint = this.secretsError !== undefined ? ` (${this.secretsError})` : ''
    return `credentials: "${entry.ref}" is required and unresolved${sources} (ref: ${entry.ref})${fileHint}`
  }
}

export default CredentialsService
