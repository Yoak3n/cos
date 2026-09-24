/**
 * `@deepseek-ai/dsh-agent` — DSH-compatible agent factory types and helpers.
 * Runtime calls are `ctx.agents.create(options)` / `ctx.agents.resume(options)`
 * (see `@cos/agents` dual-shape methods).
 * @module @deepseek-ai/dsh-agent
 */

import type {
  Agent,
  AgentHandle,
  AgentOptions,
  SessionEvent,
  SessionId,
} from '@cos/types'
import { SessionId as brandSessionId } from '@cos/types'

export type {
  Agent,
  AgentHandle,
  AgentOptions,
  AgentStatus,
  Session,
  SessionEvent,
  SessionId,
  TurnEndReason,
  UserMessage,
} from '@cos/types'

/** Session-creation metadata accepted by DSH `CreateAgentOptions.meta`. */
export interface CreateAgentMeta {
  cwd?: string
  parentSession?: SessionId
  isSeeded?: boolean
  origin?: 'subagent'
  delegationDepth?: number
  agentPrompt?: string
  ephemeral?: boolean
}

/** DSH `CreateAgentOptions` (setup callback omitted — compose via plugins). */
export interface CreateAgentOptions {
  sessionId: SessionId
  meta?: CreateAgentMeta
  inheritedEventCount?: number
  seed?: readonly SessionEvent[]
  agentOptions?: AgentOptions
  signal?: AbortSignal
}

/** DSH `ResumeAgentOptions`. */
export interface ResumeAgentOptions {
  resumeSessionId: SessionId
  agentOptions?: AgentOptions
  signal?: AbortSignal
}

/** Factory interface implemented by `@cos/agents` create/resume. */
export interface AgentFactory {
  create(options: CreateAgentOptions): Promise<AgentHandle>
  resume(options: ResumeAgentOptions): Promise<AgentHandle>
}

/** Mint a branded session id (DSH CallId/SessionId analog). */
export function toSessionId(id: string): SessionId {
  return brandSessionId(id)
}
