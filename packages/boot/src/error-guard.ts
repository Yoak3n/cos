/**
 * boot 后运行期未捕获错误分级兜底（决策 6，2026-09-25 拍板）。
 *
 * 分级策略：
 *  - **非核心插件**（用户面插件）：记日志 + 滑窗熔断——窗口内超限即停用该插件的
 *    cordis fiber（重启恢复；持久禁用在 cordis.patch.yml 设 `disabled: true`），
 *    进程存活，不阻断核心。
 *  - **引擎 / 核心插件 / 无法归属**：立即 panic（打 FATAL 日志 + 非零退出，
 *    交壳侧 fail-loud 报障，禁止以半残僵尸态继续服务）。
 *
 * 归属判定（`classifyErrorOrigin`，纯函数）：沿错误栈（含 `cause` 链）自顶向下
 * 找第一个「有主」帧——node_modules / node: 帧跳过（vendor 内部），
 * `plugins/<name>/` 帧归该插件（dev 的 `cos-plugins/<name>/` 同样命中），
 * `harness/` 帧归引擎；其余视同核心（安全侧 panic）。
 *
 * @module @cos/boot/error-guard
 */

/** 核心插件清单（引擎之外）：产品通道 + 插件间共享库。出错即 panic。 */
export const DEFAULT_CORE_PLUGINS: readonly string[] = [
  '@diver/backend', // UI 的 HTTP/SSE 通道，挂了产品即死
  '@diver/native-bridge', // 其它插件的共享 RPC 库，坏了会连坐
]

export type ErrorOrigin =
  | { kind: 'core'; where: 'engine' | 'core-plugin' | 'unknown'; name?: string }
  | { kind: 'plugin'; name: string }

const PLUGIN_FRAME_RE = /(?:^|[/\\])(?:cos-)?plugins[/\\]([^/\\]+)[\\/]/
const ENGINE_FRAME_RE = /(?:^|[/\\])harness[/\\]/
const VENDOR_FRAME_RE = /(?:^|[/\\])node_modules[/\\]/

/** 抽取一行 V8 栈帧里的文件路径（`at fn (path:line:col)` / `at path:line:col`）。 */
function framePath(line: string): string | undefined {
  const m = /^\s*at\s+(?:.+?\s+\()?(.+?):\d+:\d+\)?\s*$/.exec(line)
  return m?.[1]
}

/** 收集错误及其 cause 链上的栈文本（根因在前——最内层 cause 优先归属）。 */
function stackTexts(reason: unknown, depth = 4): string[] {
  const out: string[] = []
  let cur: unknown = reason
  for (let i = 0; i < depth && cur !== null && typeof cur === 'object'; i += 1) {
    const stack = (cur as { stack?: unknown }).stack
    if (typeof stack === 'string') out.push(stack)
    cur = (cur as { cause?: unknown }).cause
  }
  return out.reverse()
}

/** 归属分类（纯函数）：决定 panic 范围与熔断对象。 */
export function classifyErrorOrigin(
  reason: unknown,
  corePlugins: ReadonlySet<string> = new Set(DEFAULT_CORE_PLUGINS),
): ErrorOrigin {
  for (const text of stackTexts(reason)) {
    for (const line of text.split('\n')) {
      const path = framePath(line)
      if (!path || path.startsWith('node:') || VENDOR_FRAME_RE.test(path)) continue
      const m = PLUGIN_FRAME_RE.exec(path)
      if (m) {
        const dirName = m[1]
        const name = dirName.startsWith('@') ? dirName : `@diver/${dirName}`
        const core = corePlugins.has(name) || corePlugins.has(dirName)
        return core ? { kind: 'core', where: 'core-plugin', name } : { kind: 'plugin', name }
      }
      if (ENGINE_FRAME_RE.test(path)) return { kind: 'core', where: 'engine' }
      return { kind: 'core', where: 'unknown' }
    }
  }
  return { kind: 'core', where: 'unknown' }
}

export interface Breaker {
  /** 记一次错误，返回窗口内计数与是否恰好熔断。 */
  record(name: string): { count: number; tripped: boolean }
  /** 该插件是否已熔断。 */
  has(name: string): boolean
}

/** 滑动窗口熔断器：窗口内达到 limit 次即熔断。 */
export function createBreaker(limit: number, windowMs: number, now: () => number = Date.now): Breaker {
  const hits = new Map<string, number[]>()
  const tripped = new Set<string>()
  return {
    record(name) {
      const t = now()
      const arr = (hits.get(name) ?? []).filter((x) => t - x < windowMs)
      arr.push(t)
      hits.set(name, arr)
      if (arr.length >= limit) tripped.add(name)
      return { count: arr.length, tripped: tripped.has(name) && arr.length === limit }
    },
    has: (name) => tripped.has(name),
  }
}

/** 守卫可依赖的最小 loader 面（结构类型，避免 cordis 类型耦合）。 */
interface LoaderLike {
  entries(): Iterable<{
    options?: { name?: string; id?: string }
    fiber?: { dispose(): unknown }
  }>
}
interface CtxLike { loader?: LoaderLike }
interface EventTargetLike {
  on(event: string, listener: (arg: unknown) => void): unknown
  removeListener(event: string, listener: (arg: unknown) => void): unknown
}

export interface GuardDeps {
  /** cordis 上下文（熔断时按名停用插件 fiber）；缺失则只做会话内忽略。 */
  ctx?: CtxLike
  /** 核心插件清单（包名或目录名均可），默认 DEFAULT_CORE_PLUGINS。 */
  corePlugins?: readonly string[]
  /** 熔断阈值（窗口内次数），默认 5。 */
  breakerLimit?: number
  /** 熔断窗口毫秒，默认 60_000。 */
  breakerWindowMs?: number
  log?: (level: 'warn' | 'error', message: string) => void
  exit?: (code: number) => void
}

function describe(reason: unknown): string {
  if (reason instanceof Error) return reason.stack ?? String(reason)
  return String(reason)
}

/** 按名停用插件 fiber（best effort）。 */
function disposePlugin(ctx: CtxLike | undefined, name: string): boolean {
  try {
    for (const entry of ctx?.loader?.entries() ?? []) {
      const row = entry.options?.name ?? ''
      const id = entry.options?.id ?? ''
      if (row === name || `@diver/${id}` === name || id === name) {
        void entry.fiber?.dispose()
        return true
      }
    }
  } catch { /* best effort */ }
  return false
}

/** 生成守卫处理函数（纯逻辑可单测；process 接线在 installErrorGuard）。 */
export function createGuardHandler(deps: GuardDeps = {}): (reason: unknown, label: string) => void {
  const corePlugins = new Set(deps.corePlugins ?? DEFAULT_CORE_PLUGINS)
  const limit = deps.breakerLimit ?? 5
  const windowMs = deps.breakerWindowMs ?? 60_000
  const breaker = createBreaker(limit, windowMs)
  const log = deps.log ?? ((level, message) => {
    if (level === 'error') console.error(message)
    else console.warn(message)
  })
  const exit = deps.exit ?? ((code) => process.exit(code))
  return (reason, label) => {
    const origin = classifyErrorOrigin(reason, corePlugins)
    if (origin.kind !== 'plugin') {
      log('error', `[cos][guard][FATAL] ${label} 归属=${origin.where}${origin.name === undefined ? '' : ` ${origin.name}`}（核心面）— 立即退出`)
      log('error', describe(reason))
      exit(1)
      return
    }
    if (breaker.has(origin.name)) return // 已熔断：吞掉该插件后续错误
    const { count, tripped } = breaker.record(origin.name)
    log('warn', `[cos][guard] ${label} 来自非核心插件 ${origin.name}（${count}/${limit}）`)
    log('warn', describe(reason))
    if (tripped) {
      const disposed = disposePlugin(deps.ctx, origin.name)
      log('error', `[cos][guard] 熔断 ${origin.name}：${Math.round(windowMs / 1000)}s 内 ${limit} 次未捕获错误，`
        + `${disposed ? '已停用其插件 fiber' : '本次会话忽略其后续错误'}（重启恢复；持久禁用请在 cordis.patch.yml 设 disabled: true）`)
    }
  }
}

/**
 * 安装进程级守卫：unhandledRejection / uncaughtException 统一走分级兜底。
 * 返回卸载函数（测试用）。
 */
export function installErrorGuard(
  deps: GuardDeps & { target?: EventTargetLike } = {},
): () => void {
  const target = deps.target ?? (process as unknown as EventTargetLike)
  const handle = createGuardHandler(deps)
  const onReject = (reason: unknown): void => { handle(reason, 'unhandledRejection') }
  const onException = (error: unknown): void => { handle(error, 'uncaughtException') }
  target.on('unhandledRejection', onReject)
  target.on('uncaughtException', onException)
  return () => {
    target.removeListener('unhandledRejection', onReject)
    target.removeListener('uncaughtException', onException)
  }
}
