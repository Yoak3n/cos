/**
 * `@deepseek-ai/dsh-system-prompt` — DSH-compatible prompt section types.
 * Registration is `ctx.systemPrompt.section({...})` / `.variable(...)`.
 * @module @deepseek-ai/dsh-system-prompt
 */

export { renderPrompt } from '@cos/system-prompt'
export type {
  AssembleContext,
  AssembledSection,
  PromptAssembly,
  PromptSection,
} from '@cos/types'
