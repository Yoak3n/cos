/**
 * @cos/agent-loop — agent-loop driver (`ctx.agentLoop`): the turn/step state
 * machine. A turn is zero or more steps; a step is one model request plus the
 * tools it requested. Every decision point is a documented Cordis event,
 * dispatched through the agent's scope carrier so agent-scoped listeners only
 * see their own agent:
 *   agent/pre-step      (waterfall) — enter or reject the proposed step
 *   agent/request       (waterfall) — replace the request configuration
 *   agent/request-error (waterfall) — retry or close a failed request
 *   agent/turn-stopping (serial)    — last look before the turn closes
 * @module @cos/agent-loop
 */

import { randomUUID } from 'node:crypto'
import { Service } from 'cordis'
import type { Context } from 'cordis'
import { BlockAssembler } from '@cos/llm'
import { renderPrompt } from '@cos/system-prompt'
import type {
  Agent,
  AgentCancelCause,
  AgentHandle,
  AgentOptions,
  AgentStatus,
  AssistantMessage,
  FinishReason,
  GenerateOptions,
  InboxTarget,
  LlmCallConfig,
  PreStepDecision,
  RequestErrorAction,
  Session,
  SessionEvent,
  SessionId,
  TurnEndReason,
  UserMessage,
  WireTool,
} from '@cos/types'
import { LlmError } from '@cos/types'
import { SessionId as brandSessionId } from '@cos/types'
import { createScope, scopeTarget } from '@cos/scope'
import type { Scope, Scoped } from '@cos/scope'

declare module 'cordis' {
  interface Context {
    agentLoop: AgentLoop
  }
}

type Phase =
  | { kind: 'idle'; lastTurn: number }
  | { kind: 'running'; abort: AbortController; turn: number; step: number }

type RunningPhase = Extract<Phase, { kind: 'running' }>

type StepEndReason = Extract<TurnEndReason, { kind: 'completed' | 'max-tokens' }>

/** Safety net: a turn that keeps proposing steps past this never converges. */
const DEFAULT_MAX_STEPS = 200

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void; reject: (reason: unknown) => void } {
  let resolve!: (value: T) => void
  let reject!: (reason: unknown) => void
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej })
  return { promise, resolve, reject }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

function parseArguments(raw: string): unknown {
  try {
    return raw === '' ? {} : JSON.parse(raw)
  } catch {
    return raw
  }
}

/** One agent: scope + inbox + phase machine + the durable log it drives. */
export class LoopAgent implements Agent {
  readonly options: AgentOptions
  readonly session: Session
  /** The agent's registration context: dispatches route through its carrier. */
  readonly ctx: Context
  readonly carrier: Scoped<LoopAgent>
  private readonly scope: Scope
  private phase: Phase
  private activityDone: Promise<void> = Promise.resolve()
  private readonly inbox: Record<InboxTarget, UserMessage[]> = { 'next-turn': [], 'next-step': [] }
  private readonly maxSteps: number
  private readonly debugPrompt: boolean

  constructor(
    loopCtx: Context,
    public readonly id: SessionId,
    options: AgentOptions,
    session: Session,
    maxSteps: number = DEFAULT_MAX_STEPS,
    debugPrompt = false,
  ) {
    this.scope = createScope(loopCtx, this)
    this.ctx = this.scope.ctx.extend({ agent: this })
    this.carrier = scopeTarget(this, this)
    this.options = { ...options }
    this.session = session
    this.maxSteps = maxSteps
    this.debugPrompt = debugPrompt
    const lastTurn = session.events
      .filter((event): event is Extract<SessionEvent, { type: 'turn/start' }> => event.type === 'turn/start')
      .at(-1)?.data.turn ?? 0
    this.phase = { kind: 'idle', lastTurn }
  }

  get status(): AgentStatus {
    return this.phase.kind === 'idle' ? 'idle' : 'running'
  }

  private get nextTurn(): UserMessage[] {
    return this.inbox['next-turn']
  }

  private get nextStep(): UserMessage[] {
    return this.inbox['next-step']
  }

  private get hasPending(): boolean {
    return this.nextTurn.length > 0 || this.nextStep.length > 0
  }

  /** Scope-routed notification (emit): every agent listener sees its own agent. */
  private emitAgent(name: string, payload: Record<string, unknown>): void {
    ;(this.ctx.emit as (thisArg: unknown, event: string, arg: unknown) => void)(this.carrier, name, payload)
  }

  /** Scope-routed around-middleware (waterfall). */
  private async waterfallAgent<T>(name: string, payload: Record<string, unknown>, next: () => Promise<T>): Promise<T> {
    return (this.ctx.waterfall as (
      thisArg: unknown,
      event: string,
      arg: unknown,
      nxt: () => Promise<T>,
    ) => Promise<T>)(this.carrier, name, payload, next)
  }

  /** Scope-routed in-order dispatch (serial). */
  private serialAgent(name: string, payload: Record<string, unknown>): Promise<unknown> {
    return (this.ctx.serial as (thisArg: unknown, event: string, arg: unknown) => Promise<unknown>)(
      this.carrier,
      name,
      payload,
    )
  }

  /** Commit a phase and publish the externally visible status transition. */
  private setPhase(next: Phase): void {
    const previousStatus = this.status
    this.phase = next
    if (this.status !== previousStatus) {
      this.emitAgent('agent/status', { agent: this, status: this.status })
    }
  }

  send(message: UserMessage, target: InboxTarget, wakeup: boolean): void {
    this.splice(target, this.inbox[target].length, 0, [message])
    if (wakeup) this.wakeDriver()
  }

  followup(input: UserMessage): void {
    this.send(input, 'next-turn', true)
  }

  steer(input: UserMessage): void {
    this.send(input, 'next-step', true)
  }

  inject(input: UserMessage): void {
    this.send(input, 'next-step', false)
  }

  cancel(cause: AgentCancelCause, options: { keepInbox?: boolean } = {}): void {
    if (!options.keepInbox) this.clearInbox()
    if (this.phase.kind === 'running') this.phase.abort.abort(cause)
  }

  async whenIdle(): Promise<void> {
    let activity: Promise<void>
    do {
      await (activity = this.activityDone)
    } while (activity !== this.activityDone)
  }

  async dispose(): Promise<void> {
    this.cancel({ kind: 'disposed' })
    await this.whenIdle()
    await this.scope.dispose()
  }

  private clearInbox(): void {
    for (const target of ['next-step', 'next-turn'] as const) {
      this.splice(target, 0, this.inbox[target].length, [], true)
    }
  }

  /** Commit one inbox mutation and publish its live notifications. */
  private splice(
    target: InboxTarget,
    start: number,
    deleteCount: number,
    inserted: UserMessage[],
    discard = true,
  ): UserMessage[] {
    const removed = this.inbox[target].splice(start, deleteCount, ...inserted)
    if (discard) {
      for (const message of removed) {
        this.emitAgent('agent/inbox/discarded', { agent: this, message })
      }
    }
    for (const message of inserted) {
      this.emitAgent('agent/inbox/inserted', { agent: this, message })
    }
    return removed
  }

  /** Remove and return the complete batch proposed for one step. */
  private claim(target: InboxTarget, turn: number): UserMessage[] {
    const claimed = this.splice('next-step', 0, this.inbox['next-step'].length, [], false)
    if (target === 'next-turn') claimed.push(...this.splice('next-turn', 0, 1, [], false))
    for (const message of claimed) {
      this.emitAgent('agent/inbox/claimed', { agent: this, message, turn })
    }
    return claimed
  }

  /** Start one driver; work arriving while running is claimed by the live driver. */
  private wakeDriver(): void {
    if (this.phase.kind !== 'idle') return
    const done = deferred<void>()
    this.activityDone = done.promise
    this.setPhase({ kind: 'running', abort: new AbortController(), turn: this.phase.lastTurn, step: 0 })
    void this.kick().then(done.resolve, done.reject)
  }

  private async kick(): Promise<void> {
    try {
      while (await this.turn()) {
        // next turn
      }
    } catch {
      // contained at the driver boundary; failures were reported live
    } finally {
      if (this.phase.kind === 'running') {
        const { turn } = this.phase
        this.setPhase({ kind: 'idle', lastTurn: turn })
      }
    }
  }

  /** Open one turn and drive its steps until nothing is owed. */
  private async turn(): Promise<boolean> {
    if (this.phase.kind !== 'running') throw new Error('turn without driver reservation')
    const phase = this.phase
    const { signal } = phase.abort
    signal.throwIfAborted()
    const turn = phase.turn + 1
    this.session.append('turn/start', { turn })
    phase.turn = turn
    let reason: TurnEndReason | null = null
    let target: InboxTarget = 'next-turn'
    try {
      while (true) {
        signal.throwIfAborted()
        const step = phase.step + 1
        const decision = await this.preStep(target, { turn, step })
        if (decision.kind === 'reject') {
          reason = { kind: 'blocked' }
          break
        }
        if (reason !== null && decision.messages.length === 0) break
        // A removed waking message still owns the turn boundary but spends no
        // model call.
        if (phase.step === 0 && decision.messages.length === 0) {
          reason = { kind: 'completed' }
          break
        }
        signal.throwIfAborted()
        if (phase.step >= this.maxSteps) {
          throw new Error(`agent "${this.id}" exceeded the step limit (${this.maxSteps})`)
        }
        this.session.append('step/start', { turn, step })
        phase.step = step
        try {
          for (const message of decision.messages) {
            this.session.append('user/message', message)
          }
          const stepEnd = await this.step(phase, step)
          if (reason === null) reason = stepEnd
        } finally {
          this.session.append('step/end', { turn, step })
        }
        signal.throwIfAborted()
        if (reason !== null && this.nextStep.length === 0) {
          await this.serialAgent('agent/turn-stopping', { agent: this, turn, signal })
          signal.throwIfAborted()
        }
        if (reason !== null && this.nextStep.length === 0) break
        target = 'next-step'
      }
    } catch (error: unknown) {
      if (signal.aborted) {
        reason = { kind: 'aborted', reason: signal.reason as AgentCancelCause }
      } else {
        reason = { kind: 'error', error: { message: errorMessage(error), code: 'UNKNOWN' } }
        this.emitAgent('agent/error', {
          agent: this,
          turn,
          step: this.phase.kind === 'running' ? this.phase.step : 0,
          error,
        })
      }
    } finally {
      this.session.append('turn/end', {
        turn,
        reason: reason ?? { kind: 'error', error: { message: 'missing turn end reason', code: 'UNKNOWN' } },
      })
    }
    // Durability checkpoint: the completed turn reaches storage before the next
    // turn starts (persistence plugins join the parallel `session/flush`).
    await this.ctx.sessions.flush(this.session)
    if (!this.hasPending) return false
    // A fresh controller makes the turn's cancellation window per-turn.
    phase.abort = new AbortController()
    phase.step = 0
    return true
  }

  /** Claim input, then let `agent/pre-step` decide what the model sees. */
  private async preStep(target: InboxTarget, position: { turn: number; step: number }): Promise<PreStepDecision> {
    if (this.phase.kind !== 'running') throw new Error('pre-step outside running phase')
    const { signal } = this.phase.abort
    const claimed = this.claim(target, position.turn)
    return await this.waterfallAgent<PreStepDecision>(
      'agent/pre-step',
      { agent: this, messages: claimed, ...position, signal },
      async () => ({ kind: 'enter', messages: claimed }),
    )
  }

  /** One step: request, route through the llm seam, stream, assemble, run tools. */
  private async step(phase: RunningPhase, step: number): Promise<StepEndReason | null> {
    const { turn, abort: { signal } } = phase
    while (true) {
      const config = await this.waterfallAgent<LlmCallConfig>(
        'agent/request',
        { agent: this, turn, step, signal },
        async () => ({
          provider: this.options.provider ?? '',
          model: this.options.model ?? '',
          ...(this.options.maxTokens === undefined ? {} : { maxTokens: this.options.maxTokens }),
        }),
      )
      // The registry resolves the requested route; a provider with no adapter
      // (or a model it does not serve) surfaces through agent/request-error
      // with the available routes in the message (default action: terminal).
      let resolved: LlmCallConfig
      try {
        resolved = await this.ctx.llm.prepareCall(config, signal)
      } catch (error: unknown) {
        if (!(error instanceof LlmError) || error.code !== LlmError.NO_ADAPTER) throw error
        const action = await this.waterfallAgent<RequestErrorAction | undefined>(
          'agent/request-error',
          {
            agent: this,
            turn,
            step,
            provider: config.provider,
            model: config.model,
            failure: error,
            available: error.message,
            signal,
          },
          () => Promise.resolve(undefined),
        )
        if (action?.kind !== 'retry') throw error
        continue
      }
      signal.throwIfAborted()
      const assembly = await this.ctx.systemPrompt.assemble({
        agent: this,
        scope: this,
        signal,
        provider: resolved.provider,
        model: resolved.model,
      })
      const system = renderPrompt(assembly)
      const tools = assembly.tools
      if (this.debugPrompt && system !== '') console.log(`[system] ${system}`)
      const request: GenerateOptions = {
        ...resolved,
        messages: this.session.deriveMessages(),
        sessionId: this.session.id,
        signal,
        ...(system === '' ? {} : { system }),
        ...(tools.length === 0 ? {} : { tools }),
      }
      const assembler = new BlockAssembler()
      let finish: FinishReason | undefined
      try {
        for await (const chunk of this.ctx.llm.stream(request)) {
          signal.throwIfAborted()
          this.session.append('assistant/chunk', { turn, step, chunk })
          if (chunk.type === 'finish') { finish = chunk.reason; continue }
          assembler.push(chunk)
        }
      } catch (error: unknown) {
        if (signal.aborted) throw error
        const action = await this.waterfallAgent<RequestErrorAction | undefined>(
          'agent/request-error',
          {
            agent: this,
            turn,
            step,
            provider: request.provider,
            model: request.model,
            failure: error,
            available: '',
            signal,
          },
          () => Promise.resolve(undefined),
        )
        if (action?.kind !== 'retry') throw error
        continue
      }
      signal.throwIfAborted()
      const message: AssistantMessage = {
        id: randomUUID(),
        role: 'assistant',
        content: assembler.blocks.map((block) => {
          if (block.type === 'text') return { type: 'text' as const, text: block.text }
          return {
            type: 'tool-call' as const,
            id: block.id !== '' ? block.id : `call-${randomUUID().slice(0, 8)}`,
            name: block.name,
            arguments: block.arguments,
          }
        }),
      }
      this.session.append('assistant/message', { turn, step, message })
      const toolCalls = message.content.filter((block) => block.type === 'tool-call')
      if (toolCalls.length === 0) {
        // max-tokens 必须上抛：半截回复不能当成正常 completed。
        return finish?.kind === 'max-tokens' ? { kind: 'max-tokens' } : { kind: 'completed' }
      }
      if (finish?.kind === 'stop') {
        // The adapter signaled completion without tool execution (e.g. a tool
        // call emitted but the model said stop); still run the requested tools.
        void finish
      }
      for (const call of toolCalls) {
        this.session.append('tool/call', { turn, step, callId: call.id, name: call.name, arguments: call.arguments })
      }
      for (const call of toolCalls) {
        const result = await this.ctx.tools.execute(call.name, parseArguments(call.arguments), signal)
        this.session.append('tool/result', {
          turn,
          step,
          callId: call.id,
          message: {
            callId: call.id,
            content: result.content,
            isError: result.isError === true,
            ...(result.images !== undefined && result.images.length > 0 ? { images: result.images } : {}),
          },
        })
      }
      // The tool results join history, then the model is asked again in a new step.
      return null
    }
  }
}

export interface AgentLoopConfig {
  /** Safety cap on steps per turn; the mock model should never approach it. */
  maxStepsPerTurn?: number
  /** Print the rendered system prompt on every request (dev aid). */
  debugSystemPrompt?: boolean
}

export class AgentLoop extends Service {
  static inject = ['sessions', 'agents', 'tools', 'llm', 'systemPrompt']
  private readonly maxStepsPerTurn: number | undefined
  private readonly debugSystemPrompt: boolean

  constructor(ctx: Context, config: AgentLoopConfig = {}) {
    super(ctx, 'agentLoop')
    this.maxStepsPerTurn = config.maxStepsPerTurn
    this.debugSystemPrompt = config.debugSystemPrompt === true
    // Route + workspace variables any mounted section can reference.
    this.ctx.systemPrompt.variable('provider', (context) => context.provider ?? context.agent?.options.provider)
    this.ctx.systemPrompt.variable('model', (context) => context.model ?? context.agent?.options.model)
    this.ctx.systemPrompt.variable('cwd', (context) => context.agent?.session.header.cwd ?? process.cwd())
  }

  /** Create one agent over a fresh (or resumed) session, publish it, and start nothing yet. */
  async createAgent(options: {
    sessionId?: SessionId
    agentOptions?: AgentOptions
    meta?: { cwd?: string; ephemeral?: boolean }
    resume?: boolean
  } = {}): Promise<AgentHandle> {
    const id = options.sessionId ?? brandSessionId(randomUUID())
    const persistence = this.ctx.get('sessionPersistence') as
      | { prepare(id: SessionId): readonly SessionEvent[] | undefined }
      | undefined
    const seed = options.resume === true
      ? persistence?.prepare(id)
      : undefined
    const session = this.ctx.sessions.create(id, options.meta, seed)
    const agent = new LoopAgent(this.ctx, id, options.agentOptions ?? {}, session, this.maxStepsPerTurn, this.debugSystemPrompt)
    const remove = this.ctx.agents.enter(agent)
    let disposing: Promise<void> | undefined
    const dispose = (): Promise<void> => (disposing ??= (async () => {
      await agent.dispose()
      remove()
      this.ctx.sessions.remove(session)
    })())
    this.ctx.agents.announce(agent)
    ;(this.ctx.emit as (thisArg: unknown, event: string, arg: unknown) => void)(
      agent.carrier,
      'agent/session-start',
      { agent, source: options.resume === true ? 'resume' : 'startup' },
    )
    return { agent, dispose }
  }
}

export default AgentLoop