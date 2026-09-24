# DSH compatibility layer

DSH 插件 ABI 的 cos 兼容垫片。包名保持 `@deepseek-ai/dsh-*`，目录收拢在本层，不再平铺到 `packages/` 根。

| 目录 | 包名 | 映射到 |
|------|------|--------|
| `agent/` | `@deepseek-ai/dsh-agent` | `@cos/agents` / `@cos/agent-loop` |
| `llm/` | `@deepseek-ai/dsh-llm` | `@cos/llm` |
| `scope/` | `@deepseek-ai/dsh-scope` | `@cos/scope` |
| `session/` | `@deepseek-ai/dsh-session` | `@cos/types` / `@cos/session` |
| `system-prompt/` | `@deepseek-ai/dsh-system-prompt` | `@cos/system-prompt` |
| `tools/` | `@deepseek-ai/dsh-tools` | `@cos/tools`（`defineTool` / schema DSL） |

社区插件可继续 `import { defineTool } from '@deepseek-ai/dsh-tools'`。
