/**
 * @cos/persistence — segmented JSONL session persistence (`ctx.sessionPersistence`):
 * appends each session's durable log to segment files and replays them on
 * `prepare()`. The checkpoint policy lives with the loop: it flushes after every
 * turn end.
 *
 * Storage anchor — the agent data home: the cos home (`COS_HOME`) when set,
 * else the cwd. The config & artifacts of an agent live in ONE folder
 * (`$COS_HOME/sessions`, sibling of settings / workspace / memory).
 * An absolute `root` is used as-is.
 *
 * On-disk format (`format`):
 * - **standard** (default): the generic agent-session event stream (`.pi/agent`
 *   style, version 3) — `id`/`parentId`-chained `session` + `message` events
 *   (roles `user` / `assistant` / `toolResult`, camelCase `toolCall` blocks,
 *   assistant usage/stopReason). The framework's internal event flow is
 *   untouched; only the persisted bytes use the standard shape, decoded back to
 *   internal events on `prepare()`. Internal bookkeeping events
 *   (turn/start, step/*, assistant/chunk, session/end-seed) never hit the file.
 * - `internal` (legacy): raw `SessionEvent` JSON lines.
 *
 * Volume & growth control (single long-lived sessions must not balloon):
 * - streaming-transient events (`assistant/chunk`) are never persisted: deltas
 *   are UI-only and fully replaced by the assembled assistant message.
 * - **segmented rotation** (`segmentBytes`, default 8 MiB): once the active file
 *   exceeds the cap it is renamed to `<id>.<n>.jsonl` (higher n = newer) and a
 *   fresh active file starts (each segment self-describes with a header line).
 * - **retention cap** (`maxSegments`, default 0 = unlimited): keeps at most that
 *   many rotated segments and drops the oldest, bounding resume IO for
 *   long-running sessions.
 *
 * A resumed session's watermark is seeded from what is already on disk, so the
 * first flush after a restart never re-appends the replayed history.
 * @module @cos/persistence
 */

import { existsSync, appendFileSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statSync } from 'node:fs'
import { isAbsolute, join, resolve } from 'node:path'
import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { Session, SessionEvent, SessionId } from '@cos/types'

import { StandardReader, StandardWriter, isStandardLine } from './codec.ts'

declare module 'cordis' {
  interface Context {
    sessionPersistence: JsonlPersistenceService
  }
  interface Events {
    /** Published after a persisted session failed to load or store. */
    'session/persist-error'(sessionId: SessionId, error: unknown): void
  }
}

/** Streaming-only event types that never hit the log file (assembled messages replace them). */
const DEFAULT_TRANSIENT: readonly string[] = ['assistant/chunk']

export type PersistenceFormat = 'standard' | 'internal'

export interface PersistenceConfig {
  /** Storage directory (absolute, or relative to the cos home / cwd). */
  root?: string
  /** On-disk line format (default `standard`, the generic event stream). */
  format?: PersistenceFormat
  /** Event types excluded from the log file (streaming/transient only; `internal` format). */
  transientEventTypes?: readonly string[]
  /** Roll the active segment file once it exceeds this many bytes (0 = single file). */
  segmentBytes?: number
  /** Keep at most this many rotated segments; 0 = unlimited (oldest dropped beyond the cap). */
  maxSegments?: number
}

export class JsonlPersistenceService extends Service {
  static inject = ['sessions']
  private readonly root: string
  private readonly format: PersistenceFormat
  private readonly watermarks = new WeakMap<Session, number>()
  private readonly transient: ReadonlySet<string>
  private readonly segmentBytes: number
  private readonly maxSegments: number
  /** 增量写端（标准格式）：每会话一个，跨 flush 维持 id 链与 callId → 工具名。 */
  private readonly standardWriters = new Map<string, StandardWriter>()

  constructor(ctx: Context, config: PersistenceConfig = {}) {
    super(ctx, 'sessionPersistence')
    // cos 数据家园锚点：COS_HOME 优先（sidecar 注入的 agent home），其次 cwd
    // ——与宿主 cosHome() 语义一致（配置与产物同目录）。
    const anchor = process.env.COS_HOME ?? process.cwd()
    const root = config.root ?? '.sessions'
    this.root = isAbsolute(root) ? root : resolve(anchor, root)
    mkdirSync(this.root, { recursive: true })
    this.format = config.format ?? 'standard'
    this.transient = new Set(config.transientEventTypes ?? DEFAULT_TRANSIENT)
    this.segmentBytes = config.segmentBytes ?? 8 * 1024 * 1024
    this.maxSegments = config.maxSegments ?? 0
    this.ctx.on('session/flush', (session) => {
      try {
        this.save(session)
      } catch (error: unknown) {
        this.ctx.logger.warn(`cos/persistence: flush failed for ${session.id}: ${String(error)}`)
      }
    }, { prepend: true })
  }

  private activeFileOf(id: SessionId): string {
    return join(this.root, `${id}.jsonl`)
  }

  /** Rotated segment files in ascending N (oldest first); the active file is excluded. */
  private rotatedFilesOf(id: SessionId): string[] {
    const prefix = `${String(id)}.`
    const out: Array<{ file: string; index: number }> = []
    let entries: string[]
    try {
      entries = readdirSync(this.root)
    } catch {
      return []
    }
    for (const name of entries) {
      const suffix = name.slice(prefix.length)
      if (!name.startsWith(prefix) || !suffix.endsWith('.jsonl')) continue
      const index = Number(suffix.slice(0, -'.jsonl'.length))
      if (Number.isFinite(index) && index >= 1) out.push({ file: join(this.root, name), index })
    }
    out.sort((a, b) => a.index - b.index)
    return out.map((entry) => entry.file)
  }

  private filesOf(id: SessionId): string[] {
    return [...this.rotatedFilesOf(id), this.activeFileOf(id)]
  }

  /** 活动文件（或最后一段）最后一行非空行的 id——标准格式续写 `parentId` 链用。 */
  private tailIdOf(id: SessionId): string | null {
    const files = this.filesOf(id)
    for (let index = files.length - 1; index >= 0; index -= 1) {
      let raw: string
      try {
        raw = readFileSync(files[index], 'utf8')
      } catch {
        continue
      }
      const lines = raw.trim().split('\n').filter((line) => line !== '')
      if (lines.length === 0) continue
      const match = /"id"\s*:\s*"([^"]+)"/.exec(lines[lines.length - 1])
      if (match !== null) return match[1]
    }
    return null
  }

  /** Load one session's durable events across every segment, or undefined when nothing was stored. */
  prepare(id: SessionId): readonly SessionEvent[] | undefined {
    const files = this.filesOf(id)
    if (files.length === 0) return undefined
    const events: SessionEvent[] = []
    const reader = new StandardReader(events)
    for (const file of files) {
      let raw: string
      try {
        raw = readFileSync(file, 'utf8')
      } catch {
        continue
      }
      if (raw === '') continue
      for (const line of raw.trim().split('\n')) {
        if (line === '') continue
        try {
          if (isStandardLine(line)) {
            // 标准格式行：解码（seq = 输出数组长度，全局单调）。
            reader.handle(JSON.parse(line) as Parameters<StandardReader['handle']>[0])
          } else {
            // 旧 internal 格式行：原样保留。
            events.push({ ...JSON.parse(line) as SessionEvent })
          }
        } catch {
          // 坏行跳过（历史损坏不应阻断整段恢复）
        }
      }
    }
    return events
  }

  /** Whether durable message events exist for one session. */
  isPersisted(id: SessionId): boolean {
    const events = this.prepare(id)
    return events !== undefined
      && events.some((event) => event.type === 'turn/end' || event.type === 'assistant/message')
  }

  /** Append durable events after the last watermark; advances the watermark and rotates when due. */
  private save(session: Session): void {
    // Ephemeral sessions (worker/subagent one-shots) never reach disk: they are
    // disposable by construction and resume would never want their history.
    if (session.header.ephemeral === true) return
    // Resume 语义：新会话对象首次 flush 时，磁盘上已有的重放历史不得重复追加
    // （否则每次重启后首次对话都会让文件近似翻倍）。
    let written = this.watermarks.get(session)
    if (written === undefined) {
      written = this.prepare(session.id)?.length ?? 0
      this.watermarks.set(session, written)
    }
    const file = this.activeFileOf(session.id)
    const pending = session.events.slice(written)
    if (this.format === 'standard') {
      let writer = this.standardWriters.get(String(session.id))
      if (writer === undefined) {
        // 活动段为新文件时先写 header 行；否则从段尾续链。
        const isNew = !existsSync(file) || statSync(file).size === 0
        writer = new StandardWriter(this.tailIdOf(session.id))
        if (isNew) appendFileSync(file, `${writer.header(session.id, session.header)}\n`)
        this.standardWriters.set(String(session.id), writer)
      }
      const lines = writer.encodeBatch(pending)
      if (lines.length > 0) appendFileSync(file, `${lines.join('\n')}\n`)
    } else {
      const durable = pending.filter((event) => !this.transient.has(event.type))
      if (durable.length > 0) {
        appendFileSync(file, `${durable.map((event) => JSON.stringify(event)).join('\n')}\n`)
      }
    }
    // 水印推进到全部新事件（含被过滤/不落盘的记账事件），保证 seq 对齐内存日志。
    this.watermarks.set(session, session.events.length)
    if (this.segmentBytes > 0) this.maybeRotate(session.id)
  }

  /** Rename the active file to the next segment when it exceeds the byte cap; then trim retention. */
  private maybeRotate(id: SessionId): void {
    const active = this.activeFileOf(id)
    let size: number
    try {
      size = statSync(active).size
    } catch {
      return
    }
    if (size < this.segmentBytes) return
    const rotated = this.rotatedFilesOf(id)
    const maxIndex = rotated.length > 0 ? this.segmentIndexOf(rotated[rotated.length - 1]) : 0
    try {
      renameSync(active, join(this.root, `${String(id)}.${maxIndex + 1}.jsonl`))
    } catch {
      return
    }
    if (this.maxSegments > 0) this.trimSegments(id)
  }

  private segmentIndexOf(file: string): number {
    const match = /\.(\d+)\.jsonl$/.exec(file)
    return match === null ? 0 : Number(match[1])
  }

  /** Drop the oldest rotated segments beyond the retention cap. */
  private trimSegments(id: SessionId): void {
    const rotated = this.rotatedFilesOf(id)
    let excess = rotated.length - this.maxSegments
    for (const file of rotated) {
      if (excess <= 0) break
      try {
        rmSync(file)
      } catch {
        // 删除失败不阻断
      }
      excess -= 1
    }
  }
}

export default JsonlPersistenceService