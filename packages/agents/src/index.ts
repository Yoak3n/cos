/**
 * @cos/agents — agent registry (`ctx.agents`): keeps live agents, publishes
 * `agent/created` / `agent/disposed`, and pairs removal with creation
 * (a synchronous creation failure rolls back to a disposal).
 * @module @cos/agents
 */

import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { Agent, SessionId } from '@cos/types'

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