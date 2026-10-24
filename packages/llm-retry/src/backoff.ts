/**
 * @cos/llm-retry/backoff — 重试节奏（纯逻辑）：指数退避与按 (turn, step) 的
 * 失败计数。计数只记「已决定重试的瞬时失败」；key 一变（新 step / 新 turn）
 * 即自动归零，单 agent 单条记录，无泄漏。
 * @module @cos/llm-retry/backoff
 */

/** 第 `attempt` 次重试前的等待毫秒（1 起）：base × 2^(attempt-1)，封顶 maxMs。 */
export function backoffDelayMs(attempt: number, baseMs: number, maxMs: number): number {
  const raw = baseMs * 2 ** (attempt - 1)
  return Math.min(Math.max(Math.ceil(raw), 0), Math.max(maxMs, 0))
}

/** 单 agent 的连续失败计数：一个 key = 同一 turn 的同一 step。 */
export class StepAttempts {
  private key: string | null = null
  private count = 0

  /** 记一次失败，返回这是该 key 的第几次（1 起）；换 key 自动重开。 */
  record(key: string): number {
    if (key !== this.key) {
      this.key = key
      this.count = 0
    }
    this.count += 1
    return this.count
  }
}
