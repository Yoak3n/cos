/**
 * @cos/session — session spine (`ctx.sessions`): the append-only durable log,
 * the `SessionEventMap` contract, and the `session/event` broadcast.
 * Model-visible means logged: every request is derived from this log.
 * @module @cos/session
 */

import { randomUUID } from 'node:crypto'
import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { ModelMessage, SessionEvent, SessionEventMap, SessionEventType, SessionId } from '@cos/types'
import { SessionId as brandSessionId } from '@cos/types'
import type { Session as SessionShape } from '@cos/types'

declare module 'cordis' {
  interface Context {
    sessions: SessionsService
  }
}

/** One session's live log and store entry. */
export class Session implements SessionShape {
  readonly id: SessionId
  readonly header: { cwd?: string }
  readonly events: SessionEvent[] = []
  private seq = 0

  constructor(
    private readonly ctx: Context,
    id: SessionId,
    meta: { cwd?: string } = {},
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
        messages.push({
          role: 'tool',
          callId: event.data.callId,
          content: [{ type: 'text', text: event.data.message.content }],
        })
      }
    }
    return messages
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
  create(id: SessionId = brandSessionId(randomUUID()), meta?: { cwd?: string }, seed?: readonly SessionEvent[]): SessionShape {
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