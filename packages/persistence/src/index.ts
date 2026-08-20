/**
 * @cos/persistence — JSONL session persistence (`ctx.sessionPersistence`):
 * appends each session's durable log to `<root>/<id>.jsonl` on `session/flush`
 * and replays it on `prepare()`. The checkpoint policy lives with the loop:
 * it flushes after every turn end.
 * @module @cos/persistence
 */

import { appendFileSync, mkdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { Session, SessionEvent, SessionId } from '@cos/types'

declare module 'cordis' {
  interface Context {
    sessionPersistence: JsonlPersistenceService
  }
  interface Events {
    /** Published after a persisted session failed to load or store. */
    'session/persist-error'(sessionId: SessionId, error: unknown): void
  }
}

export class JsonlPersistenceService extends Service {
  static inject = ['sessions']
  private readonly root: string
  private readonly watermarks = new WeakMap<Session, number>()

  constructor(ctx: Context, config: { root?: string } = {}) {
    super(ctx, 'sessionPersistence')
    this.root = join(process.cwd(), config.root ?? '.sessions')
    mkdirSync(this.root, { recursive: true })
    this.ctx.on('session/flush', (session) => {
      try {
        this.save(session)
      } catch (error: unknown) {
        this.ctx.logger.warn(`cos/persistence: flush failed for ${session.id}: ${String(error)}`)
      }
    }, { prepend: true })
  }

  private fileOf(id: SessionId): string {
    return join(this.root, `${id}.jsonl`)
  }

  /** Load one session's durable events, or undefined when nothing was stored. */
  prepare(id: SessionId): readonly SessionEvent[] | undefined {
    const file = this.fileOf(id)
    let raw: string
    try {
      raw = readFileSync(file, 'utf8')
    } catch {
      return undefined
    }
    if (raw === '') return []
    const events: SessionEvent[] = []
    for (const line of raw.trim().split('\n')) {
      if (line === '') continue
      events.push(JSON.parse(line) as SessionEvent)
    }
    return events
  }

  /** Whether a durable file exists with stored events for one session. */
  isPersisted(id: SessionId): boolean {
    const events = this.prepare(id)
    return events !== undefined && events.some((event) => event.type === 'turn/end')
  }

  /** Append every event after the last watermark; advances the watermark. */
  private save(session: Session): void {
    const file = this.fileOf(session.id)
    const written = this.watermarks.get(session) ?? 0
    const pending = session.events.filter((event) => event.seq >= written)
    if (pending.length === 0) return
    appendFileSync(file, `${pending.map((event) => JSON.stringify(event)).join('\n')}\n`)
    this.watermarks.set(session, written + pending.length)
  }
}

export default JsonlPersistenceService