/**
 * 图片降级单测：纯函数折叠 + `llm/stream` 上的拒收重发（拒收→去图重试成功、
 * 降级仍败上抛原始错误、瞬时/半路断流不降级、retries=0 不影响降级）。
 * 运行：pnpm --dir harness test
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { Context } from 'cordis'
import LlmRuntime, { LlmAdapter } from '@cos/llm'
import { LlmError, SessionId } from '@cos/types'
import type { GenerateOptions, ModelMessage, StreamChunk } from '@cos/types'

import * as llmRetry from './index.ts'
import { hasImageBlocks, stripImageBlocks } from './degrade.ts'

const userWithImages = (n: number): ModelMessage => ({
  role: 'user',
  content: [
    ...Array.from({ length: n }, () => ({ type: 'image' as const, mime: 'image/png', data: 'aGk=' })),
    { type: 'text' as const, text: '看看' },
  ],
})

test('hasImageBlocks / stripImageBlocks：计数折叠与无图浅等', () => {
  const plain: ModelMessage = { role: 'user', content: [{ type: 'text', text: '纯文本' }] }
  const toolImg: ModelMessage = {
    role: 'tool',
    callId: 'c1',
    content: [{ type: 'image', mime: 'image/jpeg', data: 'eHg=' }],
  }
  assert.equal(hasImageBlocks([plain]), false)
  assert.equal(hasImageBlocks([userWithImages(1), plain]), true)
  assert.equal(hasImageBlocks([toolImg]), true)

  const stripped = stripImageBlocks([plain, userWithImages(2)])
  assert.equal(stripped[0], plain, '无图消息原对象返回')
  const texts = stripped[1]!.content.filter((b) => b.type === 'text')
  assert.equal(texts.length, 2)
  assert.ok(texts[0].type === 'text' && texts[0].text.includes('2 张图片'))
  assert.ok(texts[0].text.includes('未送达'))
  assert.deepEqual(texts[1], { type: 'text', text: '看看' })
})

/** 拒收带图请求的适配器：首调（含图）4xx，去图后的重发成功并回报看见了注释。 */
function rejectingAdapter(calls: { count: number; sawNote: boolean[]; failSecond: boolean }): LlmAdapter {
  return new (class extends LlmAdapter {
    async *stream(request: GenerateOptions): AsyncGenerator<StreamChunk> {
      const call = ++calls.count
      const hasImg = request.messages.some((m) => m.content.some((b) => b.type === 'image'))
      const note = request.messages.some((m) =>
        m.content.some((b) => b.type === 'text' && b.text.includes('未送达')))
      calls.sawNote[call - 1] = note
      if (hasImg) throw new LlmError('PROVIDER_ERROR', 'flaky request failed: 400 image content not supported')
      if (call === 2 && calls.failSecond) throw new LlmError('PROVIDER_ERROR', 'flaky request failed: 401 unauthorized')
      yield { type: 'block-start', index: 0, blockType: 'text' }
      yield { type: 'text-delta', index: 0, text: 'ok' }
      yield { type: 'block-end', index: 0, block: { type: 'text', text: 'ok' } }
      yield { type: 'finish', reason: { kind: 'stop' } }
    }
  })()
}

async function harness(
  adapter: LlmAdapter,
  retries = 3,
): Promise<{ ctx: Context; stream: (messages: ModelMessage[]) => Promise<string> }> {
  const ctx = new Context()
  await ctx.plugin(LlmRuntime)
  await ctx.plugin(llmRetry as unknown as Parameters<Context['plugin']>[0], {
    retries,
    baseDelayMs: 1,
    maxDelayMs: 2,
  })
  ctx.llm.registerAdapter(['flaky'], adapter)
  const request = (messages: ModelMessage[]): GenerateOptions => ({
    provider: 'flaky',
    model: 'flaky-1',
    messages,
    sessionId: SessionId('degrade-test'),
    signal: new AbortController().signal,
  })
  const stream = async (messages: ModelMessage[]): Promise<string> => {
    let text = ''
    for await (const chunk of ctx.llm.stream(request(messages))) {
      if (chunk.type === 'text-delta') text += chunk.text
    }
    return text
  }
  return { ctx, stream }
}

test('拒收带图请求 → 去图重发成功，模型看见「未送达」注释', async () => {
  const calls = { count: 0, sawNote: [] as boolean[], failSecond: false }
  const { ctx, stream } = await harness(rejectingAdapter(calls))
  try {
    const text = await stream([userWithImages(1)])
    assert.equal(text, 'ok')
    assert.equal(calls.count, 2)
    assert.equal(calls.sawNote[1], true, '重发的请求里应带降级注释')
  } finally {
    await ctx.fiber.dispose()
  }
})

test('降级重发仍失败 → 上抛原始错误（不被降级尝试掩盖）', async () => {
  const calls = { count: 0, sawNote: [] as boolean[], failSecond: true }
  const { ctx, stream } = await harness(rejectingAdapter(calls))
  try {
    await assert.rejects(
      () => stream([userWithImages(1)]),
      /400 image content not supported/,
      '应上抛首个 400，而不是降级尝试的 401',
    )
    assert.equal(calls.count, 2)
  } finally {
    await ctx.fiber.dispose()
  }
})

test('瞬时失败不降级（交给退避重试路径，图片原样保留）', async () => {
  let calls = 0
  const adapter = new (class extends LlmAdapter {
    async *stream(): AsyncGenerator<StreamChunk> {
      calls += 1
      const cause = Object.assign(new Error('connection reset'), { code: 'ECONNRESET' })
      throw new TypeError('fetch failed', { cause })
    }
  })()
  const { ctx, stream } = await harness(adapter)
  try {
    await assert.rejects(() => stream([userWithImages(1)]), /fetch failed/)
    assert.equal(calls, 1, '瞬时错误不应触发去图重发')
  } finally {
    await ctx.fiber.dispose()
  }
})

test('半路断流（已吐 chunk）不降级', async () => {
  let calls = 0
  const adapter = new (class extends LlmAdapter {
    async *stream(): AsyncGenerator<StreamChunk> {
      calls += 1
      yield { type: 'block-start', index: 0, blockType: 'text' }
      throw new LlmError('PROVIDER_ERROR', 'flaky stream error: 400 bad')
    }
  })()
  const { ctx, stream } = await harness(adapter)
  try {
    await assert.rejects(() => stream([userWithImages(1)]), /stream error: 400/)
    assert.equal(calls, 1)
  } finally {
    await ctx.fiber.dispose()
  }
})

test('retries=0 只关退避，图片降级仍生效', async () => {
  const calls = { count: 0, sawNote: [] as boolean[], failSecond: false }
  const { ctx, stream } = await harness(rejectingAdapter(calls), 0)
  try {
    const text = await stream([userWithImages(3)])
    assert.equal(text, 'ok')
    assert.equal(calls.count, 2)
  } finally {
    await ctx.fiber.dispose()
  }
})

test('无图请求零开销直通', async () => {
  const calls = { count: 0, sawNote: [] as boolean[], failSecond: false }
  const { ctx, stream } = await harness(rejectingAdapter(calls))
  try {
    const text = await stream([{ role: 'user', content: [{ type: 'text', text: '纯文字' }] }])
    assert.equal(text, 'ok')
    assert.equal(calls.count, 1)
  } finally {
    await ctx.fiber.dispose()
  }
})
