/**
 * @cos/tools — tool registry (`ctx.tools`): named executors the loop
 * schedules after the model requests calls. A real deployment replaces this
 * with guarded pipeline events (`tools/pre-execute` …).
 * @module @cos/tools
 */

import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { ToolDefinition, ToolSchema } from '@cos/types'

export interface ToolResult {
  content: string
  /** 工具返回的图片（如 read 读图）：data 为 base64，不含 data: 前缀。 */
  images?: Array<{ mime: string; data: string; name?: string }>
  isError?: boolean
  concludesTurn?: boolean
}

export type ToolExecutor = (args: unknown, signal: AbortSignal) => Promise<ToolResult>

/** Options a tool can declare for its wire schema. */
export interface ToolOptions {
  description?: string
  /** OpenAI-style JSON schema for the arguments object. */
  parameters?: Record<string, unknown>
}

/**
 * DSH-shaped registered tool (duck-typed, as produced by `defineTool`).
 * This is the **canonical** cos registration shape: `ctx.tools.register(defineTool({...}))`.
 * The triple form `register(name, executor, options)` remains as a thin alias.
 */
export interface DshToolDefinition {
  name: string
  description: string
  parameters: Record<string, unknown>
  execute(args: unknown, exec: { signal: AbortSignal; [key: string]: unknown }): Promise<unknown>
  output?: {
    schema?: unknown
    render?(args: unknown, value: unknown): Array<{ type: string; text?: string; [key: string]: unknown }>
    presentationMeta?(args: unknown, value: unknown): unknown
  }
  timeoutMs?: number
  finalizeContent?(exec: unknown, result: unknown): unknown
  isConcurrencySafe?(args: unknown): boolean
  presentCall?(args: unknown): unknown
  presentResult?(args: unknown, result: unknown): unknown
}

/**
 * Canonical authoring helper (DSH `defineTool`). Re-exported here so cos
 * plugins can `import { defineTool } from '@cos/plugin-api'` (or `@cos/tools`)
 * with the same call shape as `@deepseek-ai/dsh-tools`.
 */
export { defineTool } from '@deepseek-ai/dsh-tools/schema'
export type {
  DefineToolOptions,
  ParameterSchemaSpec,
  ParameterPropertySpec,
  ValueSchemaSpec,
} from '@deepseek-ai/dsh-tools/schema'

interface Entry {
  executor: ToolExecutor
  schema: ToolSchema
  dsh?: DshToolDefinition
}

declare module 'cordis' {
  interface Context {
    tools: ToolsService
  }
}

function isDshToolDefinition(value: unknown): value is DshToolDefinition {
  return (
    typeof value === 'object'
    && value !== null
    && typeof (value as DshToolDefinition).name === 'string'
    && typeof (value as DshToolDefinition).execute === 'function'
    && typeof (value as DshToolDefinition).parameters === 'object'
  )
}

function contentBlocksToText(blocks: Array<{ type: string; text?: string; [key: string]: unknown }>): string {
  return blocks
    .map((block) => (block.type === 'text' && typeof block.text === 'string' ? block.text : JSON.stringify(block)))
    .join('\n')
}

/** Wrap a DSH `defineTool` body into the cos string-result executor. */
export function dshToolExecutor(definition: DshToolDefinition): ToolExecutor {
  return async (args, signal) => {
    try {
      const exec = { signal, arguments: args }
      const value = await definition.execute(args, exec)
      const render = definition.output?.render
      const content = typeof render === 'function'
        ? contentBlocksToText(render(args, value) ?? [])
        : typeof value === 'string'
          ? value
          : JSON.stringify(value ?? null)
      return { content }
    } catch (error: unknown) {
      return { content: error instanceof Error ? error.message : String(error), isError: true }
    }
  }
}

export class ToolsService extends Service {
  static inject: string[] = []
  private readonly registry = new Map<string, Entry>()

  constructor(ctx: Context) {
    super(ctx, 'tools')
  }

  /** DSH shape: `register(defineTool({...}))`. */
  register(definition: DshToolDefinition): () => void
  /** cos shape: `register(name, executor, options)`. */
  register(name: string, executor: ToolExecutor, options?: ToolOptions): () => void
  /** Register one tool (cos triple or DSH definition); returns the disposer. */
  register(
    nameOrDefinition: string | DshToolDefinition,
    executor?: ToolExecutor,
    options: ToolOptions = {},
  ): () => void {
    if (isDshToolDefinition(nameOrDefinition)) {
      const definition = nameOrDefinition
      return this.register(definition.name, dshToolExecutor(definition), {
        description: definition.description,
        parameters: definition.parameters,
      })
    }
    const name = nameOrDefinition
    if (typeof executor !== 'function') {
      throw new TypeError(`tools.register("${name}"): executor is required`)
    }
    this.registry.set(name, {
      executor,
      schema: {
        description: options.description ?? `Call the tool "${name}"`,
        parameters: options.parameters ?? { type: 'object', properties: {} },
      },
    })
    return () => { this.registry.delete(name) }
  }

  /** DSH alias: visible schemas in registration order. */
  schemas(): Array<{ name: string; description: string; parameters: Record<string, unknown> }> {
    return this.listDefinitions().map(({ name, schema }) => ({
      name,
      description: schema.description,
      parameters: schema.parameters,
    }))
  }

  /** DSH-ish lookup: the registered entry (schema only; execute is internal). */
  get(name: string): { name: string; description: string; parameters: Record<string, unknown> } | undefined {
    const entry = this.registry.get(name)
    if (entry === undefined) return undefined
    return {
      name,
      description: entry.schema.description,
      parameters: entry.schema.parameters,
    }
  }

  /** Every registered tool with its schema, in registration order. */
  listDefinitions(): ToolDefinition[] {
    return [...this.registry.entries()].map(([name, entry]) => ({ name, schema: entry.schema }))
  }

  execute(name: string, args: unknown, signal: AbortSignal): Promise<ToolResult> {
    const entry = this.registry.get(name)
    if (entry === undefined) {
      return Promise.resolve({ content: `unknown tool: ${name}`, isError: true })
    }
    return Promise.resolve(entry.executor(args, signal)).catch((error: unknown) => ({
      // 保留 Error.message（含工具给出的下一步指引），避免 String(error) 变成 "Error: ..."
      content: error instanceof Error ? error.message : String(error),
      isError: true,
    }))
  }
}

export default ToolsService