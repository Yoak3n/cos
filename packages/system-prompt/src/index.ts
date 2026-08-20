/**
 * @cos/system-prompt — dynamic system-prompt assembly (`ctx.systemPrompt`),
 * ported from dsh-system-prompt: ordered sections (global + per-scope
 * shadowing), prompt variables resolved through the scope chain, tool schemas
 * collected from ctx.tools, and the `system-prompt/assemble` waterfall whose
 * returned value is authoritative.
 * @module @cos/system-prompt
 */

import { Service } from 'cordis'
import type { Context } from 'cordis'
import type { Agent, AssembleContext, AssembledSection, PromptAssembly, PromptSection, WireTool } from '@cos/types'
import { scopeChainOf } from '@cos/scope'
import type { ScopeKey } from '@cos/scope'

export type { AssembleContext, AssembledSection, PromptAssembly, PromptSection } from '@cos/types'

declare module 'cordis' {
    interface Context {
        systemPrompt: SystemPrompt
    }
    interface Events {
        /**
         * Expert waterfall over the assembled sections, tools, and variables.
         * @param assembly - the mutable assembly built from registered providers.
         * @param context - the caller's per-assembly context.
         * @mode waterfall
         */
        'system-prompt/assemble'(
            this: SystemPrompt,
            assembly: PromptAssembly,
            context: AssembleContext,
            next: () => Promise<PromptAssembly>,
        ): Promise<PromptAssembly>
        /** Emitted when any prompt provider changes. @mode emit */
        'system-prompt/change'(): void
    }
}

type VariableProvider = (context: AssembleContext) => string | undefined

const VARIABLE_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/

/** Strictly interpolate `{{name}}` references; unknown/undefined references throw. */
export function renderPrompt(assembly: PromptAssembly): string {
    return assembly.sections
        .map((section) => interpolate(section.text, assembly.variables, section.name))
        .filter((text) => text.length > 0)
        .join('\n\n')
}

function interpolate(text: string, variables: Record<string, string | undefined>, sectionName: string): string {
    let result = ''
    let last = 0
    for (let open = text.indexOf('{{'); open >= 0; open = text.indexOf('{{', last)) {
        const close = text.indexOf('}}', open + 2)
        if (close < 0) {
            result += text.slice(last, open + 2)
            last = open + 2
            continue
        }
        const name = text.slice(open + 2, close)
        if (!VARIABLE_NAME.test(name)) {
            throw new Error(`malformed prompt variable reference "{{${name}}}" in section "${sectionName}"`)
        }
        if (!Object.hasOwn(variables, name)) {
            throw new Error(`unknown prompt variable "{{${name}}}" in section "${sectionName}" (registered: ${Object.keys(variables).join(', ') || 'none'})`)
        }
        const value = variables[name]
        if (value === undefined) {
            throw new Error(`prompt variable "{{${name}}}" has no value for this assembly (section "${sectionName}")`)
        }
        result += text.slice(last, open) + value
        last = close + 2
    }
    return result + text.slice(last)
}

/** Registry for ordered sections and prompt variables, global plus per-scope layers. */
export class SystemPrompt extends Service {
    static inject = ['sessions', 'tools']
    private readonly sections = new Map<string, PromptSection>()
    private readonly scopedSections = new Map<ScopeKey, Map<string, PromptSection>>()
    private readonly variables = new Map<string, VariableProvider>()
    private readonly scopedVariables = new Map<ScopeKey, Map<string, VariableProvider>>()

    constructor(ctx: Context) {
        super(ctx, 'systemPrompt')
    }

    /** Register one ordered section; a duplicate name in the same layer throws. */
    section(spec: PromptSection, options: { scope?: ScopeKey } = {}): () => void {
        if (options.scope === undefined) {
            this.assertUnique(this.sections, spec.name)
            this.sections.set(spec.name, spec)
        } else {
            const layer = this.layer(this.scopedSections, options.scope)
            this.assertUnique(layer, spec.name)
            layer.set(spec.name, spec)
        }
        const dispose = this.ctx.effect(() => () => {
            if (options.scope === undefined) this.sections.delete(spec.name)
            else this.layer(this.scopedSections, options.scope)!.delete(spec.name)
        }, 'systemPrompt.section()')
        return () => void dispose()
    }

    /** Register one prompt variable; scoped providers shadow globals. */
    variable(name: string, provider: VariableProvider, options: { scope?: ScopeKey } = {}): () => void {
        if (options.scope === undefined) {
            this.variables.set(name, provider)
        } else {
            this.layer(this.scopedVariables, options.scope).set(name, provider)
        }
        const dispose = this.ctx.effect(() => () => {
            if (options.scope === undefined) this.variables.delete(name)
            else this.layer(this.scopedVariables, options.scope)!.delete(name)
        }, 'systemPrompt.variable()')
        return () => void dispose()
    }

    /**
     * Assemble global + scoped providers, collect tool schemas, resolve
     * variables, then run the `system-prompt/assemble` waterfall.
     */
    async assemble(context: AssembleContext = {}): Promise<PromptAssembly> {
        const variables: Record<string, string | undefined> = {}
        for (const [name, provider] of this.variables) variables[name] = provider(context)
        for (const scope of scopeChainOf(context.scope)) {
            const layer = this.scopedVariables.get(scope)
            if (layer === undefined) continue
            for (const [name, provider] of layer) variables[name] = provider(context)
        }
        const sectionDefs = [...this.sections.values()]
        for (const scope of scopeChainOf(context.scope)) {
            const layer = this.scopedSections.get(scope)
            if (layer === undefined) continue
            for (const [name, spec] of layer) {
                const index = sectionDefs.findIndex((candidate) => candidate.name === name)
                if (index >= 0) sectionDefs[index] = spec
                else sectionDefs.push(spec)
            }
        }
        sectionDefs.sort((a, b) => a.order - b.order)
        const sections: AssembledSection[] = sectionDefs.map((spec) => ({
            name: spec.name,
            text: typeof spec.text === 'function' ? spec.text(context) : spec.text,
        }))
        const tools: WireTool[] = this.ctx.tools.listDefinitions().map(({ name, schema }) => ({
            type: 'function',
            function: { name, description: schema.description, parameters: schema.parameters },
        }))
        const assembly: PromptAssembly = { sections, tools, variables }
        const transformed = await this.ctx.waterfall(
            this,
            'system-prompt/assemble',
            assembly,
            context,
            () => Promise.resolve(assembly),
        )
        return transformed
    }

    private layer<K, V>(map: Map<K, Map<string, V>>, key: K): Map<string, V> {
        let layer = map.get(key)
        if (layer === undefined) {
            layer = new Map()
            map.set(key, layer)
        }
        return layer
    }

    private assertUnique(layer: Map<string, PromptSection>, name: string): void {
        if (layer.has(name)) throw new Error(`prompt section "${name}" is already registered`)
    }
}

export default SystemPrompt