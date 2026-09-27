/**
 * trimForModel 分层裁剪单测：fresh/recent/old 三档上限 + 预算丢弃不进保留窗口。
 * 运行：pnpm --dir harness test
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  KEEP_RECENT_TURNS,
  MAX_FRESH_TOOL_RESULT_CHARS,
  MAX_TEXT_BLOCK_CHARS,
  MAX_TOOL_RESULT_CHARS,
  MAX_WIRE_CHARS,
  RECENT_TOOL_RESULT_CHARS,
  trimForModel,
} from './trim'
import type { MessageContent, ModelMessage } from '@cos/types'

const user = (text: string): ModelMessage => ({ role: 'user', content: [{ type: 'text', text }] })
const assistant = (text: string, callId?: string): ModelMessage => ({
  role: 'assistant',
  content: callId === undefined
    ? [{ type: 'text', text }]
    : [{ type: 'text', text }, { type: 'tool-call', id: callId, name: 'sh', arguments: '{}' }],
})
const tool = (callId: string, text: string): ModelMessage => ({
  role: 'tool',
  callId,
  content: [{ type: 'text', text }],
})

const textOf = (m: ModelMessage): string => {
  const block = m.content[0]
  assert.ok(block?.type === 'text')
  return block.text
}

/** 已被截断（带标记）且长度落在 cap 附近。 */
const assertClipped = (m: ModelMessage, cap: number): void => {
  const text = textOf(m)
  assert.ok(text.includes('已截断'), `expected truncation marker, got length ${text.length}`)
  assert.ok(text.length > cap && text.length <= cap + 40, `expected clip to ~${cap}, got ${text.length}`)
}

/** 原样保留（无截断标记）。 */
const assertIntact = (m: ModelMessage, length: number): void => {
  const text = textOf(m)
  assert.equal(text.length, length)
  assert.ok(!text.includes('已截断'))
}

/** 一轮：user 提问 + assistant 带 tool-call + tool 结果。 */
const turn = (n: number, resultChars: number): ModelMessage[] => [
  user(`第${n}轮`),
  assistant(`查${n}`, `a${n}`),
  tool(`a${n}`, 'w'.repeat(resultChars)),
]

const allTexts = (messages: ModelMessage[]): string[] =>
  messages.flatMap((m): string[] => m.content.flatMap((b: MessageContent[number]): string[] => b.type === 'text' ? [b.text] : []))

test('empty history stays empty', () => {
  assert.deepEqual(trimForModel([]), [])
})

test('fresh tool result (after last assistant) is preserved', () => {
  const messages = [...turn(1, 20_000)]
  const trimmed = trimForModel(messages)
  assert.equal(trimmed.length, 3)
  assertIntact(trimmed[2]!, 20_000)
})

test('fresh tool result is still bounded by the safety cap', () => {
  const messages = [...turn(1, MAX_FRESH_TOOL_RESULT_CHARS + 10_000)]
  const trimmed = trimForModel(messages)
  assertClipped(trimmed[2]!, MAX_FRESH_TOOL_RESULT_CHARS)
})

test('recent window keeps relaxed cap; older turns keep the tight cap', () => {
  const messages = [
    ...turn(1, RECENT_TOOL_RESULT_CHARS + 1000),
    ...turn(2, RECENT_TOOL_RESULT_CHARS + 1000),
    ...turn(3, RECENT_TOOL_RESULT_CHARS + 1000),
    user('第四轮'),
    assistant('结论'),
  ]
  const trimmed = trimForModel(messages)
  // 总量未超预算，全量保留；窗口 = 最近 KEEP_RECENT_TURNS 条 user 起始的轮。
  assert.equal(trimmed.length, messages.length)
  assert.equal(textOf(trimmed[0]!), '第1轮')
  assertClipped(trimmed[2]!, MAX_TOOL_RESULT_CHARS)
  assertClipped(trimmed[5]!, RECENT_TOOL_RESULT_CHARS)
  assertClipped(trimmed[8]!, RECENT_TOOL_RESULT_CHARS)
})

test('assistant text blocks keep their cap', () => {
  const trimmed = trimForModel([user('问'), assistant('z'.repeat(MAX_TEXT_BLOCK_CHARS + 500))])
  assertClipped(trimmed[1]!, MAX_TEXT_BLOCK_CHARS)
  assertIntact(trimmed[0]!, 1)
})

test('budget drop removes whole oldest turns and never enters the window', () => {
  // 60 轮 × 40k：紧上限裁剪后旧轮仅 ~1.2k/轮，总量仍超预算 → 从最旧整轮丢弃。
  const messages: ModelMessage[] = []
  for (let n = 1; n <= 60; n++) messages.push(...turn(n, 40_000))
  const trimmed = trimForModel(messages)
  // 丢弃切在 user 边界，不产生孤儿 tool 结果。
  assert.equal(trimmed[0]!.role, 'user')
  assert.ok(!allTexts(trimmed).some((t) => t.startsWith('第1轮')), 'oldest turn should be dropped')
  // 窗口三轮完整保留：两轮 recent 上限 + 最新一轮 fresh 原样。
  const tail = trimmed.slice(-9)
  assertClipped(tail[2]!, RECENT_TOOL_RESULT_CHARS)
  assertClipped(tail[5]!, RECENT_TOOL_RESULT_CHARS)
  assertIntact(tail[8]!, 40_000)
  assert.ok(allTexts(trimmed).includes('第58轮'), 'window turns survive')
})

test('a single long turn is never gutted (investigation amnesia regression)', () => {
  const messages: ModelMessage[] = [user('排查一个简单问题')]
  for (let step = 0; step < 24; step++) {
    messages.push(assistant(`step ${step}`, `a${step}`))
    messages.push(tool(`a${step}`, 'v'.repeat(5000)))
  }
  assert.ok(24 * 5000 > MAX_WIRE_CHARS)
  const trimmed = trimForModel(messages)
  // 只有一条 user 轮：窗口覆盖全部，绝不裁剪中间步骤。
  assert.equal(trimmed.length, messages.length)
  assertIntact(trimmed[2]!, 5000)
  assert.equal(textOf(trimmed[0]!), '排查一个简单问题')
})

test(`history within ${KEEP_RECENT_TURNS} turns is never dropped even over budget`, () => {
  const messages = [...turn(1, 40_000), ...turn(2, 40_000), ...turn(3, 40_000)]
  const trimmed = trimForModel(messages)
  assert.equal(trimmed.length, messages.length)
  assertClipped(trimmed[2]!, RECENT_TOOL_RESULT_CHARS)
  assertIntact(trimmed[8]!, 40_000)
})
