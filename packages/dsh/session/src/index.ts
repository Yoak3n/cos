/**
 * `@deepseek-ai/dsh-session` — DSH-compatible session types on cos.
 * Runtime session store is `ctx.sessions`; durable replay is
 * `ctx.sessionPersistence.prepare(id)`.
 * @module @deepseek-ai/dsh-session
 */

export type {
  AssistantMessage,
  MessageContent,
  ModelMessage,
  Session,
  SessionEvent,
  SessionEventMap,
  SessionEventType,
  ToolResultMessage,
  UserMessage,
} from '@cos/types'

export { CallId, SessionId, createUserMessage } from '@cos/types'
