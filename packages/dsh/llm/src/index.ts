/**
 * `@deepseek-ai/dsh-llm` — DSH-compatible LLM package surface on cos.
 * Plugins import `LlmAdapter`, stream/message types, and `HarnessError` from
 * here; registration is `ctx.llm.registerAdapter(providers, adapter)` (same
 * call shape as DSH).
 * @module @deepseek-ai/dsh-llm
 */

import { LlmError } from '@cos/types'

export { LlmAdapter, BlockAssembler } from '@cos/llm'
export type {
  AdapterConfigField,
  LlmProviderInfo,
  ProviderConfigDecl,
  ResolvedModelInfo,
} from '@cos/llm'

export {
  CallId,
  LlmError as LlmRuntimeError,
  createUserMessage,
} from '@cos/types'

export type {
  Agent,
  AgentHandle,
  AgentOptions,
  AgentStatus,
  AssembleContext,
  AssistantMessage,
  FinishReason,
  GenerateOptions,
  LlmCallConfig,
  MessageContent,
  ModelBlock,
  ModelMessage,
  PromptAssembly,
  PromptSection,
  Session,
  SessionEvent,
  SessionEventMap,
  SessionEventType,
  StreamChunk,
  TokenUsage,
  ToolSchema,
  TurnEndReason,
  UserMessage,
  WireTool,
} from '@cos/types'

/**
 * DSH `HarnessError` stand-in: an Error with a stable `code`.
 * Cos's structured seam error is `LlmError`; keep the DSH name importable.
 */
export class HarnessError extends Error {
  readonly code: string
  constructor(message: string, code = 'HARNESS_ERROR') {
    super(message)
    this.name = 'HarnessError'
    this.code = code
  }
}

/** DSH export name for the structured LLM failure. */
export { LlmError }

/** Alias matching dsh-llm's assert helper (pass-through identity check). */
export function assertUsableApiKey(raw: string, pkg: string, ref: string): string {
  if (typeof raw !== 'string' || raw.trim() === '') {
    throw new HarnessError(`${pkg}: API key "${ref}" is empty`, 'EMPTY_API_KEY')
  }
  return raw
}
