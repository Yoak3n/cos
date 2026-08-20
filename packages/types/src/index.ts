/**
 * @cos/types — shared type vocabulary for the harness: session log events,
 * agent messages, the Session contract, and the Cordis event surface
 * (declaration merging). One home for the `app/*`, `session/*`, and
 * `agent/*` events the loop dispatches.
 * @module @cos/types
 */

import { randomUUID } from 'node:crypto'
import type { Context } from 'cordis'

/** Opaque brand for shared agent/session identities. */
export type SessionId = string & { readonly __sessionId: unique symbol }

/** Mint one branded session id. */
export function SessionId(id: string): SessionId {
  return id as SessionId
}

/** Model-visible message content blocks. */
export type MessageContent = Array<
  | { type: 'text'; text: string }
  | { type: 'tool-call'; id: string; name: string; arguments: string }
  | { type: 'tool-result'; callId: string; isError: boolean; content: string }
>

export interface UserMessage {
  id: string
  role: 'user'
  content: MessageContent
  source: { kind: 'human' | 'plugin' | 'goal'; detail?: string }
}

export interface AssistantMessage {
  id: string
  role: 'assistant'
  content: MessageContent
}

export interface ToolResultMessage {
  callId: string
  content: string
  isError: boolean
}

export interface ToolSchema {
  description: string
  /** OpenAI-style JSON schema; only the object form is supported. */
  parameters: Record<string, unknown>
}

/** OpenAI-compatible tool definition sent on the request wire. */
export interface WireTool {
  type: 'function'
  function: { name: string; description: string; parameters: Record<string, unknown> }
}

/** A registered tool with its schema, as the loop advertises to the model. */
export interface ToolDefinition {
  name: string
  schema: ToolSchema
}

/** Opaque brand for a tool-call identifier (DSH CallId analog). */
export type CallId = string & { readonly __callId: unique symbol }
/** Mint one branded tool-call id. */
export function CallId(id: string): CallId {
  return id as CallId
}

/** Token accounting reported by an adapter, when available. */
export interface TokenUsage {
  inputTokens?: number
  outputTokens?: number
}

/** Why the model finished producing this response. */
export type FinishReason =
  | { kind: 'stop' }
  | { kind: 'tool-calls' }
  | { kind: 'max-tokens' }
  | { kind: 'aborted'; failure: unknown }
  | { kind: 'error'; failure: unknown }

/** One assembled model block: a finished text or tool-call unit. */
export type ModelBlock =
  | { type: 'text'; text: string }
  | { type: 'tool-call'; id: string; name: string; arguments: string }

/**
 * The adapter-output stream protocol, matching dsh-llm's StreamChunk. Every
 * block opens with `block-start` and closes with `block-end` carrying the
 * complete block; `usage` precedes `finish`, which is always the final chunk.
 * @mode raw stream
 */
export type StreamChunk =
  | { type: 'block-start'; index: number; blockType: 'text' | 'tool-call' }
  | { type: 'text-delta'; index: number; text: string }
  | { type: 'tool-call-delta'; index: number; id: string; name: string; argumentsDelta: string }
  | { type: 'block-end'; index: number; block: ModelBlock }
  | { type: 'usage'; usage: TokenUsage }
  | { type: 'finish'; reason: FinishReason }

/** Where an inbox message waits: one queued turn or the nearest step boundary. */
export type InboxTarget = 'next-turn' | 'next-step'

export type AgentStatus = 'idle' | 'running'

export type AgentCancelCause = { kind: 'user' | 'parent' | 'disposed' }

export type TurnEndReason =
  | { kind: 'completed' }
  | { kind: 'blocked' }
  | { kind: 'max-tokens' }
  | { kind: 'aborted'; reason: AgentCancelCause }
  | { kind: 'error'; error: { message: string; code: string } }

/** One durable slot of the append-only session log, discriminated by `type`. */
export type SessionEvent = {
  [Type in SessionEventType]: { type: Type; seq: number; time: number; data: SessionEventMap[Type] }
}[SessionEventType]

/** The merge-extensible, append-only source of truth for one interaction. */
export interface SessionEventMap {
  /** Opens turn `turn` before the loop claims queued input or runs pre-step. */
  'turn/start': { turn: number }
  /** Closes turn `turn` with the {@link TurnEndReason} that ended it. */
  'turn/end': { turn: number; reason: TurnEndReason }
  /** Opens step `step` of turn `turn` — one model call plus the tools it requested. */
  'step/start': { turn: number; step: number }
  /** Closes step `step` of turn `turn`. */
  'step/end': { turn: number; step: number }
  /** A user-role message on the model-visible surface. */
  'user/message': UserMessage
  /** Raw stream chunk — token-level replay fidelity. */
  'assistant/chunk': { turn: number; step: number; chunk: StreamChunk }
  /** Assembled assistant message for one step (derived history uses this). */
  'assistant/message': { turn: number; step: number; message: AssistantMessage }
  /** The model requested one tool invocation; `callId` pairs it with its result. */
  'tool/call': { turn: number; step: number; callId: string; name: string; arguments: string }
  /** A completed tool call's model-facing result. */
  'tool/result': { turn: number; step: number; callId: string; message: ToolResultMessage }
  /** Marks the end of a constructor seed (resume, fork, replay). */
  'session/end-seed': Record<string, never>
}

export type SessionEventType = keyof SessionEventMap

/** A derived model-visible message for the request wire. */
export interface ModelMessage {
  role: 'user' | 'assistant' | 'tool'
  content: MessageContent
  callId?: string
}

/** Request config the loop sends through the llm seam. */
export interface LlmCallConfig {
  provider: string
  model: string
  maxTokens?: number
}

export interface GenerateOptions extends LlmCallConfig {
  messages: ModelMessage[]
  sessionId: SessionId
  signal: AbortSignal
  /** Rendered dynamic system prompt for this request. */
  system?: string
  /** Tool definitions advertised to the model on this request. */
  tools?: WireTool[]
}

/** The `llm` seam: one provider (mock or real) fills this shape. */
export interface LlmRoute {
  provider: string
  model: string
}

/** Structured llm-seam failure, thrown by adapter resolution. */
export class LlmError extends Error {
  static readonly NO_ADAPTER = 'NO_ADAPTER'
  readonly code: string
  constructor(code: string, message: string) {
    super(message)
    this.code = code
  }
}

export type PreStepDecision =
  | { kind: 'enter'; messages: UserMessage[] }
  | { kind: 'reject' }

/** A listener returns this from `agent/request-error` to claim recovery. */
export type RequestErrorAction = { kind: 'retry' } | undefined

/** Loop options supplied at agent creation. */
export interface AgentOptions {
  provider?: string
  model?: string
  maxTokens?: number
}

/** One live agent: the subject every `agent/*` event carries. */
export interface Agent {
  readonly id: SessionId
  readonly ctx: Context
  readonly session: Session
  readonly options: AgentOptions
  readonly status: AgentStatus
  followup(message: UserMessage): void
  steer(message: UserMessage): void
  inject(message: UserMessage): void
  cancel(cause: AgentCancelCause, options?: { keepInbox?: boolean }): void
  whenIdle(): Promise<void>
  dispose(): Promise<void>
}

export interface AgentHandle {
  agent: Agent
  dispose(): Promise<void>
}

/** A live session: durable log plus registry identity. */
export interface Session {
  readonly id: SessionId
  readonly header: { cwd?: string }
  readonly events: SessionEvent[]
  append<T extends SessionEventType>(type: T, data: SessionEventMap[T]): SessionEvent
  deriveMessages(): ModelMessage[]
}

/** Create a user message with a stable identity. */
export function createUserMessage(text: string, source: UserMessage['source'] = { kind: 'human' }): UserMessage {
  return {
    id: randomUUID(),
    role: 'user',
    content: [{ type: 'text', text }],
    source,
  }
}

export type VariableProvider = (context: AssembleContext) => string | undefined

/** Context for one system-prompt assembly (consumed by @cos/system-prompt). */
export interface AssembleContext {
  /** The agent requesting the assembly, when any. */
  agent?: Agent
  /** Scope whose providers participate; absent means global only. */
  scope?: object
  /** Explicit control signal for the requesting turn. */
  signal?: AbortSignal
  /** Resolved provider/model route for this assembly, when known. */
  provider?: string
  model?: string
}

/** One contributed system-prompt section (registry input). */
export interface PromptSection {
  readonly name: string
  readonly order: number
  readonly text: string | ((context: AssembleContext) => string)
}

/** One section of an assembly, with its text resolved. */
export interface AssembledSection {
  name: string
  text: string
}

/** The merge-extensible assembled model input from @cos/system-prompt. */
export interface PromptAssembly {
  sections: AssembledSection[]
  tools: WireTool[]
  variables: Record<string, string | undefined>
}

declare module 'cordis' {
  interface Events {
    // ---- session domain: durable facts broadcast post-commit ----
    'session/created'(session: Session): void
    'session/disposed'(session: Session): void
    'session/event'(session: Session, event: SessionEvent): void
    'session/flush'(session: Session): void

    // ---- agent domain: the loop's live extension surface ----
    'agent/created'(payload: { agent: Agent }): void
    'agent/disposed'(payload: { agent: Agent }): void
    'agent/status'(payload: { agent: Agent; status: AgentStatus }): void
    'agent/inbox/inserted'(payload: { agent: Agent; message: UserMessage }): void
    'agent/inbox/claimed'(payload: { agent: Agent; message: UserMessage; turn: number }): void
    'agent/inbox/discarded'(payload: { agent: Agent; message: UserMessage }): void
    'agent/session-start'(payload: { agent: Agent; source: 'startup' | 'resume' }): void
    'agent/pre-step'(
      payload: { agent: Agent; messages: UserMessage[]; turn: number; step: number; signal: AbortSignal },
      next: () => Promise<PreStepDecision>,
    ): Promise<PreStepDecision>
    'agent/request'(
      payload: { agent: Agent; turn: number; step: number; signal: AbortSignal },
      next: () => Promise<LlmCallConfig>,
    ): Promise<LlmCallConfig>
    'agent/request-error'(
      payload: {
        agent: Agent
        turn: number
        step: number
        provider: string
        model: string
        failure: unknown
        available: string
        signal: AbortSignal
      },
      next: () => Promise<RequestErrorAction>,
    ): Promise<RequestErrorAction>
    'agent/turn-stopping'(payload: { agent: Agent; turn: number; signal: AbortSignal }): Promise<void> | void
    'agent/error'(payload: { agent: Agent; turn: number; step: number; error: unknown }): void
  }
}