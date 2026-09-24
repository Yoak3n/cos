/**
 * @cos/mock-llm — mock llm adapter registering into the @cos/llm registry,
 * aligned to the dsh-llm adapter contract: serves provider "mock" with model
 * "mock-1" via a `LlmAdapter` subclass yielding the block-protocol
 * StreamChunk stream. Deterministic: answers with a tool call once (when the
 * prompt asks for one), then finishes once the tool result sits in history.
 * @module @cos/mock-llm
 */

import type { Context } from 'cordis'
import { LlmAdapter } from '@cos/llm'
import type { LlmProviderInfo, ProviderConfigDecl } from '@cos/llm'
import type { GenerateOptions, ModelMessage, StreamChunk } from '@cos/types'

export const name = 'mock-llm'
export const inject = ['llm']

const MOCK_PROVIDER = 'mock'

/** Whether an assistant tool-call block is still awaiting its tool/result. */
function hasPendingToolCall(messages: readonly ModelMessage[]): boolean {
  const pending: string[] = []
  for (const message of messages) {
    if (message.role === 'assistant') {
      for (const block of message.content) {
        if (block.type === 'tool-call') pending.push(block.id)
      }
    } else if (message.role === 'tool') {
      const index = pending.indexOf(message.callId ?? '')
      if (index >= 0) pending.splice(index, 1)
    }
  }
  return pending.length > 0
}

function lastUserText(messages: readonly ModelMessage[]): string | undefined {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index]
    if (message === undefined || message.role !== 'user') continue
    const text = message.content
      .filter((block) => block.type === 'text')
      .map((block) => block.text)
      .join('')
    if (text !== '') return text
  }
  return undefined
}

/** Emit a complete text block as one open/delta/close sequence. */
async function* textBlock(index: number, text: string): AsyncGenerator<StreamChunk> {
  yield { type: 'block-start', index, blockType: 'text' }
  for (const word of text.split(' ')) {
    if (word !== '') yield { type: 'text-delta', index, text: word + ' ' }
  }
  yield { type: 'block-end', index, block: { type: 'text', text } }
}

class MockLlmAdapter extends LlmAdapter {
  providerInfo(provider: string): LlmProviderInfo {
    return { id: provider, name: 'Mock provider' }
  }

  async listModels(provider: string): Promise<readonly string[]> {
    if (provider !== MOCK_PROVIDER) return []
    return ['mock-1']
  }

  /** Adapter-owned configuration surface: the mock needs no configuration. */
  providerConfig(provider: string): ProviderConfigDecl {
    return {
      provider,
      name: 'Mock provider',
      description: '本地 mock 适配器（离线 / 无 key 调试）。',
      fields: [],
    }
  }

  async *stream(request: GenerateOptions): AsyncGenerator<StreamChunk> {
    const executedTool = request.messages.some((message) => message.role === 'tool')
    if (hasPendingToolCall(request.messages)) {
      yield* textBlock(0, 'Tool result received; continuing.')
      yield { type: 'finish', reason: { kind: 'stop' } }
      return
    }
    const prompt = lastUserText(request.messages) ?? ''
    if (!executedTool && /tool/.test(prompt)) {
      const args = JSON.stringify({ prompt })
      yield { type: 'block-start', index: 1, blockType: 'tool-call' }
      yield { type: 'tool-call-delta', index: 1, id: 'call-1', name: 'echo', argumentsDelta: args }
      yield { type: 'block-end', index: 1, block: { type: 'tool-call', id: 'call-1', name: 'echo', arguments: args } }
      yield { type: 'finish', reason: { kind: 'tool-calls' } }
      return
    }
    yield* textBlock(0, `Hello from the mock model (${request.provider}/${request.model}). You said: ${prompt}`)
    yield { type: 'finish', reason: { kind: 'stop' } }
  }
}

export function apply(ctx: Context) {
  ctx.llm.registerAdapter([MOCK_PROVIDER], new MockLlmAdapter())
}