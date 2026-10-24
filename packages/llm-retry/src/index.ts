/**
 * @cos/llm-retry — agent 请求失败的默认恢复策略（`agent/request-error`
 * waterfall 的最外层监听器）：瞬时失败（网络层 / 断流 / HTTP 408/429/5xx）
 * 指数退避后返回 `{ kind: 'retry' }` 让 agent-loop 重发当前 step；次数用尽、
 * 错误不可重试或已取消时交回 `next()`（无后续监听器即维持「直接失败」的
 * 原语义）。作用于所有 agent（含 subagents 的 worker）。退避等待可被
 * AbortSignal 打断：取消立即上抛，不拖着死会话等计时器。
 * @module @cos/llm-retry
 */

import type { Context } from 'cordis'
import type { Agent } from '@cos/types'
import { backoffDelayMs, StepAttempts } from './backoff.ts'
import { describeFailure, isTransientFailure } from './classify.ts'

export const name = 'llm-retry'

export interface LlmRetryConfig {
  /** 每个 (turn, step) 最多重试几次（不含首次尝试；0 = 关闭重试）。 */
  retries?: number
  /** 第一次重试前等待的毫秒，之后逐次翻倍。 */
  baseDelayMs?: number
  /** 单次退避等待的上限。 */
  maxDelayMs?: number
}

/** 可被 AbortSignal 打断的 sleep：打断即以 signal.reason 拒绝。 */
function abortableSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort)
      resolve()
    }, ms)
    function onAbort(): void {
      clearTimeout(timer)
      reject(signal.reason)
    }
    signal.addEventListener('abort', onAbort, { once: true })
    if (signal.aborted) onAbort()
  })
}

export function apply(ctx: Context, config: LlmRetryConfig = {}) {
  const retries = Math.max(0, Math.floor(config.retries ?? 3))
  const baseMs = Math.max(0, config.baseDelayMs ?? 500)
  const maxMs = Math.max(0, config.maxDelayMs ?? 8000)
  if (retries === 0) return

  const attempts = new WeakMap<Agent, StepAttempts>()

  ctx.on('agent/request-error', async ({ agent, turn, step, failure, signal }, next) => {
    if (signal.aborted || !isTransientFailure(failure)) return next()
    const counter = attempts.get(agent) ?? new StepAttempts()
    attempts.set(agent, counter)
    const failureNo = counter.record(`${turn}:${step}`)
    if (failureNo > retries) return next()
    const delay = backoffDelayMs(failureNo, baseMs, maxMs)
    console.warn(
      `[llm-retry] transient failure at turn ${turn} step ${step} `
      + `(${describeFailure(failure)}), retry ${failureNo}/${retries} in ${delay}ms`,
    )
    await abortableSleep(delay, signal)
    return { kind: 'retry' }
  })
}
