/**
 * @cos/plugin-api — third-party plugin type surface for the cos harness.
 *
 * Plugins import types (and Cordis Context service-key augmentations) from
 * here so their tsconfigs need not `include` the harness tree. Internal
 * re-exports use **relative** paths into sibling `@cos/*` sources so this
 * package typechecks whether it is loaded via `node_modules` link or
 * `paths` mapping.
 *
 * Runtime service implementations are still mounted by `@cos/boot` via
 * `pluginPaths` (`@cos/llm`, `@cos/tools`, …); this package is **not** a
 * cordis plugin row.
 * @module @cos/plugin-api
 */

// Core service packages — load for Context declaration merging (ctx.* keys).
// Paths are relative to this file: packages/plugin-api/src/ → packages/<pkg>/src/
import '../../agent-loop/src/index.ts'
import '../../agents/src/index.ts'
import '../../credentials/src/index.ts'
import '../../llm/src/index.ts'
import '../../persistence/src/index.ts'
import '../../session/src/index.ts'
import '../../skills/src/index.ts'
import '../../subagents/src/index.ts'
import '../../system-prompt/src/index.ts'
import '../../tools/src/index.ts'

// Shared vocabulary + runtime helpers plugins commonly need.
export {
  CallId,
  LlmError,
  SessionId,
  createUserMessage,
} from '../../types/src/index.ts'
export type {
  Agent,
  AgentHandle,
  AgentOptions,
  AgentStatus,
  AssembleContext,
  AssembledSection,
  AssistantMessage,
  FinishReason,
  GenerateOptions,
  LlmCallConfig,
  MessageContent,
  ModelBlock,
  ModelMessage,
  PromptAssembly,
  PromptSection,
  Session,
  SessionEvent,
  SessionEventMap,
  SessionEventType,
  StreamChunk,
  TokenUsage,
  ToolSchema,
  TurnEndReason,
  UserMessage,
  WireTool,
} from '../../types/src/index.ts'

// LLM adapter surface (types + the adapter base class plugins extend).
export { LlmAdapter } from '../../llm/src/index.ts'
export type {
  AdapterConfigField,
  LlmProviderInfo,
  ProviderConfigDecl,
  ResolvedModelInfo,
} from '../../llm/src/index.ts'

// Tools registry types + **canonical DSH-shaped authoring surface**.
// Preferred plugin registration syntax (aligned with deepseek-harness):
//   import { defineTool } from '@cos/plugin-api'
//   ctx.tools.register(defineTool({ name, description, parameters, output, execute }))
// The triple form `ctx.tools.register(name, executor, options)` remains as an alias.
// Plugin config (DSH/cordis Standard Schema): export `Config` and cordis validates YAML `config` before apply.
export { default as z } from '@deepseek-ai/schemastery'

/** Settings-page field descriptor (Desktop model-config / ProviderConfigDecl shape). */
export interface PluginConfigField {
  key: string
  label: string
  type?: 'text' | 'password' | 'number' | 'boolean' | 'select'
  secret?: boolean
  required?: boolean
  description?: string
  options?: Array<{ value: string; label: string }>
  default?: string | number | boolean
}

export interface PluginConfigDecl {
  title?: string
  fields: readonly PluginConfigField[]
}
export type { ToolExecutor, ToolOptions, ToolResult } from '../../tools/src/index.ts'
export { dshToolExecutor } from '../../tools/src/index.ts'
export type { DshToolDefinition } from '../../tools/src/index.ts'

// Skills registry surface (ctx.skills).
export type { Skill, SkillMeta } from '../../skills/src/index.ts'
export { SkillsService } from '../../skills/src/index.ts'

export {
  ToolArgsError,
  defineTool,
  parameterSchemaSpecToJsonSchema,
  validateArgs,
  valueSchemaSpecToJsonSchema,
} from '@deepseek-ai/dsh-tools'
export type {
  DefineToolOptions,
  ParameterSchemaSpec,
  ParameterPropertySpec,
  ValueSchemaSpec,
} from '@deepseek-ai/dsh-tools'
