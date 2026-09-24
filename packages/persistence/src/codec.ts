/**
 * @cos/persistence — 落盘事件编解码（codec）。
 *
 * 把框架内部的内存会话事件（SessionEvent）与"通用 agent 会话事件流" JSONL
 * （对齐 `.pi/agent` 事件格式 v3：`id`/`parentId` 链 + `message` 内容块、
 * `toolCall` 骆驼式块、assistant 消息携带 `usage` / `stopReason`）做双向映射。
 *
 * 设计约束：**不触碰框架内部数据流**——session/event 广播、deriveMessages、
 * 记忆/压缩回调全部照旧；只有"最终落盘的字节"按通用格式写出，读取时再解码回
 * SessionEvent 供 resume 使用。内部记账事件（turn/start、step/*、assistant/chunk、
 * session/end-seed）不进入落盘文件。
 * @module @cos/persistence/codec
 */

import type { FinishReason, SessionEvent, SessionEventMap, SessionId, TokenUsage } from '@cos/types'

/** 通用事件流中受支持的一行（.pi/agent 风格）。 */
export interface StandardEvent {
  type: string
  id: string
  parentId: string | null
  timestamp: string
  [key: string]: unknown
}

/** 通用事件流的顶层 type 词汇。 */
const STANDARD_TYPES = new Set(['session', 'message', 'model_change', 'thinking_level_change'])

/** 按行首 `"type"` 判定一行是否已是通用格式（与内部 `{"type":"user/message",...}` 区分）。 */
export function isStandardLine(raw: string): boolean {
  const match = /^\{?\s*"type"\s*:\s*"([a-z_]+)"/.exec(raw)
  return match !== null && STANDARD_TYPES.has(match[1])
}

function hexId(seq: number): string {
  return seq.toString(16).padStart(8, '0')
}

function iso(ms: number): string {
  return new Date(ms).toISOString()
}

function textOf(blocks: unknown): string {
  if (!Array.isArray(blocks)) return ''
  return blocks
    .filter((block) => block?.type === 'text' && typeof block.text === 'string')
    .map((block) => block.text)
    .join('\n')
}

function parseArgs(raw: string): unknown {
  try {
    return raw === '' ? {} : JSON.parse(raw)
  } catch {
    return raw
  }
}

/** 内部 tool-call 块 → 通用 `toolCall` 块。 */
function toToolCall(block: { id: string; name: string; arguments: string }) {
  return { type: 'toolCall', id: block.id, name: block.name, arguments: parseArgs(block.arguments) }
}

/** 通用 `toolCall` 块 → 内部 tool-call 块。 */
function fromToolCall(block: { id?: unknown; name?: unknown; arguments?: unknown }) {
  const rawArgs = block.arguments === undefined ? '' : JSON.stringify(block.arguments)
  return {
    type: 'tool-call' as const,
    id: String(block.id ?? ''),
    name: String(block.name ?? ''),
    arguments: rawArgs,
  }
}

function mapUsage(usage: TokenUsage): Record<string, unknown> {
  const input = usage.inputTokens ?? 0
  const output = usage.outputTokens ?? 0
  return { input, output, totalTokens: input + output }
}

function mapFinish(reason: FinishReason): { stopReason: string; rawStopReason: string } {
  switch (reason.kind) {
    case 'tool-calls':
      return { stopReason: 'toolUse', rawStopReason: 'tool_calls' }
    case 'stop':
      return { stopReason: 'stop', rawStopReason: 'stop' }
    case 'max-tokens':
      return { stopReason: 'maxTokens', rawStopReason: 'max_tokens' }
    case 'aborted':
      return { stopReason: 'aborted', rawStopReason: 'aborted' }
    case 'error':
      return { stopReason: 'error', rawStopReason: String((reason.failure as { code?: unknown } | null)?.code ?? 'error') }
  }
}

/**
 * 增量写端：把一批评分事件编码为通用事件行（维护 `id` 链、`callId → 工具名`、
 * 兄弟 toolResult 共享 assistant 父节点）。持有一个会话的连续状态。
 */
export class StandardWriter {
  lastId: string | null
  /** 当前 toolResult 群的父节点（最近一个带 toolCall 的 assistant 消息 id）。 */
  private toolParent: string | null = null
  private readonly callNames = new Map<string, string>()

  constructor(tailId: string | null = null) {
    this.lastId = tailId
  }

  /** 新文件/新段的 header 行（type=session）。 */
  header(id: SessionId, meta: { cwd?: string } = {}): string {
    const line: StandardEvent = {
      type: 'session',
      version: 3,
      id: String(id),
      parentId: null,
      timestamp: iso(Date.now()),
      ...(meta.cwd !== undefined ? { cwd: meta.cwd } : {}),
    }
    this.lastId = line.id
    return JSON.stringify(line)
  }

  /** 把一批内部事件编码为通用事件行（记账/transient 类型自动跳过）。 */
  encodeBatch(events: readonly SessionEvent[]): string[] {
    const out: string[] = []
    if (events.length === 0) return out
    // 预扫 chunk，收集每个 (turn,step) 的 usage/finish，用于丰富 assistant 消息。
    const usageBy = new Map<string, TokenUsage>()
    const finishBy = new Map<string, FinishReason>()
    for (const event of events) {
      if (event.type !== 'assistant/chunk') continue
      const key = `${event.data.turn}:${event.data.step}`
      const chunk = event.data.chunk
      if (chunk.type === 'usage') usageBy.set(key, chunk.usage)
      else if (chunk.type === 'finish') finishBy.set(key, chunk.reason)
    }
    for (const event of events) {
      const line = this.encodeOne(event, usageBy, finishBy)
      if (line !== null) out.push(line)
    }
    return out
  }

  private encodeOne(
    event: SessionEvent,
    usageBy: Map<string, TokenUsage>,
    finishBy: Map<string, FinishReason>,
  ): string | null {
    switch (event.type) {
      case 'user/message': {
        const data = event.data
        const parent = this.lastId
        // 保留 text + image 块，重启 resume 后图片仍在模型可见历史里
        const content: Array<Record<string, unknown>> = []
        for (const block of data.content) {
          if (block.type === 'text') {
            content.push({ type: 'text', text: block.text })
          } else if (block.type === 'image') {
            content.push({
              type: 'image',
              mime: block.mime,
              data: block.data,
              ...(block.name !== undefined ? { name: block.name } : {}),
            })
          }
        }
        if (content.length === 0) content.push({ type: 'text', text: textOf(data.content) })
        const line: StandardEvent = {
          type: 'message',
          id: hexId(event.seq),
          parentId: parent,
          timestamp: iso(event.time),
          message: {
            role: 'user',
            content,
            timestamp: event.time,
          },
        }
        this.lastId = line.id
        this.toolParent = null
        return JSON.stringify(line)
      }
      case 'assistant/message': {
        const data = event.data
        const content: Array<Record<string, unknown>> = []
        for (const block of data.message.content) {
          if (block.type === 'text') content.push({ type: 'text', text: block.text })
          else if (block.type === 'tool-call') content.push(toToolCall(block))
          // tool-result 块（本框架 assistant 消息不产出）跳过
        }
        const key = `${data.turn}:${data.step}`
        const usage = usageBy.get(key)
        const finish = finishBy.get(key)
        const message: Record<string, unknown> = {
          role: 'assistant',
          content,
          ...(usage === undefined ? {} : { usage: mapUsage(usage) }),
          ...(finish === undefined ? {} : mapFinish(finish)),
          timestamp: event.time,
        }
        const line: StandardEvent = {
          type: 'message',
          id: hexId(event.seq),
          parentId: this.lastId,
          timestamp: iso(event.time),
          message,
        }
        this.lastId = line.id
        // 带 toolCall 的 assistant 成为后续 toolResult 的父节点。
        this.toolParent = content.some((block) => block.type === 'toolCall') ? line.id : null
        return JSON.stringify(line)
      }
      case 'tool/call': {
        // 记账：记录 callId → 工具名，落盘行不输出。
        this.callNames.set(String(event.data.callId), event.data.name)
        return null
      }
      case 'tool/result': {
        const data = event.data
        const toolName = this.callNames.get(String(data.callId)) ?? 'tool'
        // 兄弟 toolResult 共享 assistant 父节点；链尾仍推进到本行 id。
        const parent = this.toolParent ?? this.lastId
        const content: Array<Record<string, unknown>> = [
          { type: 'text', text: String(data.message.content ?? '') },
        ]
        for (const img of data.message.images ?? []) {
          content.push({
            type: 'image',
            mime: img.mime,
            data: img.data,
            ...(img.name !== undefined ? { name: img.name } : {}),
          })
        }
        const line: StandardEvent = {
          type: 'message',
          id: hexId(event.seq),
          parentId: parent,
          timestamp: iso(event.time),
          message: {
            role: 'toolResult',
            toolCallId: String(data.callId),
            toolName,
            content,
            isError: data.message.isError === true,
            timestamp: event.time,
          },
        }
        this.lastId = line.id
        return JSON.stringify(line)
      }
      default:
        // turn/start、step/*、assistant/chunk、session/end-seed：记账事件，不落盘。
        return null
    }
  }
}

/**
 * 读端：把通用事件行解码回内部 SessionEvent（resume 种子）。
 * 一个 read 实例跨整个文件（或全部段）维持 turn/step 合成与 callId → 工具名；
 * 直接向调用方提供的输出数组 push，保证混合文件（raw + standard）的读取顺序。
 */
export class StandardReader {
  private turn = 0
  private step = 0
  private readonly callNames = new Map<string, string>()
  private readonly out: SessionEvent[]

  constructor(out: SessionEvent[] = []) {
    this.out = out
  }

  /** 解码一行（seq 由输出数组长度单调分配）；header / model_change 等元数据行产出 0 条。 */
  handle(line: StandardEvent): void {
    if (line.type !== 'message') return
    const message = line.message as Record<string, unknown> | undefined
    if (typeof message !== 'object' || message === null) return
    const role = String(message.role ?? '')
    const time = Number(message.timestamp ?? Date.parse(line.timestamp) ?? Date.now())
    const seq = this.out.length
    if (role === 'user') {
      const blocks = Array.isArray(message.content) ? message.content : []
      const content: Array<
        | { type: 'text'; text: string }
        | { type: 'image'; mime: string; data: string; name?: string }
      > = []
      for (const block of blocks) {
        const b = block as Record<string, unknown> | null
        if (b === null || typeof b !== 'object') continue
        if (b.type === 'text') {
          content.push({ type: 'text', text: String(b.text ?? '') })
        } else if (b.type === 'image' && typeof b.data === 'string' && b.data !== '') {
          content.push({
            type: 'image',
            mime: String(b.mime ?? 'image/png'),
            data: b.data,
            ...(typeof b.name === 'string' && b.name !== '' ? { name: b.name } : {}),
          })
        }
      }
      const text = content.filter((c) => c.type === 'text').map((c) => (c as { text: string }).text).join('\n')
      if (text === '' && content.every((c) => c.type !== 'image')) return
      this.turn += 1
      this.step = 0
      this.out.push({
        type: 'user/message',
        seq,
        time,
        data: {
          id: line.id,
          role: 'user',
          content: content.length > 0 ? content : [{ type: 'text', text }],
          source: { kind: 'human' },
        },
      } as unknown as SessionEvent)
      return
    }
    if (role === 'assistant') {
      const blocks = Array.isArray(message.content) ? message.content : []
      const content = blocks
        .map((block) => {
          const b = block as Record<string, unknown> | null
          if (b === null || typeof b !== 'object') return null
          if (b.type === 'text') return { type: 'text' as const, text: String(b.text ?? '') }
          if (b.type === 'toolCall') {
            if (b.id !== undefined) this.callNames.set(String(b.id), String(b.name ?? ''))
            return fromToolCall(b)
          }
          return null
        })
        .filter((block): block is NonNullable<typeof block> => block !== null)
      this.step += 1
      this.out.push({
        type: 'assistant/message',
        seq,
        time,
        data: {
          turn: this.turn,
          step: this.step,
          message: { id: line.id, role: 'assistant', content },
        },
      } as unknown as SessionEvent)
      return
    }
    if (role === 'toolResult') {
      const callId = String(message.toolCallId ?? '')
      const content = textOf(message.content)
      const images: Array<{ mime: string; data: string; name?: string }> = []
      const blocks = Array.isArray(message.content) ? message.content : []
      for (const block of blocks) {
        const b = block as Record<string, unknown> | null
        if (b && b.type === 'image' && typeof b.data === 'string' && b.data !== '') {
          images.push({
            mime: String(b.mime ?? 'image/png'),
            data: b.data,
            ...(typeof b.name === 'string' && b.name !== '' ? { name: b.name } : {}),
          })
        }
      }
      this.out.push({
        type: 'tool/result',
        seq,
        time,
        data: {
          turn: this.turn,
          step: this.step,
          callId,
          message: {
            callId,
            content,
            isError: message.isError === true,
            ...(images.length > 0 ? { images } : {}),
          },
        },
      } as unknown as SessionEvent)
    }
  }

  get events(): readonly SessionEvent[] {
    return this.out
  }
}

// 类型引用：确保 SessionEventMap 形状演进时编译期提醒。
export type { SessionEventMap, SessionId }