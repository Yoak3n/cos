/**
 * launcher + command-line driver. Boots the composed tree via @cos/boot, then:
 *   - with --prompt (or a positional argument): creates one agent, feeds it the
 *     prompt, waits for the turn to settle, prints the reply, and exits;
 *   - without --prompt: stays resident and serves newline-delimited JSON-RPC
 *     over stdin/stdout, reusing the @cos/sidecar protocol (see
 *     packages/sidecar). An upper-layer application connects through
 *     @cos/sidecar/client and keeps the process alive until stdin closes.
 *
 * Usage:
 *   pnpm start --prompt "fix this typo" [--bundles ...] [--provider ...] [--model ...]
 *   pnpm dev -- --prompt "hello"          # same, via the dev script
 *   pnpm start                            # JSON-RPC sidecar on stdin/stdout
 * @module cos/main
 */

import { boot, bootOptionsFromCli, parseCliArgs } from '@cos/boot'
import { createUserMessage } from '@cos/types'
import type { AgentOptions, SessionEvent } from '@cos/types'
import { serve } from '@cos/sidecar/sidecar'

const cli = parseCliArgs(process.argv.slice(2))

const ctx = await boot(bootOptionsFromCli(cli, {
  required: ['agentLoop', 'llm', 'tools', 'sessions', 'agents', 'systemPrompt', 'sessionPersistence', 'credentials'],
}))

const prompt = cli.prompt ?? cli.rest[0]

if (prompt !== undefined && prompt !== '') {
  // One-shot mode: feed a single prompt, print the reply, and exit.
  // The agent's route must be explicit (no guessing in production). Resolve it
  // from the mounted adapter and its first advertised model, so a bare
  // invocation still works while remaining explicit at the call site.
  const provider = cli.provider ?? (ctx.llm.listProviders()[0]?.id ?? '')
  const model = cli.model ?? (provider === '' ? '' : (await ctx.llm.listModels(provider))[0] ?? '')
  const agentOptions: AgentOptions = { provider, model }
  if (provider === '' || model === '') {
    console.error('[boot] no provider/model resolved; pass --provider and --model or mount an adapter')
    await ctx.fiber.dispose()
    process.exit(1)
  }

  const { agent, dispose } = await ctx.agentLoop.createAgent({ agentOptions })
  agent.followup(createUserMessage(prompt, { kind: 'human', detail: 'cli' }))
  await agent.whenIdle()

  // Print the final assistant text (the model's reply) to stdout.
  const lastAssistant = agent.session.events
    .filter((event): event is Extract<SessionEvent, { type: 'assistant/message' }> => event.type === 'assistant/message')
    .at(-1)
  const reply = lastAssistant?.data.message.content
    ?.filter((block) => block.type === 'text')
    .map((block) => block.text)
    .join('')
  if (reply !== undefined && reply !== '') process.stdout.write(`${reply}\n`)

  await dispose()
  await ctx.fiber.dispose()
  process.exit(0)
}

// Interactive/sidecar mode: reuse the shared @cos/sidecar JSON-RPC program so
// an upper-layer application can drive agents over stdin/stdout. Serves until
// stdin closes (EOF).
await serve(ctx)
await ctx.fiber.dispose()
process.exit(0)
