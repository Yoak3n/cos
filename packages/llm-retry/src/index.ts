/**
 * @cos/llm-retry — agent 请求失败的默认恢复策略（`agent/request-error`
 * waterfall 的最外层监听器）：瞬时失败（网络层 / 断流 / HTTP 408/429/5xx）
 * 指数退避后返回 `{ kind: 'retry' }` 让 agent-loop 重发当前 step；次数用尽、
 * 错误不可重试或已取消时交回 `next()`（无后续监听器即维持「直接失败」的
 * 原语义）。作用于所有 agent（含 subagents 的 worker）。退避等待可被
 * AbortSignal 打断：取消立即上抛，不拖着死会话等计时器。
 *
 * 另含图片降级（`llm/stream` waterfall，不受 retries=0 影响）：带图请求被
 * provider 拒收（非瞬时、尚未吐出任何 chunk）时，去图换占位文本重发一次——
 * 模型至少知道「用户发过图、因模型侧不支持而未能查看」，回合不因整单 4xx 崩掉。
 * @module @cos/llm-retry
 */

import type { Context } from 'cordis'
import type { Agent, GenerateOptions, StreamChunk } from '@cos/types'
import { backoffDelayMs, StepAttempts } from './backoff.ts'
import { describeFailure, isTransientFailure } from './classify.ts'
import { hasImageBlocks, stripImageBlocks } from './degrade.ts'

export const name = 'llm-retry'

/** 图片降级要经 ctx.llm.stream 重发，须声明依赖 llm 服务。 */
export const inject = ['llm']

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
  const baseMs = Math.max(0, Math.floor(config.baseDelayMs ?? 500))
  const maxMs = Math.max(0, Math.floor(config.maxDelayMs ?? 8000))

  // 图片降级独立于退避配额：retries=0 关的是「等一等再试」，不是「换个内容试」。
  ctx.on('llm/stream', (options, next) => degradeOnImageReject(ctx, options, next))

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

/**
 * 图片降级重发：仅在「带图请求被拒收」时出手——非瞬时失败、未吐出任何
 * chunk（请求整体被拒而非半路断流）、未被取消。降级重发走完整
 * `ctx.llm.stream` 管线（重入 waterfall；去图后本监听器自然短路），
 * 仍失败则上抛原始错误（真实病因，不让降级尝试掩盖它）。
 */
async function* degradeOnImageReject(
  ctx: Context,
  options: GenerateOptions,
  next: () => AsyncIterable<StreamChunk>,
): AsyncGenerator<StreamChunk> {
  if (!hasImageBlocks(options.messages)) {
    yield* next()
    return
  }
  let delivered = false
  try {
    for await (const chunk of next()) {
      delivered = true
      yield chunk
    }
    return
  } catch (error) {
    if (delivered || options.signal.aborted || isTransientFailure(error)) throw error
    const degraded = stripImageBlocks(options.messages)
    console.warn(
      `[llm-retry] image-bearing request rejected (${describeFailure(error)}); `
      + 'retrying without images so the model can acknowledge the undelivered image',
    )
    try {
      yield* ctx.llm.stream({ ...options, messages: degraded })
    } catch {
      throw error
    }
  }
}
