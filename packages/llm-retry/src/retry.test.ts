/**
 * llm-retry 单测：失败分类（cause 链 / HTTP 状态码 / 域错误 / 取消）、指数
 * 退避取值、按 (turn, step) 的失败计数。运行：pnpm --dir harness test
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { LlmError } from '@cos/types'
import type { Agent } from '@cos/types'
import type { Context } from 'cordis'
import { apply } from './index.ts'
import { backoffDelayMs, StepAttempts } from './backoff.ts'
import { describeFailure, isTransientFailure } from './classify.ts'

/** undici 网络层错误的形状：TypeError('fetch failed') + cause 带 errno。 */
function undiciFailure(code: string): TypeError {
  const cause = Object.assign(new Error('connect timeout'), { code })
  return new TypeError('fetch failed', { cause })
}

test('isTransientFailure：网络层瞬时错误可重试', () => {
  assert.equal(isTransientFailure(undiciFailure('ECONNRESET')), true)
  assert.equal(isTransientFailure(undiciFailure('ETIMEDOUT')), true)
  assert.equal(isTransientFailure(undiciFailure('EAI_AGAIN')), true)
  // 无 cause 的裸 fetch failed 也是网络层签名
  assert.equal(isTransientFailure(new TypeError('fetch failed')), true)
  // undici 断流：body 中途被砍
  assert.equal(isTransientFailure(new TypeError('terminated')), true)
  assert.equal(isTransientFailure(new Error('socket hang up')), true)
})

test('isTransientFailure：happy-eyeballs 的 AggregateError 深链可重试', () => {
  const addr = Object.assign(new Error('dns'), { code: 'EAI_AGAIN' })
  const aggregate = new AggregateError([addr], 'all failed')
  assert.equal(isTransientFailure(new TypeError('fetch failed', { cause: aggregate })), true)
})

test('isTransientFailure：HTTP 408/429/5xx 可重试，其余 PROVIDER_ERROR 终局', () => {
  assert.equal(isTransientFailure(new LlmError('PROVIDER_ERROR', 'custom provider x request failed: 429 rate limit')), true)
  assert.equal(isTransientFailure(new LlmError('PROVIDER_ERROR', 'deepseek request failed: 502 bad gateway')), true)
  assert.equal(isTransientFailure(new LlmError('PROVIDER_ERROR', 'request failed: 408 timeout')), true)
  assert.equal(isTransientFailure(new LlmError('PROVIDER_ERROR', 'request failed: 401 unauthorized')), false)
  assert.equal(isTransientFailure(new LlmError('PROVIDER_ERROR', 'custom provider x: 未配置 API Base URL（设置面板填写）')), false)
})

test('isTransientFailure：网关把上游故障包成 400 + server_error 时仍可重试', () => {
  const wrapped = 'opencode-go request failed: 400 {"error":{"type":"server_error","code":"server_error",'
    + '"message":"Upstream request failed: [server_error] Upstream response was not valid JSON"}}'
  assert.equal(isTransientFailure(new LlmError('PROVIDER_ERROR', wrapped)), true)
  const invalid = 'gateway request failed: 400 {"error":{"type":"invalid_request_error","message":"bad tools schema"}}'
  assert.equal(isTransientFailure(new LlmError('PROVIDER_ERROR', invalid)), false)
})

test('isTransientFailure：跨模块副本的 PROVIDER_ERROR 按鸭子类型判（不依赖 instanceof）', () => {
  // 同一引擎可能同时装有挂载版与安装版 @cos 模块，LlmError 的类身份不相等；
  // 插件边界上拿到的可能只是同形对象。
  const foreign = (message: string) => Object.assign(new Error(message), { code: 'PROVIDER_ERROR' })
  assert.equal(isTransientFailure(foreign('graph request failed: 503 upstream unavailable')), true)
  assert.equal(isTransientFailure(foreign('graph request failed: 400 {"error":{"type":"server_error"}}')), true)
  assert.equal(isTransientFailure(foreign('graph request failed: 401 unauthorized')), false)
})

test('isTransientFailure：域错误与取消永不重试', () => {
  assert.equal(isTransientFailure(new LlmError('NO_ADAPTER', 'no adapter registered for provider "x"')), false)
  assert.equal(isTransientFailure(new LlmError('INVALID_STREAM', 'text-delta without open block 0')), false)
  // fetch failed 的 cause 是取消：重试是对着死会话白等
  const aborted = Object.assign(new Error('This operation was aborted'), { name: 'AbortError' })
  assert.equal(isTransientFailure(new TypeError('fetch failed', { cause: aborted })), false)
  assert.equal(isTransientFailure(Object.assign(new Error('aborted'), { code: 'UND_ERR_ABORTED' })), false)
  assert.equal(isTransientFailure(new Error('whatever else')), false)
})

test('describeFailure：把 cause 链上的 errno 带进日志摘要', () => {
  assert.equal(describeFailure(undiciFailure('ECONNRESET')), 'fetch failed (ECONNRESET)')
  assert.equal(describeFailure(new Error('socket hang up')), 'socket hang up')
})

test('backoffDelayMs：指数退避并封顶', () => {
  assert.equal(backoffDelayMs(1, 500, 8000), 500)
  assert.equal(backoffDelayMs(2, 500, 8000), 1000)
  assert.equal(backoffDelayMs(3, 500, 8000), 2000)
  assert.equal(backoffDelayMs(5, 500, 8000), 8000)
  assert.equal(backoffDelayMs(9, 500, 8000), 8000)
})

test('StepAttempts：同一 step 累计，换 step 归零', () => {
  const counter = new StepAttempts()
  assert.equal(counter.record('6:1'), 1)
  assert.equal(counter.record('6:1'), 2)
  assert.equal(counter.record('6:2'), 1)
  assert.equal(counter.record('7:1'), 1)
})

/** 装配 apply 捕获其监听器，直接在 waterfall 契约上驱动（next = 默认终局）。 */
function mountListener(config?: Record<string, number>): (failure: unknown, signal?: AbortSignal) => Promise<unknown> {
  let listener!: (payload: any, next: () => Promise<unknown>) => Promise<unknown>
  const fakeCtx = { on: (_name: string, cb: typeof listener) => { listener = cb } } as unknown as Context
  apply(fakeCtx, config)
  const agent = {} as Agent
  const decline = async () => undefined
  return (failure, signal = new AbortController().signal) =>
    listener({ agent, turn: 6, step: 1, failure, signal }, decline)
}

test('apply：瞬时失败按配额重试，用尽后交回终局', async () => {
  const decide = mountListener({ retries: 2, baseDelayMs: 0 })
  const flaky = undiciFailure('ECONNRESET')
  assert.deepEqual(await decide(flaky), { kind: 'retry' })
  assert.deepEqual(await decide(flaky), { kind: 'retry' })
  assert.equal(await decide(flaky), undefined)
})

test('apply：不可重试失败与已取消信号直接终局', async () => {
  const decide = mountListener({ retries: 3, baseDelayMs: 0 })
  assert.equal(await decide(new LlmError('NO_ADAPTER', 'no adapter')), undefined)
  const aborted = new AbortController()
  aborted.abort()
  assert.equal(await decide(undiciFailure('ECONNRESET'), aborted.signal), undefined)
})

test('apply：retries: 0 只关退避监听（图片降级仍注册）', () => {
  const events: string[] = []
  const fakeCtx = { on: (name: string) => { events.push(name); return () => {} } } as unknown as Context
  apply(fakeCtx, { retries: 0 })
  assert.deepEqual(events, ['llm/stream'])
})

test('apply：retries>0 时退避与降级监听都注册', () => {
  const events: string[] = []
  const fakeCtx = { on: (name: string) => { events.push(name); return () => {} } } as unknown as Context
  apply(fakeCtx, { retries: 3 })
  assert.deepEqual(events, ['llm/stream', 'agent/request-error'])
})
