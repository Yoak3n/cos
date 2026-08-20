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

interface Entry {
  executor: ToolExecutor
  schema: ToolSchema
}

declare module 'cordis' {
  interface Context {
    tools: ToolsService
  }
}

export class ToolsService extends Service {
  static inject: string[] = []
  private readonly registry = new Map<string, Entry>()

  constructor(ctx: Context) {
    super(ctx, 'tools')
  }

  /** Register one named executor with its wire schema; returns the disposer. */
  register(name: string, executor: ToolExecutor, options: ToolOptions = {}): () => void {
    this.registry.set(name, {
      executor,
      schema: {
        description: options.description ?? `Call the tool "${name}"`,
        parameters: options.parameters ?? { type: 'object', properties: {} },
      },
    })
    return () => { this.registry.delete(name) }
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
      content: String(error),
      isError: true,
    }))
  }
}

export default ToolsService