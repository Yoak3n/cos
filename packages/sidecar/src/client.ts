/**
 * @cos/sidecar/client — spawns the sidecar process and speaks newline-delimited
 * JSON-RPC over its stdio. An upper-layer program imports this class, spawns
 * the harness sidecar, and drives agents without embedding this codebase.
 * @module @cos/sidecar/client
 */

import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { resolve } from 'node:path'
import type { ChildProcessWithoutNullStreams } from 'node:child_process'
import type { LlmProviderInfo } from '@cos/llm'

export interface SidecarClientOptions {
  /** Path to the sidecar server entry; defaults to this package's server. */
  serverPath?: string
  /** Working directory for the sidecar process. */
  cwd?: string
  /** Config path passed as the sidecar's positional argument. */
  configPath?: string
  /** Extra CLI flags forwarded to the sidecar process (e.g. --bundles ...). */
  args?: string[]
}

export class SidecarClient {
  private readonly child: ChildProcessWithoutNullStreams
  private readonly pending = new Map<number, { resolve: (value: unknown) => void; reject: (reason: unknown) => void }>()
  private readonly rl: ReturnType<typeof createInterface>
  private nextId = 1
  private readyResolve!: (providers: LlmProviderInfo[]) => void
  readonly ready: Promise<LlmProviderInfo[]>

  constructor(options: SidecarClientOptions = {}) {
    const serverPath = options.serverPath ?? resolve(import.meta.dirname, 'server.ts')
    const forwardArgs = [
      ...(options.configPath === undefined ? [] : [options.configPath]),
      ...(options.args ?? []),
    ]
    this.child = spawn(process.execPath, ['--import', 'tsx', '--expose-internals', serverPath, ...forwardArgs], {
      cwd: options.cwd ?? process.cwd(),
      stdio: ['pipe', 'pipe', 'pipe'],
    })
    this.rl = createInterface({ input: this.child.stdout, crlfDelay: Infinity })
    this.ready = new Promise<LlmProviderInfo[]>((resolveReady) => { this.readyResolve = resolveReady })
    this.rl.on('line', (line) => {
      if (line === '') return
      let message: { jsonrpc?: string; method?: string; id?: number; result?: unknown; error?: RpcError }
      try {
        message = JSON.parse(line) as typeof message
      } catch {
        return
      }
      if (message.method === 'sidecar-ready') {
        this.readyResolve((message as { params: { providers: LlmProviderInfo[] } }).params.providers)
        return
      }
      if (message.id !== undefined) {
        const entry = this.pending.get(message.id)
        if (entry === undefined) return
        this.pending.delete(message.id)
        if (message.error !== undefined) entry.reject(new Error(`sidecar rpc error: ${message.error.message}`))
        else entry.resolve(message.result)
      }
    })
    this.child.stderr.on('data', (chunk: Buffer) => process.stderr.write(`[sidecar] ${chunk.toString()}`))
    this.child.on('exit', (code) => {
      for (const entry of this.pending.values()) {
        entry.reject(new Error(`sidecar exited with code ${code ?? 'null'}`))
      }
      this.pending.clear()
    })
  }

  request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    const id = this.nextId++
    const result = new Promise<unknown>((resolve, reject) => { this.pending.set(id, { resolve, reject }) })
    this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n')
    return result as Promise<T>
  }

  dispose(): void {
    this.child.stdin.end()
    this.child.kill()
  }
}

interface RpcError {
  code: number
  message: string
}

export default SidecarClient