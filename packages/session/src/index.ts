/**
 * @cos/session — session spine (`ctx.sessions`): the append-only durable log,
 * the `SessionEventMap` contract, and the `session/event` broadcast.
 * Model-visible means logged: every request is derived from this log.
 * @module @cos/session
 */

import { randomUUID } from 'node:crypto'
import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { MessageContent, ModelMessage, SessionEvent, SessionEventMap, SessionEventType, SessionId } from '@cos/types'
import { SessionId as brandSessionId } from '@cos/types'
import type { Session as SessionShape } from '@cos/types'

/** 单条 tool 结果进模型前的最大字符数（超长工具输出是上下文膨胀主因）。 */
const MAX_TOOL_RESULT_CHARS = 1200
/** 单条文本块进模型前的最大字符数。 */
const MAX_TEXT_BLOCK_CHARS = 4000
/** 整段 wire 历史的近似字符预算（中文约 1 token/字；控制在数万 token 内）。 */
const MAX_WIRE_CHARS = 100_000

function clipText(text: string, max: number): string {
  if (text.length <= max) return text
  return `${text.slice(0, max)}\n…[已截断，共 ${text.length} 字]`
}

function trimContent(content: MessageContent, isTool: boolean): MessageContent {
  const cap = isTool ? MAX_TOOL_RESULT_CHARS : MAX_TEXT_BLOCK_CHARS
  return content.map((block) => {
    if (block.type !== 'text') return block
    return { type: 'text' as const, text: clipText(block.text, cap) }
  })
}

function contentChars(content: MessageContent): number {
  let n = 0
  for (const block of content) {
    if (block.type === 'text') n += block.text.length
    else if (block.type === 'tool-call') n += block.arguments.length + block.name.length
  }
  return n
}

/**
 * 进模型前裁剪 wire 历史：先掐超长 tool/文本，再从最旧整轮丢弃到字符预算内。
 * 只在 deriveMessages 出口做，不改写持久化日志（UI 历史仍完整）。
 */
function trimForModel(messages: ModelMessage[]): ModelMessage[] {
  const trimmed: ModelMessage[] = messages.map((m) => ({
    ...m,
    content: trimContent(m.content, m.role === 'tool'),
  }))
  const sizes = trimmed.map((m) => contentChars(m.content))
  let total = sizes.reduce((a, b) => a + b, 0)
  let start = 0
  while (total > MAX_WIRE_CHARS && start < trimmed.length - 1) {
    // 按「下一条 user」切整轮丢弃，避免留下孤儿 tool 结果或半截 tool-call。
    let next = start + 1
    while (next < trimmed.length && trimmed[next].role !== 'user') next++
    // 已经没有更完整的下一轮：至少保留最后一条（通常是当前 user）。
    if (next >= trimmed.length && start >= trimmed.length - 1) break
    const dropTo = next >= trimmed.length ? trimmed.length - 1 : next
    for (let i = start; i < dropTo; i++) total -= sizes[i]
    start = dropTo
    // 落点若是 tool 结果（上一轮 tool-call 已丢）：继续丢掉孤儿 tool。
    while (start < trimmed.length && trimmed[start].role === 'tool') {
      total -= sizes[start]
      start += 1
    }
  }
  return trimmed.slice(start)
}

declare module 'cordis' {
  interface Context {
    sessions: SessionsService
  }
}

/** One session's live log and store entry. */
export class Session implements SessionShape {
  readonly id: SessionId
  readonly header: { cwd?: string; ephemeral?: boolean }
  readonly events: SessionEvent[] = []
  private seq = 0

  constructor(
    private readonly ctx: Context,
    id: SessionId,
    meta: { cwd?: string; ephemeral?: boolean } = {},
    seed: readonly SessionEvent[] = [],
  ) {
    this.id = id
    this.header = { ...meta }
    this.events.push(...seed)
    this.seq = seed.length
    // Mark the constructor seed boundary so replay consumers can tell seeded
    // history from live work (only when a seed was actually supplied).
    if (seed !== undefined && seed.length > 0) {
      this.append('session/end-seed', {})
    }
  }

  /**
   * Append one durable event: seq assigned, log pushed, then broadcast
   * through `session/event` so observers follow the authoritative stream.
   */
  append<T extends SessionEventType>(type: T, data: SessionEventMap[T]): SessionEvent {
    const event = {
      type,
      data,
      seq: this.seq++,
      time: Date.now(),
    } as unknown as SessionEvent
    this.events.push(event)
    this.ctx.emit('session/event', this, event)
    return event
  }

  /** Project the log into the model-visible message list for the request wire. */
  deriveMessages(): ModelMessage[] {
    const messages: ModelMessage[] = []
    for (const event of this.events) {
      if (event.type === 'user/message') {
        messages.push({ role: 'user', content: event.data.content })
      } else if (event.type === 'assistant/message') {
        messages.push({ role: 'assistant', content: event.data.message.content })
      } else if (event.type === 'tool/result') {
        const content: MessageContent = [{ type: 'text', text: event.data.message.content }]
        const images = event.data.message.images
        if (images) {
          for (const img of images) {
            content.push({
              type: 'image',
              mime: img.mime,
              data: img.data,
              ...(img.name !== undefined ? { name: img.name } : {}),
            })
          }
        }
        messages.push({
          role: 'tool',
          callId: event.data.callId,
          content,
        })
      }
    }
    return trimForModel(messages)
  }
}

/** Session registry: create, look up, remove, and checkpoint-flush sessions. */
export class SessionsService extends Service {
  static inject: string[] = []
  private readonly store = new Map<SessionId, Session>()

  constructor(ctx: Context) {
    super(ctx, 'sessions')
  }

  /** Create a fresh session, register it, and announce `session/created`. */
  create(id: SessionId = brandSessionId(randomUUID()), meta?: { cwd?: string; ephemeral?: boolean }, seed?: readonly SessionEvent[]): SessionShape {
    const session = new Session(this.ctx, id, meta, seed)
    this.store.set(id, session)
    this.ctx.emit('session/created', session)
    return session
  }

  get(id: SessionId): SessionShape | undefined {
    return this.store.get(id)
  }

  /** Remove a live session and announce `session/disposed`. */
  remove(session: SessionShape): void {
    if (this.store.get(session.id) !== session) return
    this.store.delete(session.id)
    this.ctx.emit('session/disposed', session)
  }

  /**
   * Whether a session's durable log has reached durable storage, without the
   * caller knowing how persistence is implemented. Delegates to the mounted
   * sessionPersistence service; false when none is configured.
   * @param id - session identity to inspect.
   */
  isPersisted(id: SessionId): boolean {
    const persistence = this.ctx.get('sessionPersistence') as
      | { isPersisted(id: SessionId): boolean }
      | undefined
    return persistence?.isPersisted(id) ?? false
  }

  /** Awaited parallel durability checkpoint; persistence plugins join this event. */
  async flush(session: SessionShape): Promise<void> {
    await this.ctx.parallel('session/flush', session)
  }
}

export default SessionsService