/**
 * @cos/sidecar/server — JSON-RPC sidecar process: boots the harness tree via
 * @cos/boot, then serves newline-delimited JSON-RPC over stdin/stdout so an
 * upper-layer program can drive agents without embedding this codebase.
 * stdout carries protocol lines only; userland logs are redirected to stderr.
 * @module @cos/sidecar/server
 */

import { join } from 'node:path'
import { createInterface } from 'node:readline'
import { boot, bootOptionsFromCli, parseCliArgs } from '@cos/boot'
import { SessionId, createUserMessage } from '@cos/types'
import type { Agent } from '@cos/types'
import type { SessionEvent, SessionId as SessionIdType } from '@cos/types'

// Protocol stdout must stay clean: userland console.log lands on stderr.
console.log = (...args: unknown[]) => console.error(...args)

interface Request {
  jsonrpc?: string
  id?: number | string
  method: string
  params?: Record<string, unknown>
}

class RpcError extends Error {
  readonly code: number
  readonly data?: unknown
  constructor(code: number, message: string, data?: unknown) {
    super(message)
    this.code = code
    this.data = data
  }
}

function respond(id: number | string | undefined, error: RpcError | null, result?: unknown): void {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: id ?? null, ...(error === null ? { result } : { error }) }) + '\n')
}

function requireAgent(ctx: import('cordis').Context, sessionId: string): Agent {
  const agent = ctx.agents.get(SessionId(sessionId))
  if (agent === undefined) throw new RpcError(404, `no agent with sessionId "${sessionId}"`)
  return agent
}

async function handle(ctx: import('cordis').Context, req: Request): Promise<unknown> {
  const params = req.params ?? {}
  switch (req.method) {
    case 'ping':
      return { ok: true, providers: ctx.llm.listProviders() }
    case 'system.listProviders':
      return { providers: ctx.llm.listProviders() }
    case 'system.model':
      return { model: (await ctx.llm.listModels(params.provider as string))[0] ?? '' }
    case 'agent.create':
      return { agent: (await ctx.agentLoop.createAgent({
        sessionId: params.sessionId === undefined ? undefined : SessionId(params.sessionId as string),
        agentOptions: (params.agentOptions ?? {}) as never,
        meta: params.meta as { cwd?: string } | undefined,
        resume: params.resume === true,
      })).agent.id }
    case 'agent.followup': {
      const agent = requireAgent(ctx, params.sessionId as string)
      agent.followup(createUserMessage(params.text as string, params.source as never))
      return { ok: true }
    }
    case 'agent.whenIdle': {
      const agent = requireAgent(ctx, params.sessionId as string)
      await agent.whenIdle()
      return { status: agent.status }
    }
    case 'agent.status': {
      const agent = requireAgent(ctx, params.sessionId as string)
      return { status: agent.status }
    }
    case 'session.events': {
      const agent = requireAgent(ctx, params.sessionId as string)
      const since = typeof params.since === 'number' ? params.since : 0
      const events: Array<{ seq: number; type: string; data: unknown }> =
        agent.session.events.filter((event) => event.seq >= since).map((event) => ({
          seq: event.seq,
          type: event.type,
          data: event.data,
        }))
      return { events, status: agent.status }
    }
    default:
      throw new RpcError(-32601, `method not found: ${req.method}`)
  }
}

async function main(): Promise<void> {
  const cli = parseCliArgs(process.argv.slice(2))
  const ctx = await boot(bootOptionsFromCli(cli, {
    required: ['agentLoop', 'llm', 'tools', 'sessions', 'agents', 'systemPrompt', 'credentials'],
  }))
  process.stdout.write(JSON.stringify({
    jsonrpc: '2.0',
    method: 'sidecar-ready',
    params: { providers: ctx.llm.listProviders() },
  }) + '\n')

  const rl = createInterface({ input: process.stdin, crlfDelay: Infinity })
  rl.on('line', (line) => {
    if (line === '') return
    let req: Request
    try {
      req = JSON.parse(line) as Request
    } catch {
      respond(undefined, new RpcError(-32700, 'parse error'))
      return
    }
    void handle(ctx, req)
      .then((result) => respond(req.id, null, result))
      .catch((error: unknown) => respond(
        req.id,
        error instanceof RpcError ? error : new RpcError(-32603, String(error)),
      ))
  })
}

void main()