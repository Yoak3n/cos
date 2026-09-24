/**
 * `@deepseek-ai/dsh-tools` — DSH-compatible tool authoring surface on cos.
 * Re-exports the schema DSL (`defineTool`, …) and the registry types plugins
 * expect. Runtime registration goes through `ctx.tools.register(definition)`,
 * which `@cos/tools` accepts in both DSH and cos shapes.
 * @module @deepseek-ai/dsh-tools
 */

export {
  ToolArgsError,
  defineTool,
  parameterSchemaSpecToJsonSchema,
  validateArgs,
  validateJsonSchemaValue,
  valueSchemaSpecToJsonSchema,
} from './schema.ts'

export type {
  ArrayValueSchemaSpec,
  BooleanValueSchemaSpec,
  ContentBlock,
  DefineToolOptions,
  InferArgs,
  InferValue,
  IntegerValueSchemaSpec,
  JsonSchemaNode,
  JsonValueSchemaSpec,
  NullValueSchemaSpec,
  NumberValueSchemaSpec,
  ObjectValueSchemaSpec,
  OneOfValueSchemaSpec,
  ParameterJsonSchema,
  ParameterPropertySpec,
  ParameterSchemaSpec,
  StringValueSchemaSpec,
  ToolDefinitionLike as ToolDefinition,
  ToolResultLike as ToolResult,
  ToolRunContext,
  ValueSchemaAnnotations,
  ValueSchemaSpec,
} from './schema.ts'

/** Minimal runtime identity for a registered tool (DSH `ToolSchema` analogue). */
export interface ToolSchema {
  name: string
  description: string
  parameters: Record<string, unknown>
}
