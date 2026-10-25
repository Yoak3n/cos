/**
 * @cos/llm-retry/degrade — 图片请求被拒收时的内容降级（纯函数）。
 *
 * provider/网关不支持图片输入时，带图请求整单 4xx，回合直接失败。降级策略：
 * 把图片块替换成一段占位文本后重发——模型至少能知道「用户发过图、因模型侧
 * 不支持而未能查看」，如实回话而不是报错崩轮。占位文案与 session 侧 trim 的
 * 历史折叠（[图片已省略]）刻意不同：这里要传达「没收到、非用户的问题」。
 * @module @cos/llm-retry/degrade
 */

import type { MessageContent, ModelMessage } from '@cos/types'

/** 降级占位文案（%d 为该消息里被替换的图片数）。 */
const UNDELIVERABLE_TEMPLATE = '（图片未送达：当前模型或网关不支持图片输入，此处原有 %d 张图片，内容无法查看。请如实告知用户你这次没能收到图。）'

/** 消息里是否含图片块（user 附件 / 工具读图都算）。 */
export function hasImageBlocks(messages: readonly ModelMessage[]): boolean {
  return messages.some((m) => m.content.some((block) => block.type === 'image'))
}

/**
 * 图片块 → 降级占位文本块：每条含图消息只插一段带计数的注释（首图位置），
 * 其余图片并入计数不再单列。无图消息原对象返回（浅等，调用方可据此短路）。
 */
export function stripImageBlocks(messages: readonly ModelMessage[]): ModelMessage[] {
  return messages.map((m) => {
    let images = 0
    for (const block of m.content) {
      if (block.type === 'image') images += 1
    }
    if (images === 0) return m
    const note = UNDELIVERABLE_TEMPLATE.replace('%d', String(images))
    const content: MessageContent = []
    let placed = false
    for (const block of m.content) {
      if (block.type !== 'image') {
        content.push(block)
        continue
      }
      if (placed) continue
      placed = true
      content.push({ type: 'text', text: note })
    }
    return { ...m, content }
  })
}
