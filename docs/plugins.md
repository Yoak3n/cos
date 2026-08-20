# Plugins & Layered Composition

This document is about how third-party plugins join the harness and how layers
compose — the two halves of delivering `cos` as a sidecar to an outer program.

## One-time model

Every boot is a composed tree. Layers apply to an empty entry list in order:

1. **base layer** — `cordis.yml` next to the launcher (rows with `name:` pointing
   at `@cos/*` workspace packages or bare npm names).
2. **overlay layers** — files listed in `COS_OVERLAYS` (comma-separated paths),
   applied in order as include patches (`cordis.patch.yml`, `overlays/*.yml`).
3. **user patch layer** — `cordis.patch.yml` in the cwd (or `COS_PATCH=<path>`),
   applied last: the user or the outer program has the final say over base and
   overlay rows.
4. **programmatic patches** — `extraPatches` passed to `boot()`.

Patch semantics are the include patch engine: a row is patched by `id` (its
`config` / `disabled` / other fields are replaced field-wise) or inserted with
`insert:`; inserted rows are indexed immediately so later layers can patch them.

## Onboarding a third-party plugin

1. Write a plugin package anywhere in `node_modules` resolution: a function
   plugin with named exports `name` / `inject` / `apply` (no default export).

   ```ts
   // plugins/my-tool/src/index.ts
   import type { Context } from 'cordis'
   import type { PromptSection } from '@cos/types'

   export const name = 'my-tool'
   export const inject = ['systemPrompt']  // optional: declare service deps

   export function apply(ctx: Context, config: { text?: string } = {}) {
     const section: PromptSection = { name: 'my/tool', order: 90, text: config.text ?? 'hello from my tool' }
     ctx.systemPrompt.section(section)     // or ctx.tools.register(...) for a tool
   }
   ```

2. Install it into the harness: `pnpm add file:../plugins/my-tool` (or publish it
   and `pnpm add my-tool`). The package's own dependencies use `file:` paths
   relative to that package's directory (e.g. `"@cos/types": "file:../../cos/packages/types"`).

3. Mount it with a bare-name row on the user layer (`cordis.patch.yml`):

   ```yaml
   - insert:
       - id: my-tool
         name: my-tool
         config:
           text: Greetings from my tool.
   ```

4. Restart the process (or let HMR reload it) — the section/text/tool is now live
   in every assembled prompt. Pass `--patch` to point at a different profile
   patch file, or `--home` to add an outer-program home layer (the last word).

## Sidecar consumption

The harness also ships as an independent process: `@cos/sidecar/server` boots the
same composed tree and serves newline-delimited JSON-RPC over stdin/stdout. An
outer program spawns it and drives agents without importing this codebase:

```ts
import { SidecarClient } from '@cos/sidecar/client'

const client = new SidecarClient({ cwd: process.cwd(), configPath: 'cordis.yml' })
await client.ready                                   // handshake
await client.request('agent.create', { sessionId: 'ext-1', agentOptions: { provider: 'deepseek-official' } })
await client.request('agent.followup', { sessionId: 'ext-1', text: 'use the echo tool' })
await client.request('agent.whenIdle', { sessionId: 'ext-1' })
const { events } = await client.request('session.events', { sessionId: 'ext-1' })
client.dispose()
```

Methods: `ping` / `system.listProviders` / `agent.create` / `agent.followup` /
`agent.whenIdle` / `agent.status` / `session.events`. Logs go to stderr; stdout
carries protocol lines only.

## DSH ecosystem compatibility

DSH plugins are written against the `@deepseek-ai/cordis` ABI. This harness runs the
same upstream cordis version (4.0.0-rc.7), so the compatibility surface is one
npm alias: `@deepseek-ai/cordis` -> `npm:cordis@4.0.0-rc.7` (root
`package.json`). Any plugin written for the DSH ecosystem imports that name and
hooks the same typed event surface (`agent/*`, `session/*`, `system-prompt/assemble`,
`llm/stream`) — no rewrites needed.

The LLM adapter contract follows `docs/user/develop/practice/llm-adapter.md`:
an adapter `extends LlmAdapter` (from `@cos/llm`), `registerAdapter(providers,
adapter)` attaches it to provider routes, and `stream()` yields the
block-protocol `StreamChunk` stream (`block-start` / `text-delta` /
`tool-call-delta` / `block-end` / `usage` / `finish`). `prepareCall` validates
the provider route only — the advertised model catalog is advisory, so an
adapter may accept models it does not list. `@cos/llm` ships a `BlockAssembler`
that converges the adapter's block stream into assembled model blocks, used by
the loop instead of hand-rolled chunk merging.

A plugin that needs an additional service (`ctx.fs`, `ctx.shell`,
`ctx.subprocess`, `ctx.credentials`, …) expects that service to be mounted —
those are plugins too in this architecture, so the outer program composes them
through the same layers instead of patching the harness core. The core spine the
harness guarantees is: sessions, agents, tools, llm registry, systemPrompt,
scope — everything else is provisional until some plugin provides it.

Verified in this repository: `hello-external` is written as a third-party plugin
outside the workspace (imports `@cos/types` and `cordis`), is installed via
`pnpm add file:../cos-plugins/hello-external`, mounted through the user patch
layer, and its section renders in every agent's system prompt with zero code
changes to the harness.

> Note: a patch `insert` that names a plugin that is not installed is silently
> skipped by the loader (it does not fail boot). `@cos/dsh-style` in earlier
> revisions of `cordis.patch.yml` was such a case — that plugin has no package
> on disk, so the row was removed from `cordis.patch.yml`. To mount it, first
> add the plugin package and include it in the resolver manifest. A patch row
> that targets a base `id` which no layer defines (`demo-section` in earlier
> revisions) is likewise a no-op and was removed.

## Verified layers (this repository)

| Layer | File | Effect proven in `pnpm dev` |
|---|---|---|
| base | `cordis.yml` | persona + the 10 core rows (mock provider) | 
| overlay | `overlays/quiet.yml` | disables system-prompt debug printing |
| bundle | `@cos/bundle-base`, `@cos/bundle-real` | base import + real-deepseek insert (`COS_BUNDLES`) |
| user patch | `cordis.patch.yml` | inserts `hello-external` (third-party, outside the workspace) |
| home patch | `$X_COS_HOME/.cos/cordis.patch.yml` | outer-program customization, last word |
| sidecar | `pnpm demo:sidecar` | JSON-RPC round trip with the same composed tree |

## Bundles (profile composition)

A **bundle** is a local directory shipped as a package that declares itself in
`bundle.yml` and carries a `cordis.patch.yml`. Boot composes them in this order:

```
base cordis.yml → the bundle aggregate → overlay files → user cordis.patch.yml → home cordis.patch.yml
```

A bundle's `bundle.yml` declares:

```yaml
bundle:
  id: cos:real
  name: '@cos/bundle-real'
  requires: [llm, tools]   # base rows this bundle depends on; boot validates them
```

Compose bundles and layers on the command line instead of environment
variables:

```sh
pnpm dev --bundles @cos/bundle-base,@cos/bundle-real     # real DeepSeek
pnpm dev --overlays overlays/quiet.yml                    # quiet mode
pnpm dev --patch my-profile/cordis.patch.yml              # profile patch
pnpm dev --home ~/.cos/cordis.patch.yml                   # home patch (last)
pnpm demo:sidecar --bundles @cos/bundle-real              # sidecar, same flags
```

`--bundles` / `--overlays` take comma-separated lists; the sidecar client's
`args` option forwards the same flags to the spawned process. A bundle name
resolves by scanning `node_modules/<name>` (and workspace `packages/<name>`)
for a `cordis.patch.yml` / `bundle.yml`. An empty/comment-only patch file is a
valid no-op bundle (e.g. `bundle-base`). Patch semantics match the include
engine: patch a base row by `id` (replacing its `config`/`disabled`) or
`insert` new rows.