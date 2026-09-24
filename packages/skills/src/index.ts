/**
 * @cos/skills — progressive-disclosure skill registry (`ctx.skills`).
 *
 * Modeled on Claude Code-style skills: a compact catalog always sits in the
 * system prompt (id + description only); the full body is loaded on demand
 * via the `load_skill` tool. Plugins register skills; the service never
 * invents content.
 * @module @cos/skills
 */

import { Service } from 'cordis'
import type { Context } from 'cordis'

/** Catalog row shown in the always-on system-prompt section. */
export interface SkillMeta {
  readonly id: string
  readonly name: string
  readonly description: string
}

/** One registered skill (catalog metadata + full body). */
export interface Skill extends SkillMeta {
  /** Static body, or a provider re-read on every `load` (hot-reload from disk). */
  readonly body: string | (() => string)
  /** Absolute file path when loaded from disk, for debugging. */
  readonly source?: string
}

declare module 'cordis' {
  interface Context {
    skills: SkillsService
  }
}

/** Catalog section order: after persona (90), before tool guides (100). */
const CATALOG_ORDER = 95

export class SkillsService extends Service {
  static inject = ['systemPrompt', 'tools']
  private readonly skills = new Map<string, Skill>()

  constructor(ctx: Context) {
    super(ctx, 'skills')
    this.#mountCatalog()
    this.#mountLoadTool()
  }

  /** Register one skill; duplicate id throws. Returns disposer. */
  register(skill: Skill): () => void {
    if (!skill.id.trim()) throw new Error('skills.register: id is required')
    if (this.skills.has(skill.id)) {
      throw new Error(`skills.register: skill "${skill.id}" is already registered`)
    }
    this.skills.set(skill.id, { ...skill })
    const dispose = this.ctx.effect(() => () => {
      this.skills.delete(skill.id)
    }, 'skills.register()')
    return () => void dispose()
  }

  /** Catalog rows in registration order. */
  list(): SkillMeta[] {
    return [...this.skills.values()].map(({ id, name, description }) => ({ id, name, description }))
  }

  /** Full skill body by id (resolves `body` provider each call). */
  load(id: string): Skill | undefined {
    const skill = this.skills.get(id)
    if (skill === undefined) return undefined
    const body = typeof skill.body === 'function' ? skill.body() : skill.body
    return { ...skill, body }
  }

  #renderCatalog(): string {
    const rows = this.list()
    if (rows.length === 0) return ''
    const lines = rows.map((s) => `- \`${s.id}\` ${s.name}：${s.description}`)
    return [
      '## Skills（按需加载）',
      '下面是已注册 skill 的目录。需要完整说明时用 `load_skill` 读取，不要凭目录猜细节。',
      ...lines,
    ].join('\n')
  }

  #mountCatalog(): void {
    this.ctx.systemPrompt.section({
      name: 'skills:catalog',
      order: CATALOG_ORDER,
      text: () => this.#renderCatalog(),
    })
  }

  #mountLoadTool(): void {
    this.ctx.tools.register('load_skill', async (args) => {
      const id = String((args as { id?: unknown })?.id ?? '').trim()
      if (!id) return { content: 'id 不能为空', isError: true }
      const skill = this.load(id)
      if (skill === undefined) {
        const known = this.list().map((s) => s.id).join(', ') || '（无）'
        return { content: `未找到 skill "${id}"。已注册：${known}`, isError: true }
      }
      return { content: `# ${skill.name}（${skill.id}）\n\n${skill.body}` }
    }, {
      description:
        '按 id 加载 skill 完整说明。先看系统提示词里的 skill 目录，需要细节时再调用本工具。',
      parameters: {
        type: 'object',
        properties: {
          id: { type: 'string', description: 'skill id，与目录中的 `id` 一致' },
        },
        required: ['id'],
      },
    })
  }
}

export default SkillsService
