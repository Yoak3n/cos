/**
 * @cos/sidecar/plugins — in-process plugin registry for the self-contained SEA
 * build. Every `@cos/*` plugin that a cordis.yml may mount is imported here
 * statically, so esbuild bundles the whole graph (including transforming TS
 * parameter properties) and the loader resolves mount names from this map at
 * runtime instead of `import('@cos/x')` against node_modules.
 * @module @cos/sidecar/plugins
 */

import * as llm from '@cos/llm'
import * as credentials from '@cos/credentials'
import * as session from '@cos/session'
import * as persistence from '@cos/persistence'
import * as systemPrompt from '@cos/system-prompt'
import * as persona from '@cos/persona'
import * as tools from '@cos/tools'
import * as scope from '@cos/scope'
import * as llmDeepseek from '@cos/llm-deepseek'
import * as mockLlm from '@cos/mock-llm'
import * as agents from '@cos/agents'
import * as agentLoop from '@cos/agent-loop'

/** Mount name -> plugin module (namespace carrying `name`/`inject`/`apply`). */
export const plugins: Readonly<Record<string, unknown>> = {
  '@cos/llm': llm,
  '@cos/credentials': credentials,
  '@cos/session': session,
  '@cos/persistence': persistence,
  '@cos/system-prompt': systemPrompt,
  '@cos/persona': persona,
  '@cos/tools': tools,
  '@cos/scope': scope,
  '@cos/llm-deepseek': llmDeepseek,
  '@cos/mock-llm': mockLlm,
  '@cos/agents': agents,
  '@cos/agent-loop': agentLoop,
}

export default plugins
