/**
 * @cos/agents — agent registry (`ctx.agents`): keeps live agents, publishes
 * `agent/created` / `agent/disposed`, and pairs removal with creation
 * (a synchronous creation failure rolls back to a disposal).
 * @module @cos/agents
 */

import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { Agent, AgentHandle, AgentOptions, SessionEvent, SessionId } from '@cos/types'

declare module 'cordis' {
  interface Context {
    agents: AgentsRegistry
  }
}

interface Entry {
  agent: Agent
  announced: boolean
  announcing: boolean
  detachRequested: boolean
}

export class AgentsRegistry extends Service {
  static inject = ['sessions']
  private readonly store = new Map<SessionId, Entry>()

  constructor(ctx: Context) {
    super(ctx, 'agents')
  }

  /** Insert a live agent; the returned remove() pairs disposal with creation. */
  enter(agent: Agent): () => void {
    if (this.store.has(agent.id)) throw new Error(`agent "${agent.id}" is already registered`)
    const entry: Entry = { agent, announced: false, announcing: false, detachRequested: false }
    this.store.set(agent.id, entry)
    let entered = true
    return () => {
      if (!entered) return
      entered = false
      if (entry.announcing) {
        // Defer removal until creation dispatch unwinds: every listener must
        // observe the same live entry, and disposal follows creation.
        entry.detachRequested = true
        return
      }
      this.remove(entry)
    }
  }

  /** Announce an inserted agent; a synchronous listener throw vetoes publication. */
  announce(agent: Agent): void {
    const entry = this.store.get(agent.id)
    if (entry === undefined || entry.agent !== agent) throw new Error(`agent "${agent.id}" is not live`)
    if (entry.announced || entry.announcing) throw new Error(`agent "${agent.id}" was already announced`)
    entry.announcing = true
    entry.announced = true
    try {
      this.ctx.emit('agent/created', { agent })
    } finally {
      entry.announcing = false
      if (entry.detachRequested) this.remove(entry)
    }
  }

  get(id: SessionId): Agent | undefined {
    return this.store.get(id)?.agent
  }

  /**
   * DSH shape: create a live agent on a fresh session.
   * Delegates to `ctx.agentLoop.createAgent` (cos loop owns session minting).
   */
  async create(options: {
    sessionId: SessionId
    meta?: { cwd?: string; ephemeral?: boolean }
    seed?: readonly SessionEvent[]
    agentOptions?: AgentOptions
    signal?: AbortSignal
  }): Promise<AgentHandle> {
    const loop = this.agentLoop()
    return loop.createAgent({
      sessionId: options.sessionId,
      meta: options.meta,
      agentOptions: options.agentOptions,
      resume: false,
    })
  }

  /**
   * DSH shape: resume a persisted session onto a live agent.
   * `resumeSessionId` is the durable id; cos loads seed via sessionPersistence.
   */
  async resume(options: {
    resumeSessionId: SessionId
    agentOptions?: AgentOptions
    signal?: AbortSignal
  }): Promise<AgentHandle> {
    const loop = this.agentLoop()
    return loop.createAgent({
      sessionId: options.resumeSessionId,
      agentOptions: options.agentOptions,
      resume: true,
    })
  }

  /** Late-bound loop (avoids a hard inject cycle with @cos/agent-loop). */
  private agentLoop(): {
    createAgent(options: {
      sessionId?: SessionId
      agentOptions?: AgentOptions
      meta?: { cwd?: string; ephemeral?: boolean }
      resume?: boolean
    }): Promise<AgentHandle>
  } {
    const loop = (this.ctx as Context & { agentLoop?: { createAgent: unknown } }).agentLoop
    if (loop === undefined || typeof loop.createAgent !== 'function') {
      throw new Error('agents.create/resume: ctx.agentLoop is not available')
    }
    return loop as ReturnType<AgentsRegistry['agentLoop']>
  }

  /** Remove a live entry; emits `agent/disposed` only for announced agents. */
  private remove(entry: Entry): void {
    if (this.store.get(entry.agent.id) !== entry) return
    if (entry.announcing) {
      entry.detachRequested = true
      return
    }
    this.store.delete(entry.agent.id)
    if (entry.announced) this.ctx.emit('agent/disposed', { agent: entry.agent })
  }
}

export default AgentsRegistry