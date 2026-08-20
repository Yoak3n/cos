/**
 * @cos/persona — deployment persona: one global system-prompt section whose
 * text is interpolated with the loop's registered variables (provider, model,
 * cwd) at every assembly. Swap this section's text to change the deployment
 * voice for every agent.
 * @module @cos/persona
 */

import type { Context } from 'cordis'

export const name = 'persona'
export const inject = ['systemPrompt']

export function apply(ctx: Context) {
  ctx.systemPrompt.section({
    name: 'deployment:persona',
    order: 0,
    text: 'You are cos-agent, a coding assistant powered by the {{provider}}/{{model}} model, working in {{cwd}}. Be brief and factual.',
  })
}