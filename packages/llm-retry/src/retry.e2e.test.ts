/**
 * llm-retry 全链路 e2e：真实 cordis Context 装配 agent-loop + llm-retry +
 * 易错适配器，验证 `agent/request-error` waterfall 上的退避重试端到端生效。
 * 两个场景：瞬时失败两次后恢复（turn 存活）；配额用尽后回到终局语义
 * （上抛 agent/error）。运行：pnpm --dir harness test
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { Context } from 'cordis'
import SessionsService from '@cos/session'
import AgentsRegistry from '@cos/agents'
import ToolsService from '@cos/tools'
import SystemPrompt from '@cos/system-prompt'
import LlmRuntime from '@cos/llm'
import { LlmAdapter } from '@cos/llm'
import AgentLoop from '@cos/agent-loop'
import * as llmRetry from '@cos/llm-retry'
import type { GenerateOptions, StreamChunk } from '@cos/types'
import { createUserMessage } from '@cos/types'

interface Harness {
  ctx: Context
  stats: { calls: number; requestErrors: number; agentErrors: number }
}

/** 装配最小服务树 + 前若干次必败的适配器 + llm-retry（毫秒级退避，不等真实节奏）。 */
async function harness(failures: number, retries: number): Promise<Harness> {
  const stats = { calls: 0, requestErrors: 0, agentErrors: 0 }
  const ctx = new Context()
  await ctx.plugin(SessionsService)
  await ctx.plugin(AgentsRegistry)
  await ctx.plugin(ToolsService)
  await ctx.plugin(SystemPrompt)
  await ctx.plugin(LlmRuntime)
  await ctx.plugin(AgentLoop, { maxStepsPerTurn: 5 })
  await ctx.plugin(llmRetry as unknown as Parameters<Context['plugin']>[0], { retries, baseDelayMs: 1, maxDelayMs: 4 })

  // 计数探针：waterfall 里 llm-retry 接管重试后下游看不到事件，须 prepend 且透传
  ctx.on('agent/request-error', (_payload, next) => {
    stats.requestErrors += 1
    return next()
  }, { prepend: true })
  ctx.on('agent/error', () => {
    stats.agentErrors += 1
  })

  const adapter = new (class extends LlmAdapter {
    async *stream(_request: GenerateOptions): AsyncGenerator<StreamChunk> {
      stats.calls += 1
      if (stats.calls <= failures) {
        const cause = Object.assign(new Error('connection reset'), { code: 'ECONNRESET' })
        throw new TypeError('fetch failed', { cause })
      }
      yield { type: 'block-start', index: 0, blockType: 'text' }
      yield { type: 'text-delta', index: 0, text: 'recovered' }
      yield { type: 'block-end', index: 0, block: { type: 'text', text: 'recovered' } }
      yield { type: 'finish', reason: { kind: 'stop' } }
    }
  })()
  ctx.llm.registerAdapter(['flaky'], adapter)
  return { ctx, stats }
}

/** 跑一轮对话，返回最终 assistant 文本。 */
async function drive(ctx: Context): Promise<string> {
  const { agent, dispose } = await ctx.agentLoop.createAgent({
    agentOptions: { provider: 'flaky', model: 'flaky-1' },
    meta: { ephemeral: true },
  })
  agent.followup(createUserMessage('hello', { kind: 'human', detail: 'e2e' }))
  await agent.whenIdle()
  const reply = agent.session.events
    .filter((event) => event.type === 'assistant/message')
    .at(-1)?.data.message.content
    ?.filter((block) => block.type === 'text')
    .map((block) => block.text)
    .join('') ?? ''
  await dispose()
  return reply
}

test('e2e：瞬时失败按配额重试后恢复，turn 存活', async () => {
  const { ctx, stats } = await harness(2, 3)
  try {
    const reply = await drive(ctx)
    assert.equal(stats.calls, 3)
    assert.equal(stats.requestErrors, 2)
    assert.equal(stats.agentErrors, 0)
    assert.equal(reply, 'recovered')
  } finally {
    await ctx.fiber.dispose()
  }
})

test('e2e：配额用尽回到终局语义（agent/error 上抛）', async () => {
  const { ctx, stats } = await harness(99, 2)
  try {
    const reply = await drive(ctx)
    assert.equal(stats.calls, 3)
    assert.equal(stats.requestErrors, 3)
    assert.equal(stats.agentErrors, 1)
    assert.equal(reply, '')
  } finally {
    await ctx.fiber.dispose()
  }
})
