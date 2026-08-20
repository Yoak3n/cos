/**
 * @cos/llm — adapter registry (`ctx.llm`), aligned to dsh-llm's LlmRuntime and
 * the llm-adapter guide: providers register an `LlmAdapter` under route names,
 * agents declare provider/model routes in their options, prepareCall resolves
 * the route (advisory model catalog, never a rejection), and stream assembles
 * the adapter's block-protocol chunks through a BlockAssembler inside the
 * `llm/stream` waterfall.
 * @module @cos/llm
 */

import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { GenerateOptions, LlmCallConfig, ModelBlock, StreamChunk } from '@cos/types'
import { LlmError } from '@cos/types'

declare module 'cordis' {
  interface Context {
    llm: LlmRuntime
  }
  interface Events {
    /**
     * Waterfall around every streaming model call: call `next()` to reach the
     * resolved adapter's stream, or yield your own chunks to short-circuit.
     * @param options - the fully assembled request; `options.provider` selects
     *   the adapter.
     * @mode waterfall
     */
    'llm/stream'(options: GenerateOptions, next: () => AsyncIterable<StreamChunk>): AsyncIterable<StreamChunk>
  }
}

/** One provider route's display metadata. */
export interface LlmProviderInfo {
  id: string
  name: string
}

/** Exact-model metadata an adapter may resolve, parallel to dsh-llm. */
export interface ResolvedModelInfo {
  provider: string
  id: string
  name: string
}

/**
 * Provider-wire adapter. Register implementations with
 * `ctx.llm.registerAdapter(providers, adapter)`. Yields the block-protocol
 * StreamChunk stream; every block opens and closes, `usage` precedes `finish`,
 * which is always the final chunk.
 */
export abstract class LlmAdapter {
  /** Display metadata for a provider route this adapter owns. */
  providerInfo(provider: string): LlmProviderInfo {
    return { id: provider, name: provider }
  }

  /** Models this adapter advertises for one owned provider, advisory only. */
  async listModels(_provider: string): Promise<readonly string[]> {
    return []
  }

  /** Resolve exact-model identity (no routing/validation side effects). */
  resolveModel(provider: string, model: string, _signal?: AbortSignal): Promise<ResolvedModelInfo> {
    return Promise.resolve({ provider, id: model, name: model })
  }

  /** Stream one model call as raw chunks; must honor `options.signal`. */
  abstract stream(options: GenerateOptions): AsyncIterable<StreamChunk>
}

interface Registration {
  adapter: LlmAdapter
  provider: LlmProviderInfo
}

/**
 * Awaited adapter-stream assembler: validates block-start/block-end pairing,
 * concatenates deltas per open block, and exposes the finished {@link ModelBlock}s.
 */
export class BlockAssembler {
  private readonly blockMap = new Map<number, ModelBlock>()
  private readonly open = new Set<number>()

  /** Feed one chunk, updating the open/closed block state. */
  push(chunk: StreamChunk): void {
    if (chunk.type === 'block-start') {
      this.open.add(chunk.index)
      this.blockMap.set(chunk.index, chunk.blockType === 'text'
        ? { type: 'text', text: '' }
        : { type: 'tool-call', id: '', name: '', arguments: '' })
    } else if (chunk.type === 'text-delta') {
      if (!this.open.has(chunk.index)) throw new LlmError(`text-delta without open block ${chunk.index}`, 'INVALID_STREAM')
      const block = this.blockMap.get(chunk.index)
      if (block !== undefined && block.type === 'text') block.text += chunk.text
    } else if (chunk.type === 'tool-call-delta') {
      if (!this.open.has(chunk.index)) throw new LlmError(`tool-call-delta without open block ${chunk.index}`, 'INVALID_STREAM')
      const block = this.blockMap.get(chunk.index)
      if (block !== undefined && block.type === 'tool-call') {
        if (block.id === '') block.id = chunk.id
        if (block.name === '') block.name = chunk.name
        block.arguments += chunk.argumentsDelta
      }
    } else if (chunk.type === 'block-end') {
      if (!this.open.delete(chunk.index)) throw new LlmError(`block-end without open block ${chunk.index}`, 'INVALID_STREAM')
      const existing = this.blockMap.get(chunk.index)
      if (chunk.block.type === 'text' && existing !== undefined) {
        // Text content is carried by deltas; the finalization payload for a
        // text block carries no text of its own, so keep the accumulated text.
        this.blockMap.set(chunk.index, existing)
      } else {
        this.blockMap.set(chunk.index, chunk.block)
      }
    }
    // usage and finish have no block state.
  }

  /** Whether any block is currently open (mid-stream). */
  get hasOpenBlocks(): boolean {
    return this.open.size > 0
  }

  /** The assembled blocks in ascending index order, frozen. */
  get blocks(): readonly ModelBlock[] {
    return [...this.blockMap.entries()].sort((a, b) => a[0] - b[0]).map(([, block]) => block)
  }
}

/**
 * The abstract `llm` service: an adapter registry plus a streaming model-call
 * API, interceptable via the `llm/stream` waterfall.
 */
export class LlmRuntime extends Service {
  private readonly adapters = new Map<string, Registration>()

  constructor(ctx: Context) {
    super(ctx, 'llm')
  }

  /**
   * Register an adapter for the given provider routes. Throws `LlmError` with
   * code `DUPLICATE_ADAPTER` if any provider already has an adapter
   * (all-or-nothing). Disposed with the fiber.
   * @param providers - every provider route this adapter should serve.
   * @param adapter - the adapter that streams calls for those providers.
   * @returns the disposer releasing every route this registration holds.
   */
  registerAdapter(providers: readonly string[], adapter: LlmAdapter): () => void {
    const owned = new Set<string>()
    const commit = (): void => {
      if (providers.length === 0) throw new LlmError('an adapter must register at least one provider', 'INVALID_ADAPTER')
      for (const provider of providers) {
        if (provider.length === 0) throw new LlmError('adapter provider names must be non-empty', 'INVALID_ADAPTER')
        if (this.adapters.has(provider) && !owned.has(provider)) {
          throw new LlmError(`an adapter for provider "${provider}" is already registered`, 'DUPLICATE_ADAPTER')
        }
      }
      for (const provider of providers) {
        const info = adapter.providerInfo(provider)
        if (info.id !== provider || info.name.length === 0) {
          throw new LlmError(`adapter metadata for provider "${provider}" must preserve its id`, 'INVALID_ADAPTER')
        }
        this.adapters.set(provider, { adapter, provider: { ...info } })
        owned.add(provider)
      }
    }
    const dispose = this.ctx.effect(() => {
      commit()
      return () => {
        for (const provider of owned) this.adapters.delete(provider)
        owned.clear()
      }
    }, 'llm.registerAdapter()')
    return () => void dispose()
  }

  /** Provider routes with a registered adapter, in registration order. */
  listProviders(): LlmProviderInfo[] {
    return [...this.adapters.values()].map(({ provider }) => ({ ...provider }))
  }

  /** Models one registered provider advertises, advisory only. */
  async listModels(provider: string): Promise<readonly string[]> {
    return this.registration(provider).adapter.listModels(provider)
  }

  /**
   * Resolve one requested route: validate that an adapter serves the provider
   * and resolve exact-model metadata. The advertised model catalog is
   * advisory — an adapter may accept models it does not list, so a non-empty
   * model id is passed through without rejection. A blank model is a
   * configuration error: every production agent must name the model it wants
   * (fail loud rather than guess).
   */
  async prepareCall(config: LlmCallConfig, signal?: AbortSignal): Promise<LlmCallConfig> {
    const registration = this.registration(config.provider)
    if (config.model === '') {
      throw new LlmError(
        `provider "${config.provider}" requires an explicit model; set AgentOptions.model (advertised: ${(await this.listModels(config.provider)).join(', ')})`,
        'NO_MODEL',
      )
    }
    await registration.adapter.resolveModel(config.provider, config.model, signal)
    return { ...config, provider: registration.provider.id }
  }

  /** Stream one model call: assemble block chunks, wrapped by `llm/stream`. */
  stream(request: GenerateOptions): AsyncIterable<StreamChunk> {
    return this.ctx.waterfall(
      this,
      'llm/stream',
      request,
      () => this.adapterStream(request),
    )
  }

  private registration(provider: string): Registration {
    const registration = this.adapters.get(provider)
    if (registration === undefined) {
      const available = [...this.adapters.keys()].join(', ') || 'none'
      throw new LlmError(`no adapter registered for provider "${provider}" (registered: ${available})`, 'NO_ADAPTER')
    }
    return registration
  }

  /** Dispatch through the adapter; adapter failures throw to the caller. */
  private async *adapterStream(request: GenerateOptions): AsyncGenerator<StreamChunk> {
    const { adapter } = this.registration(request.provider)
    yield* adapter.stream(request)
  }
}

export default LlmRuntime