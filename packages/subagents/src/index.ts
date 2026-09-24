/**
 * @cos/subagents — harness-core subagent service (`ctx.subagents`): spawns a
 * short-lived child agent on the existing @cos/agent-loop (a fresh session,
 * ephemeral — never persisted), scopes its system prompt and tool surface to
 * the run, feeds it one task, waits for settlement, and returns its
 * transcript.
 *
 * The child runs the same loop/tools machinery as the main agent, so a plugin
 * can delegate a "digest" job to a worker that operates the plugin's own tools
 * directly (e.g. a memory plugin: the worker writes memory through tools instead
 * of returning JSON the plugin would parse). Rides on @cos/agent-loop rather
 * than a parallel LLM path: no second loop to maintain, same request-error /
 * step-cap / cancellation semantics.
 *
 * How it works:
 * - `run()` creates an agent via `ctx.agentLoop.createAgent` with a unique
 *   session id and `meta.ephemeral = true` (persistence skips those sessions).
 * - A `system-prompt/assemble` waterfall participant registered at mount
 *   REPLACES the global assembly for worker agents: the run's `system` text
 *   becomes the only section and `toolNames` (+ inline `tools`) select the
 *   advertised tool set. The main agent's assembly is untouched.
 * - Inline tools (run-scoped executors) are registered into `ctx.tools` for
 *   the duration of the run and unregistered when it settles, so worker-only
 *   operations never leak into the main agent's tool surface.
 * - `agent.followup(task)` starts the loop; `whenIdle()` resolves when every
 *   queued turn settled (tool loops included); the run result is projected
 *   from the session log, then the agent + session are disposed.
 *
 * Worker system prompts must not reference unknown prompt variables — the
 * assembly contains only {{provider}} / {{model}} / {{cwd}} (registered by
 * @cos/agent-loop) and renderPrompt throws on references it cannot resolve.
 * @module @cos/subagents
 */

import { randomUUID } from 'node:crypto'
import { Service } from 'cordis'
import type { Context } from 'cordis'
import type {
  Agent,
  AssembledSection,
  PromptAssembly,
  SessionId,
  TurnEndReason,
  WireTool,
} from '@cos/types'
import { SessionId as brandSessionId, createUserMessage } from '@cos/types'
import type { ToolExecutor, ToolOptions } from '@cos/tools'

declare module 'cordis' {
  interface Context {
    subagents: SubagentsService
  }
}

/** A run-scoped tool: registered into `ctx.tools` for the run, then removed. */
export interface InlineTool {
  name: string
  executor: ToolExecutor
  options?: ToolOptions
}

export interface SubagentRunOptions {
  /** One task prompt fed to the worker (a user message). */
  task: string
  /** Worker system prompt — replaces every mounted section for this agent. */
  system?: string
  /**
   * Names of already-registered tools the worker may call. When empty the
   * worker sees every tool registered at assembly time (use with care).
   */
  toolNames?: string[]
  /**
   * Tools injected for the duration of this run only. Useful for worker-only
   * operations the main agent must never see; unregistered on settle.
   */
  tools?: InlineTool[]
  /** Provider/model/maxTokens for the worker agent (pass-through). */
  provider?: string
  model?: string
  maxTokens?: number
  /** Wall-clock cap in ms; the worker is cancelled on expiry. */
  timeoutMs?: number
  /**
   * External cancellation: when the signal aborts, the worker is cancelled
   * immediately (same semantics as a parent cancel). The run still settles
   * normally — inspect `result.reason.kind === 'aborted'` / `signal.aborted`
   * to tell. Lets callers turn a background digest into a user-invisible
   * internal flow: the moment the user speaks again, abort the running worker.
   */
  signal?: AbortSignal
  /** Worker session header cwd (default process.cwd()). */
  cwd?: string
  /** Diagnostic label (logged on completion). */
  label?: string
}

export interface SubagentResult {
  /** The worker session id (for diagnostics; never persisted). */
  sessionId: SessionId
  /** Concatenated assistant text across every turn. */
  text: string
  /** Individual assistant text blocks in order. */
  transcript: string[]
  /** Tool names the worker actually called, in call order (deduplicated). */
  toolCalls: string[]
  /** The last turn's end reason, when the log captured one. */
  reason: TurnEndReason | undefined
}

export interface SubagentsConfig {
  /** Default wall-clock cap per run, ms. */
  defaultTimeoutMs?: number
}

interface ActiveRun {
  system: string
  toolNames: Set<string>
}

const DEFAULT_TIMEOUT_MS = 120_000

export class SubagentsService extends Service {
  static inject = ['agentLoop', 'tools']

  private readonly active = new Map<SessionId, ActiveRun>()
  private readonly defaultTimeoutMs: number

  constructor(ctx: Context, config: SubagentsConfig = {}) {
    super(ctx, 'subagents')
    this.defaultTimeoutMs = Number(config?.defaultTimeoutMs) || DEFAULT_TIMEOUT_MS

    // Per-assembly scoping: only the assembling worker agent's current run is
    // affected; every other caller (the main agent, plugin-internal
    // assemblies) passes through untouched.
    ctx.on('system-prompt/assemble', (assembly: PromptAssembly, context, next) => {
      const run = context.agent !== undefined ? this.active.get(context.agent.id) : undefined
      if (run === undefined) return next()
      if (run.system !== '') {
        const section: AssembledSection = { name: 'subagent:worker', text: run.system }
        assembly.sections = [section]
      } else {
        assembly.sections = []
      }
      const defs = this.ctx.tools.listDefinitions()
      const selected = run.toolNames.size === 0
        ? defs
        : defs.filter((def) => run.toolNames.has(def.name))
      assembly.tools = selected.map(({ name, schema }): WireTool => ({
        type: 'function',
        function: { name, description: schema.description, parameters: schema.parameters },
      }))
      return next()
    })
  }

  /**
   * Spawn one worker agent, run it to settlement, and return its transcript.
   * Always disposes the agent and removes run-scoped state, even on failure.
   */
  async run(options: SubagentRunOptions): Promise<SubagentResult> {
    const id = brandSessionId(`subagent-${randomUUID().slice(0, 8)}`)
    const toolNames = new Set(options.toolNames ?? [])
    for (const tool of options.tools ?? []) toolNames.add(tool.name)

    // Register run-scoped tools for the worker's lifetime only.
    const disposers = (options.tools ?? []).map((tool) =>
      this.ctx.tools.register(tool.name, tool.executor, tool.options ?? {}),
    )
    this.active.set(id, { system: options.system ?? '', toolNames })

    const timeoutMs = options.timeoutMs ?? this.defaultTimeoutMs
    let handle: Awaited<ReturnType<typeof this.ctx.agentLoop.createAgent>>
    try {
      handle = await this.ctx.agentLoop.createAgent({
        sessionId: id,
        meta: { cwd: options.cwd ?? process.cwd(), ephemeral: true },
        agentOptions: {
          ...(options.provider === undefined ? {} : { provider: options.provider }),
          ...(options.model === undefined ? {} : { model: options.model }),
          ...(options.maxTokens === undefined ? {} : { maxTokens: options.maxTokens }),
        },
      })
    } catch (error) {
      this.active.delete(id)
      for (const dispose of disposers) dispose()
      throw error
    }

    const agent = handle.agent
    const timer = setTimeout(() => {
      this.ctx.logger.warn(`[subagents] ${options.label ?? 'run'} timed out after ${timeoutMs}ms — cancelling worker ${id}`)
      agent.cancel({ kind: 'parent' })
    }, timeoutMs)
    timer.unref?.()

    // External cancellation: a caller that treats the worker as a background
    // job (e.g. memory digest) can abort it the moment the user speaks again.
    const cancelFromSignal = (): void => { agent.cancel({ kind: 'parent' }) }
    if (options.signal?.aborted === true) cancelFromSignal()
    options.signal?.addEventListener('abort', cancelFromSignal, { once: true })

    try {
      agent.followup(createUserMessage(options.task, { kind: 'plugin', detail: `subagent:${options.label ?? 'run'}` }))
      await agent.whenIdle()
      const result = projectResult(agent, id)
      this.ctx.logger.info(
        `[subagents] ${options.label ?? 'run'} done (${result.text.length} chars, tools: ${result.toolCalls.join(', ') || 'none'})`,
      )
      return result
    } finally {
      clearTimeout(timer)
      options.signal?.removeEventListener('abort', cancelFromSignal)
      this.active.delete(id)
      for (const dispose of disposers) dispose()
      await handle.dispose()
    }
  }
}

function projectResult(agent: Agent, id: SessionId): SubagentResult {
  const transcript: string[] = []
  const toolCalls: string[] = []
  let reason: TurnEndReason | undefined
  for (const event of agent.session.events) {
    if (event.type === 'assistant/message') {
      for (const block of event.data.message.content) {
        if (block.type === 'text' && block.text.trim() !== '') transcript.push(block.text)
        else if (block.type === 'tool-call') toolCalls.push(block.name)
      }
    } else if (event.type === 'turn/end') {
      reason = event.data.reason
    }
  }
  return {
    sessionId: id,
    text: transcript.join('\n'),
    transcript,
    toolCalls: [...new Set(toolCalls)],
    reason,
  }
}

export default SubagentsService