/**
 * deriveMessages 出口的 wire 裁剪（纯函数）：按新鲜度分层截断 + 超预算丢最旧整轮。
 * 只影响进模型的内容，不改写持久化日志（UI 历史仍完整）。
 *
 * 分层（越新越宽）：
 *   fresh  —— 最后一条 assistant 之后的 tool 结果（刚返回、模型正要用）：仅安全上限；
 *             各工具自身已限幅（sh 40k / read 50KB），正常触不到这条线。
 *   recent —— 最近 KEEP_RECENT_TURNS 条 user 起始的轮（含当前轮）：宽松上限。
 *   old    —— 更早的历史：紧上限，超预算按整轮从最旧丢弃。
 * 预算丢弃绝不进入 recent 窗口：排查类任务一轮可达数十步，把窗口内旧步骤裁掉
 * 会让模型失忆重查（步数暴涨）；窗口整体超预算时交由模型上下文窗口兜底。
 */
import type { MessageContent, ModelMessage } from '@cos/types'

/** 单条 tool 结果（保留窗口之外）进模型前的最大字符数。 */
export const MAX_TOOL_RESULT_CHARS = 1200
/** 刚返回（最新步骤）的 tool 结果安全上限。 */
export const MAX_FRESH_TOOL_RESULT_CHARS = 50_000
/** 保留窗口内单条 tool 结果的最大字符数（比旧历史放宽）。 */
export const RECENT_TOOL_RESULT_CHARS = 8000
/** 单条文本块进模型前的最大字符数。 */
export const MAX_TEXT_BLOCK_CHARS = 4000
/** 整段 wire 历史的近似字符预算（中文约 1 token/字；控制在数万 token 内）。 */
export const MAX_WIRE_CHARS = 100_000
/** 保留窗口覆盖的最近 user 轮数（含当前轮）。 */
export const KEEP_RECENT_TURNS = 3

function clipText(text: string, max: number): string {
  if (text.length <= max) return text
  return `${text.slice(0, max)}\n…[已截断，共 ${text.length} 字]`
}

function clipContent(content: MessageContent, cap: number): MessageContent {
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

/** 最新步骤起点：最后一条 assistant 之后的下标（其 tool 结果刚返回、尚未被回答）。 */
function freshStartIndex(messages: ModelMessage[]): number {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role === 'assistant') return i + 1
  }
  return messages.length
}

/** 保留窗口起点：倒数第 KEEP_RECENT_TURNS 条 user 消息的下标；不足该轮数时为 0（全保留）。 */
function recentWindowStart(messages: ModelMessage[]): number {
  let seen = 0
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role === 'user' && ++seen === KEEP_RECENT_TURNS) return i
  }
  return 0
}

/**
 * 进模型前裁剪 wire 历史：先按新鲜度分层掐超长 tool/文本，再从最旧整轮丢弃到
 * 字符预算内。丢弃只发生在保留窗口之前，切在 user 边界，不产生孤儿 tool 结果。
 */
export function trimForModel(messages: ModelMessage[]): ModelMessage[] {
  const freshStart = freshStartIndex(messages)
  const windowStart = recentWindowStart(messages)
  const trimmed = messages.map((m, i) => {
    const cap = m.role !== 'tool'
      ? MAX_TEXT_BLOCK_CHARS
      : i >= freshStart
        ? MAX_FRESH_TOOL_RESULT_CHARS
        : i >= windowStart
          ? RECENT_TOOL_RESULT_CHARS
          : MAX_TOOL_RESULT_CHARS
    return { ...m, content: clipContent(m.content, cap) }
  })
  const sizes = trimmed.map((m) => contentChars(m.content))
  let total = sizes.reduce((a, b) => a + b, 0)
  let start = 0
  while (total > MAX_WIRE_CHARS && start < windowStart) {
    // 按下一条 user 切整轮丢弃；窗口起点必是 user 边界，故 next 必存在且 ≤ windowStart。
    let next = start + 1
    while (next < trimmed.length && trimmed[next].role !== 'user') next++
    for (let i = start; i < next; i++) total -= sizes[i]
    start = next
  }
  return trimmed.slice(start)
}
