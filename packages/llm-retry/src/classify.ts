/**
 * @cos/llm-retry/classify — 请求失败分类（纯函数）：一次 agent 请求失败是否
 * 「瞬时」、值得重试。认网络层信号：undici 的 fetch failed 及其 cause 链
 * 里的 socket/DNS errno、断流消息（terminated / socket hang up）、HTTP
 * 408/429/5xx，以及网关把上游服务端故障包成 4xx 时响应体里的 `server_error`
 * 标记。其余（配置/鉴权类 4xx、NO_ADAPTER/NO_MODEL 路由错、
 * INVALID_STREAM 协议错、AbortError 取消）一律终局——重试只是白等。
 * @module @cos/llm-retry/classify
 */

/** Node/undici 网络层 errno（cause.code）：连接被重置/拒绝/超时、DNS 抖动等。 */
const TRANSIENT_SOCKET_CODES = new Set([
  'ECONNRESET', 'ECONNABORTED', 'ECONNREFUSED', 'EPIPE',
  'ETIMEDOUT', 'ESOCKETTIMEDOUT', 'EHOSTUNREACH', 'EHOSTDOWN',
  'ENETUNREACH', 'ENETDOWN', 'ENETRESET',
  'EAI_AGAIN', 'ENOTFOUND',
  'UND_ERR_CONNECT_TIMEOUT', 'UND_ERR_SOCKET', 'UND_ERR_HEADERS_TIMEOUT', 'UND_ERR_BODY_TIMEOUT',
])

/** 断流/半开连接的消息片段（undici 与 node:http 措辞不一，只能按消息认）。 */
const TRANSIENT_MESSAGE_FRAGMENTS = ['terminated', 'socket hang up', 'other side closed']

/**
 * 网关把上游服务端故障包成 4xx 时响应体里的标记（OpenAI 兼容错误载荷）。
 * 状态码落在客户端错误区间，语义却是服务端故障，仍值得重试。
 */
const SERVER_ERROR_BODY = /"type"\s*:\s*"server_error"|"code"\s*:\s*"server_error"/

/** HTTP 可重试状态码：请求超时、限流、网关/服务端故障。 */
function isRetryableStatus(status: number): boolean {
  return status === 408 || status === 429 || (status >= 500 && status <= 599)
}

/**
 * 适配器错误消息里的 HTTP 状态码。各家适配器统一成 `… request failed:
 * <status> …` / `… stream error: <status> …` 两种措辞，按这两种取。
 */
function httpStatusOf(message: string): number | undefined {
  const match = /(?:failed|error): (\d{3})\b/.exec(message)
  return match === null ? undefined : Number(match[1])
}

interface FailureNode {
  name?: unknown
  message?: unknown
  code?: unknown
  cause?: unknown
  errors?: unknown
}

/** 深度优先走 cause 链（含 AggregateError.errors，happy-eyeballs 会打包多地址）。 */
function collectNodes(error: unknown, out: FailureNode[], depth = 0): void {
  if (typeof error !== 'object' || error === null || depth > 4) return
  const node = error as FailureNode
  out.push(node)
  collectNodes(node.cause, out, depth + 1)
  if (Array.isArray(node.errors)) {
    for (const inner of node.errors) collectNodes(inner, out, depth + 1)
  }
}

/**
 * 失败是否瞬时、值得重试。判定顺序即优先级：
 * 取消（AbortError 可能藏在 fetch failed 的 cause 里）永不重试；
 * cause 链上的网络 errno 即瞬时；`code: PROVIDER_ERROR` 的域错误按状态码与
 * 响应体语义判（鸭子类型，不依赖 instanceof：同一引擎可能同时装着多份
 * @cos 模块副本，跨副本的类身份不相等）；最后看消息特征。
 */
export function isTransientFailure(error: unknown): boolean {
  if (typeof error !== 'object' || error === null) return false
  const nodes: FailureNode[] = []
  collectNodes(error, nodes)
  if (nodes.some((node) => node.name === 'AbortError')) return false
  if (nodes.some((node) => typeof node.code === 'string' && TRANSIENT_SOCKET_CODES.has(node.code))) return true
  const message = String((error as { message?: unknown }).message ?? error)
  if ((error as { code?: unknown }).code === 'PROVIDER_ERROR') {
    const status = httpStatusOf(message)
    if (status !== undefined && isRetryableStatus(status)) return true
    return SERVER_ERROR_BODY.test(message)
  }
  if (typeof (error as { code?: unknown }).code === 'string') return false
  if (TRANSIENT_MESSAGE_FRAGMENTS.some((fragment) => message.includes(fragment))) return true
  return error instanceof TypeError && message === 'fetch failed'
}

/** 日志用的失败摘要：消息 + cause 链上的 errno（fetch failed 单看没信息量）。 */
export function describeFailure(error: unknown): string {
  const base = error instanceof Error ? error.message : String(error)
  const nodes: FailureNode[] = []
  collectNodes(error, nodes)
  const codes = [...new Set(nodes.map((node) => (typeof node.code === 'string' ? node.code : undefined)))]
    .filter((code): code is string => code !== undefined)
  return codes.length === 0 ? base : `${base} (${codes.join(' ← ')})`
}
