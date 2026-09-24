/**
 * DSH `defineTool` schema DSL → JSON Schema + typed tool definition.
 * Shape-locked to `@deepseek-ai/dsh-tools@0.1.2-rc.1` `schema.d.ts` so community
 * plugins that `import { defineTool } from '@deepseek-ai/dsh-tools'` work on cos.
 * @module @deepseek-ai/dsh-tools/schema
 */

export interface ValueSchemaAnnotations {
  description?: string
  title?: string
  default?: unknown
  examples?: unknown
}

export type StringValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'string'
  enum?: readonly string[]
  const?: string
}
export type NumberValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'number'
  enum?: readonly number[]
  const?: number
}
export type IntegerValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'integer'
  enum?: readonly number[]
  const?: number
}
export type BooleanValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'boolean'
}
export type NullValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'null'
}
export type ArrayValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'array'
  items?: ValueSchemaSpec
}
export type ObjectValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'object'
  properties?: ParameterSchemaSpec
  additionalProperties: boolean
}
export type JsonValueSchemaSpec = ValueSchemaAnnotations & {
  type: 'json'
}
export type OneOfValueSchemaSpec = ValueSchemaAnnotations & {
  oneOf: readonly [ValueSchemaSpec, ValueSchemaSpec, ...ValueSchemaSpec[]]
}

export type ValueSchemaSpec =
  | StringValueSchemaSpec
  | NumberValueSchemaSpec
  | IntegerValueSchemaSpec
  | BooleanValueSchemaSpec
  | NullValueSchemaSpec
  | ArrayValueSchemaSpec
  | ObjectValueSchemaSpec
  | JsonValueSchemaSpec
  | OneOfValueSchemaSpec

export type ParameterPropertySpec = ValueSchemaSpec & { required?: true }
export type ParameterSchemaSpec = Record<string, ParameterPropertySpec>
export type ParameterJsonSchema = Record<string, unknown>
export type JsonSchemaNode = Record<string, unknown>

export type InferValue<S> = unknown
export type InferArgs<S> = Record<string, unknown>

export interface ToolRunContext {
  signal: AbortSignal
  arguments?: unknown
  [key: string]: unknown
}

export interface ContentBlock {
  type: string
  text?: string
  [key: string]: unknown
}

export interface ToolResultLike {
  content: ContentBlock[]
  isError: boolean
  meta?: unknown
}

export interface ToolDefinitionLike {
  name: string
  description: string
  parameters: ParameterJsonSchema
  output: {
    schema: JsonSchemaNode
    render(args: unknown, value: unknown): ContentBlock[]
    presentationMeta?(args: unknown, value: unknown): unknown
  }
  execute(args: unknown, exec: ToolRunContext): Promise<unknown>
  timeoutMs?: number
  finalizeContent?(exec: unknown, result: unknown): ContentBlock[] | undefined
  isConcurrencySafe?(args: unknown): boolean
  presentCall?(args: unknown): unknown
  presentResult?(args: unknown, result: ToolResultLike): unknown
}

export class ToolArgsError extends Error {
  readonly violations: string[]
  constructor(violations: string[]) {
    super(`invalid arguments: ${violations.join('; ')}`)
    this.name = 'ToolArgsError'
    this.violations = violations
  }
}

function annotationKeys(spec: ValueSchemaAnnotations): Record<string, unknown> {
  const out: Record<string, unknown> = {}
  if (spec.description !== undefined) out.description = spec.description
  if (spec.title !== undefined) out.title = spec.title
  if (spec.default !== undefined) out.default = spec.default
  if (spec.examples !== undefined) out.examples = spec.examples
  return out
}

/** Compile one author-facing value schema to raw JSON Schema. */
export function valueSchemaSpecToJsonSchema(spec: ValueSchemaSpec): JsonSchemaNode {
  const annotations = annotationKeys(spec)
  if ('oneOf' in spec) {
    return {
      ...annotations,
      anyOf: spec.oneOf.map((branch: ValueSchemaSpec) => valueSchemaSpecToJsonSchema(branch)),
    }
  }
  if (spec.type === 'json') {
    return { ...annotations }
  }
  if (spec.type === 'array') {
    return {
      ...annotations,
      type: 'array',
      ...(spec.items === undefined ? {} : { items: valueSchemaSpecToJsonSchema(spec.items) }),
    }
  }
  if (spec.type === 'object') {
    const compiled = spec.properties === undefined
      ? { properties: {} as Record<string, JsonSchemaNode>, required: undefined as string[] | undefined }
      : compilePropertyMap(spec.properties, 'properties')
    return {
      ...annotations,
      type: 'object',
      properties: compiled.properties,
      additionalProperties: spec.additionalProperties,
      ...(compiled.required === undefined ? {} : { required: compiled.required }),
    }
  }
  const node: JsonSchemaNode = { ...annotations, type: spec.type }
  if ('enum' in spec && spec.enum !== undefined) node.enum = [...(spec.enum as readonly unknown[])]
  if ('const' in spec && spec.const !== undefined) node.const = spec.const
  return node
}

function compilePropertyMap(
  spec: ParameterSchemaSpec,
  path: string,
): { properties: Record<string, JsonSchemaNode>; required: string[] | undefined } {
  const properties: Record<string, JsonSchemaNode> = {}
  const required: string[] = []
  for (const [key, value] of Object.entries(spec)) {
    if (typeof value !== 'object' || value === null) {
      throw new Error(`defineTool: invalid property schema at ${path}.${key}`)
    }
    const { required: isRequired, ...rest } = value as ParameterPropertySpec
    properties[key] = valueSchemaSpecToJsonSchema(rest as ValueSchemaSpec)
    if (isRequired === true) required.push(key)
  }
  return { properties, required: required.length > 0 ? required : undefined }
}

/** Compile the implicit open parameter object into raw JSON Schema. */
export function parameterSchemaSpecToJsonSchema(spec: ParameterSchemaSpec): ParameterJsonSchema {
  const compiled = compilePropertyMap(spec ?? {}, 'parameters')
  return {
    type: 'object',
    properties: compiled.properties,
    ...(compiled.required === undefined ? {} : { required: compiled.required }),
  }
}

function typeOfJson(value: unknown): string {
  if (value === null) return 'null'
  if (Array.isArray(value)) return 'array'
  return typeof value
}

function validateNode(schema: JsonSchemaNode, value: unknown, path: string): string[] {
  const violations: string[] = []
  const at = path === '' ? '(root)' : path
  const expected = schema.type as string | string[] | undefined
  if (expected !== undefined) {
    const actual = typeOfJson(value)
    const allowed = Array.isArray(expected) ? expected : [expected]
    // integer is a subset of number
    const ok = allowed.some((type) => type === actual || (type === 'number' && actual === 'number') || (type === 'integer' && actual === 'number' && Number.isInteger(value)))
    if (!ok) {
      violations.push(`${at}: expected ${allowed.join('|')}, got ${actual}`)
      return violations
    }
    if (allowed.includes('integer') && typeof value === 'number' && !Number.isInteger(value)) {
      violations.push(`${at}: expected integer`)
    }
  }
  if (schema.const !== undefined && value !== schema.const) {
    violations.push(`${at}: expected const ${JSON.stringify(schema.const)}`)
  }
  if (Array.isArray(schema.enum) && !schema.enum.some((item) => item === value)) {
    violations.push(`${at}: expected one of ${JSON.stringify(schema.enum)}`)
  }
  if (schema.type === 'object' && typeof value === 'object' && value !== null && !Array.isArray(value)) {
    const properties = (schema.properties ?? {}) as Record<string, JsonSchemaNode>
    const required = (schema.required ?? []) as string[]
    const record = value as Record<string, unknown>
    for (const key of required) {
      if (record[key] === undefined) violations.push(`${path ? `${path}.` : ''}${key}: required`)
    }
    for (const [key, child] of Object.entries(properties)) {
      if (record[key] !== undefined) {
        violations.push(...validateNode(child, record[key], path === '' ? key : `${path}.${key}`))
      }
    }
  }
  if (schema.type === 'array' && Array.isArray(value) && schema.items !== undefined) {
    value.forEach((item, index) => {
      violations.push(...validateNode(schema.items as JsonSchemaNode, item, `${path}[${index}]`))
    })
  }
  if (Array.isArray(schema.anyOf)) {
    const branches = schema.anyOf as JsonSchemaNode[]
    const matched = branches.some((branch) => validateNode(branch, value, path).length === 0)
    if (!matched) violations.push(`${at}: does not match anyOf`)
  }
  return violations
}

/** Validate model-generated arguments against an implicit parameter schema. */
export function validateArgs(spec: ParameterSchemaSpec, args: unknown): string[] {
  return validateNode(parameterSchemaSpecToJsonSchema(spec), args, '')
}

export function validateJsonSchemaValue(schema: JsonSchemaNode, args: unknown, path = ''): string[] {
  return validateNode(schema, args, path)
}

export interface DefineToolOptions<S extends ParameterSchemaSpec = ParameterSchemaSpec, O extends ValueSchemaSpec = ValueSchemaSpec> {
  name: string
  description: string
  parameters: S
  output: {
    schema: O
    render(args: InferArgs<S>, value: unknown): ContentBlock[]
    presentationMeta?(args: InferArgs<S>, value: unknown): unknown
  }
  timeoutMs?: number
  isConcurrencySafe?(args: InferArgs<S>): boolean
  execute(args: InferArgs<S>, exec: ToolRunContext): Promise<unknown>
  finalizeContent?(exec: unknown, result: unknown): ContentBlock[] | undefined
  presentCall?(args: InferArgs<S>): unknown
  presentResult?(args: InferArgs<S>, result: ToolResultLike): unknown
}

/**
 * Define a first-party tool with inferred arguments and strict execution
 * validation. Returns a registry-ready definition accepted by
 * `ctx.tools.register` on cos (and by real DSH ToolRuntime).
 */
export function defineTool<const S extends ParameterSchemaSpec, const O extends ValueSchemaSpec>(
  options: DefineToolOptions<S, O>,
): ToolDefinitionLike {
  if (options.timeoutMs !== undefined && (!Number.isFinite(options.timeoutMs) || options.timeoutMs <= 0)) {
    throw new Error(`defineTool(${options.name}): timeoutMs must be a positive finite number`)
  }
  const parameters = parameterSchemaSpecToJsonSchema(options.parameters)
  const outputSchema = valueSchemaSpecToJsonSchema(options.output.schema)
  const validate = (args: unknown) => validateJsonSchemaValue(parameters, args, '')
  const userExecute = options.execute
  const userRender = options.output.render
  const userPresentationMeta = options.output.presentationMeta
  const userFinalizeContent = options.finalizeContent
  const userPresentCall = options.presentCall
  const userPresentResult = options.presentResult
  const userIsConcurrencySafe = options.isConcurrencySafe

  const tool: ToolDefinitionLike = {
    name: options.name,
    description: options.description,
    parameters,
    output: {
      schema: outputSchema,
      render(args, value) {
        return userRender(args as InferArgs<S>, value)
      },
      ...(userPresentationMeta === undefined
        ? {}
        : {
            presentationMeta(args: unknown, value: unknown) {
              return userPresentationMeta!(args as InferArgs<S>, value)
            },
          }),
    },
    ...(options.timeoutMs === undefined ? {} : { timeoutMs: options.timeoutMs }),
    async execute(args, exec) {
      const violations = validate(args)
      if (violations.length > 0) throw new ToolArgsError(violations)
      return userExecute(args as InferArgs<S>, exec)
    },
  }
  if (userFinalizeContent) {
    tool.finalizeContent = (exec, result) => userFinalizeContent!(exec, result)
  }
  if (userPresentCall) {
    tool.presentCall = (args) => {
      if (validate(args).length > 0) return undefined
      return userPresentCall!(args as InferArgs<S>)
    }
  }
  if (userPresentResult) {
    tool.presentResult = (args, result) => {
      if (validate(args).length > 0) return undefined
      return userPresentResult!(args as InferArgs<S>, result)
    }
  }
  if (userIsConcurrencySafe) {
    tool.isConcurrencySafe = (args) => {
      if (validate(args).length > 0) return false
      return userIsConcurrencySafe!(args as InferArgs<S>)
    }
  }
  return tool
}
