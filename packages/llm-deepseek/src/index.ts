/**
 * @cos/llm-deepseek — DeepSeek adapter registering into the @cos/llm registry:
 * serves provider "deepseek-official" with models deepseek-flash / -pro.
 * Translates the loop's history into the OpenAI-compatible chat-completions
 * streaming API and SSE deltas back into the block-protocol StreamChunk.
 * The API key resolves through the @cos/credentials seam when the adapter is
 * built (in apply); a missing key fails loud with a diagnostic naming the
 * setting to fix. stream() matches the LlmAdapter contract (options only).
 * @module @cos/llm-deepseek
 */

import type { Context } from 'cordis'
import { LlmAdapter } from '@cos/llm'
import type { LlmProviderInfo, ProviderConfigDecl } from '@cos/llm'
import type { GenerateOptions, MessageContent, ModelMessage, StreamChunk } from '@cos/types'
import { LlmError } from '@cos/types'

export const name = 'llm-deepseek'
export const inject = ['llm', 'credentials']

const PROVIDER = 'deepseek-official'
const API_KEY_REF = 'deepseek.apiKey'

/** DeepSeek adapter options — content plus how the API key is sourced. */
export interface DeepSeekConfig {
  baseUrl?: string
  defaultModel?: string
  /** Exact models this adapter serves under the deepseek-official provider. */
  models?: readonly string[]
  /** Dot-path of the API key in the credentials secrets file (default deepseek.apiKey). */
  apiKeyKey?: string
  /** Fallback environment variable holding the API key, when the file source is absent. */
  apiKeyEnv?: string
}

interface ChatMessage {
  role: 'system' | 'user' | 'assistant' | 'tool'
  content: string | Array<{ type: 'text'; text: string } | { type: 'image_url'; image_url: { url: string } }>
  tool_call_id?: string
  tool_calls?: Array<{ id: string; type: 'function'; function: { name: string; arguments: string } }>
}

function contentToText(content: MessageContent): string {
  return content
    .filter((block) => block.type === 'text')
    .map((block) => block.text)
    .join('')
}

/** 用户消息含图片时走 OpenAI 多模态 parts；否则纯文本。 */
function userWireContent(content: MessageContent): ChatMessage['content'] {
  const text = contentToText(content)
  const images = content.filter((block) => block.type === 'image')
  if (images.length === 0) return text
  const parts: Array<{ type: 'text'; text: string } | { type: 'image_url'; image_url: { url: string } }> =
    images.map((img) => ({
      type: 'image_url',
      image_url: { url: `data:${img.mime};base64,${img.data}` },
    }))
  parts.push({ type: 'text', text: text === '' ? '（图片）' : text })
  return parts
}

/** Translate our model-visible history into the wire format, with any system prompt first.
 *  工具结果带图时：tool 行只留文本摘要，图片以紧随其后的 user 消息注入
 *  （OpenAI 兼容 tool 角色通常只收文本；user 的 image_url 视觉模型才能「看见」）。 */
function translate(messages: readonly ModelMessage[], system?: string): ChatMessage[] {
  const wire: ChatMessage[] = system === undefined ? [] : [{ role: 'system', content: system }]
  for (const message of messages) {
    const text = contentToText(message.content)
    if (message.role === 'user') {
      wire.push({ role: 'user', content: userWireContent(message.content) })
    } else if (message.role === 'tool') {
      const images = message.content.filter((block) => block.type === 'image')
      wire.push({ role: 'tool', tool_call_id: message.callId ?? '', content: text })
      if (images.length > 0) {
        const name = images.find((i) => i.name)?.name ?? 'image'
        wire.push({
          role: 'user',
          content: [
            ...images.map((img) => ({
              type: 'image_url' as const,
              image_url: { url: `data:${img.mime};base64,${img.data}` },
            })),
            {
              type: 'text' as const,
              text: `[tool result image: ${name}] — inspect this image visually and describe what you see.`,
            },
          ],
        })
      }
    } else {
      const toolCalls = message.content
        .filter((block) => block.type === 'tool-call')
        .map((block) => ({
          id: block.id,
          type: 'function' as const,
          function: { name: block.name, arguments: block.arguments },
        }))
      wire.push({
        role: 'assistant',
        content: text,
        ...(toolCalls.length === 0 ? {} : { tool_calls: toolCalls }),
      })
    }
  }
  return wire
}

/** Parse one SSE `data:` payload; null for keep-alives and the [DONE] marker. */
function parseSseData(line: string): unknown | null {
  if (!line.startsWith('data:')) return null
  const payload = line.slice(5).trim()
  if (payload === '' || payload === '[DONE]') return null
  return JSON.parse(payload) as unknown
}

class DeepSeekLlmAdapter extends LlmAdapter {
  private readonly apiKey: string
  private readonly baseUrl: string
  private readonly models: readonly string[]
  private readonly defaultModel: string
  private readonly apiKeyRef: string
  private readonly apiKeyEnv: string | undefined

  constructor(credentials: Context['credentials'], config: DeepSeekConfig) {
    super()
    this.baseUrl = (config.baseUrl ?? 'https://api.deepseek.com').replace(/\/+$/, '')
    this.models = config.models ?? ['deepseek-flash', 'deepseek-pro']
    this.defaultModel = config.defaultModel ?? 'deepseek-flash'
    this.apiKeyRef = config.apiKeyKey ?? API_KEY_REF
    this.apiKeyEnv = config.apiKeyEnv ?? 'DEEPSEEK_API_KEY'
    // Credential resolution is deliberately NON-fail-loud at construction:
    // the adapter must be mountable before the user has configured an API key
    // (packaged first-run). An unresolved key yields '' here; the backend's
    // isConfigured() guard blocks chat until it is set, and stream() surfaces
    // a provider error if a request somehow goes out keyless.
    let key = ''
    try {
      key = credentials.get(this.apiKeyRef)
    } catch {
      key = process.env[this.apiKeyEnv ?? ''] ?? ''
    }
    this.apiKey = key
  }

  providerInfo(provider: string): LlmProviderInfo {
    return { id: provider, name: 'DeepSeek' }
  }

  /** Adapter-owned configuration surface: the settings panel renders this
   * API-key field (and checks `configured` through the credentials seam). */
  providerConfig(provider: string): ProviderConfigDecl {
    return {
      provider,
      name: 'DeepSeek（官方）',
      description: 'DeepSeek 官方 API（deepseek-flash / deepseek-pro）。',
      fields: [
        {
          key: 'apiKey',
          label: 'API Key',
          type: 'password',
          secret: true,
          store: 'credentials',
          credentialRef: this.apiKeyRef,
          envKey: this.apiKeyEnv,
          required: true,
          placeholder: 'sk-…',
          hint: `存于 secrets 文件（${this.apiKeyRef}）${this.apiKeyEnv === undefined ? '' : `，环境变量 ${this.apiKeyEnv} 兜底`}`,
        },
        {
          key: 'baseUrl',
          label: 'API Base URL',
          type: 'text',
          store: 'settings',
          required: false,
          placeholder: 'https://api.deepseek.com',
          hint: '留空用官方端点；自定义（如中转/代理）时填写，保存后热生效（无需重启）。清空并保存即恢复默认。',
        },
      ],
    }
  }

  async listModels(provider: string): Promise<readonly string[]> {
    if (provider !== PROVIDER) return []
    return this.models
  }

  async *stream(request: GenerateOptions): AsyncGenerator<StreamChunk> {
    const model = request.model === '' ? this.defaultModel : request.model
    // 运行时 baseUrl：优先 settings UI 写入的 <provider>.baseUrl（热生效），否则构造期默认。
    const baseUrl = (this.settingsValue(PROVIDER, 'baseUrl') ?? this.baseUrl).replace(/\/+$/, '')
    const response = await fetch(`${baseUrl}/chat/completions`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: `Bearer ${this.apiKey}`,
      },
      body: JSON.stringify({
        model,
        messages: translate(request.messages, request.system),
        ...(request.tools !== undefined && request.tools.length > 0 ? { tools: request.tools } : {}),
        // 推理模型 completion_tokens 含 CoT；缺省 max_tokens 会被网关砍半截正文。
        max_tokens: request.maxTokens ?? 384000,
        stream: true,
      }),
      signal: request.signal,
    })
    if (!response.ok || response.body === null) {
      throw new LlmError('PROVIDER_ERROR', `deepseek request failed: ${response.status} ${await response.text()}`)
    }
    const decoder = new TextDecoder()
    const reader = response.body.getReader()
    let buffer = ''
    let textOpened = false
    let rawFinish: string | undefined
    const toolBlocks = new Map<number, { id: string; name: string; arguments: string }>()
    try {
      /** 解析一行 SSE data 并产出 StreamChunk（供主循环与流末残留 buffer 共用）。 */
      const consume = function* (line: string): Generator<StreamChunk> {
        const data = parseSseData(line)
        if (data === null || typeof data !== 'object' || !('choices' in data)) return
        const choice = (data as { choices: Array<{ delta?: unknown; finish_reason?: string | null }> }).choices[0]
        if (choice === undefined) return
        for (const c of (data as { choices: Array<{ finish_reason?: string | null }> }).choices) {
          if (typeof c.finish_reason === 'string' && c.finish_reason !== '') {
            rawFinish = c.finish_reason
          }
        }
        if (typeof choice.delta !== 'object' || choice.delta === null) return
        const delta = choice.delta as {
          content?: string
          reasoning_content?: string
          reasoning?: string
          tool_calls?: Array<{
            index?: number
            id?: string
            function?: { name?: string; arguments?: string }
          }>
        }
        const thinking =
          typeof delta.reasoning_content === 'string' && delta.reasoning_content !== ''
            ? delta.reasoning_content
            : typeof delta.reasoning === 'string' && delta.reasoning !== ''
              ? delta.reasoning
              : ''
        if (thinking !== '') {
          yield { type: 'thinking-delta', text: thinking }
        }
        if (typeof delta.content === 'string' && delta.content !== '') {
          if (!textOpened) {
            yield { type: 'block-start', index: 0, blockType: 'text' }
            textOpened = true
          }
          yield { type: 'text-delta', index: 0, text: delta.content }
        }
        for (const call of delta.tool_calls ?? []) {
          const blockIndex = (call.index ?? 0) + 1
          const block = toolBlocks.get(blockIndex) ?? { id: '', name: '', arguments: '' }
          const firstForBlock = !toolBlocks.has(blockIndex)
          block.id += call.id ?? ''
          block.name += call.function?.name ?? ''
          block.arguments += call.function?.arguments ?? ''
          toolBlocks.set(blockIndex, block)
          if (firstForBlock) yield { type: 'block-start', index: blockIndex, blockType: 'tool-call' }
          yield {
            type: 'tool-call-delta',
            index: blockIndex,
            id: call.id ?? '',
            name: call.function?.name ?? '',
            argumentsDelta: call.function?.arguments ?? '',
          }
        }
      }
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        buffer += decoder.decode(value, { stream: true })
        const lines = buffer.split('\n')
        buffer = lines.pop() ?? ''
        for (const line of lines) {
          yield* consume(line.trim())
        }
      }
      // 流末尾：flush 解码器 + 处理残留 buffer（最后一行 SSE 往往没有尾换行，不处理会丢掉正文尾巴）。
      buffer += decoder.decode()
      for (const line of buffer.split('\n')) {
        const trimmed = line.trim()
        if (trimmed !== '') yield* consume(trimmed)
      }
      buffer = ''
      if (textOpened) yield { type: 'block-end', index: 0, block: { type: 'text', text: '' } }
      for (const index of [...toolBlocks.keys()].sort((a, b) => a - b)) {
        const block = toolBlocks.get(index)
        yield { type: 'block-end', index, block: { type: 'tool-call', id: block?.id ?? '', name: block?.name ?? '', arguments: block?.arguments ?? '' } }
      }
      // 契约：finish 必须是最后一个 chunk（与 mock-llm / commandcode 一致）。
      // 映射上游 finish_reason：length/max_tokens → max-tokens，避免半截回复被当成正常结束。
      const reason =
        toolBlocks.size > 0
          ? ({ kind: 'tool-calls' } as const)
          : rawFinish === 'length' || rawFinish === 'max_tokens'
            ? ({ kind: 'max-tokens' } as const)
            : ({ kind: 'stop' } as const)
      yield { type: 'finish', reason }
    } finally {
      reader.releaseLock()
    }
  }
}

export function apply(ctx: Context, config: DeepSeekConfig = {}) {
  // Register the credential source, then resolve it by building the adapter.
  // The key comes from the @cos/credentials secrets file (key deepseek.apiKey by
  // default), with an environment fallback (DEEPSEEK_API_KEY by default); a
  // missing key fails the load with a clear diagnostic.
  const envKey = config.apiKeyEnv ?? 'DEEPSEEK_API_KEY'
  ctx.credentials.provide(API_KEY_REF, {
    key: config.apiKeyKey ?? 'deepseek.apiKey',
    envKey,
  }, {
    ref: 'DeepSeek API key (credentials.config.file → deepseek.apiKey, or env DEEPSEEK_API_KEY)',
  })
  ctx.llm.registerAdapter([PROVIDER], new DeepSeekLlmAdapter(ctx.credentials, config))
}