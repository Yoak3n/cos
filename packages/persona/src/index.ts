/**
 * @cos/persona — deployment persona: one global system-prompt section whose
 * text is interpolated with the loop's registered variables (provider, model,
 * cwd) at every assembly. Swap this section's text to change the deployment
 * voice for every agent.
 *
 * 设计理念：不预设身份 —— 初始不声明"你是谁"、不给角色标签与性格模板。
 * 性格与自我认知由 agent 在对话中自行形成，经记忆插件的身份卡片
 * （agent_model 等字段）沉淀并常驻注入。本节只保留可长期存在的运行环境
 * 事实（模型/工作目录/操作系统/当前时间——避免 agent 对运行环境做错误
 * 假设而走弯路）；"身份尚未形成时可如何行动"这类临时引导放在 memory 的
 * 动态注入里（卡片为空时才出现，成型后自动消失），避免长期占用系统提示词。
 *
 * order 说明：本节只是运行环境事实，不是身份定义，故排在身份卡片
 * （memory:relation-card, order 15）之后、工具引导（order 100）之前——
 * 核心长期上下文（我是谁/我们之间）始终在最前。
 * @module @cos/persona
 */

import type { Context } from 'cordis'

export const name = 'persona'
export const inject = ['systemPrompt']

/** 当前时间（含时区），如 2026-08-25 23:30:00 (UTC+8)。 */
function nowText(): string {
  const d = new Date()
  const pad = (n: number) => String(n).padStart(2, '0')
  const tz = -d.getTimezoneOffset() / 60
  const tzText = tz >= 0 ? `UTC+${tz}` : `UTC${tz}`
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())} (${tzText})`
}

/** 运行平台描述：OS + 架构 + Node 版本。 */
function platformText(): string {
  const os = process.platform === 'win32' ? 'Windows'
    : process.platform === 'darwin' ? 'macOS'
      : process.platform === 'linux' ? 'Linux'
        : process.platform
  return `${os} ${process.arch} (Node ${process.version})`
}

export function apply(ctx: Context) {
  // 运行环境变量：每次组装时动态求值，供 persona 节引用。
  ctx.systemPrompt.variable('datetime', () => nowText())
  ctx.systemPrompt.variable('platform', () => platformText())

  ctx.systemPrompt.section({
    name: 'deployment:persona',
    order: 90,
    text: '当前运行环境：模型 {{provider}}/{{model}}，工作目录 {{cwd}}；系统 {{platform}}，当前时间 {{datetime}}。',
  })
}