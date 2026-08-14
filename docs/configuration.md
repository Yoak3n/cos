# cordis.yml 配置指南

cos 的配置文件是一个 **YAML 数组**（插件清单）：每个条目声明一个插件实例，loader 按
`inject`/`provide` 建图拓扑排序后依次 `apply`。内置服务（tools / system-prompt /
invariants / shell / agents / llm 注册表 / bridges / rpc-providers）由宿主在装载前装配，
**不需要也不允许**在 yml 中声明。

> 快速验证：`cos --config <你的.yml> --dump-config` 输出与真实装载同序的计划 JSON。

---

## 1. 条目通用字段

```yaml
- name: todo            # 工厂名（必须；= 内置插件名 / 自定义插件 plugin! 注册名）
  id: my-todo           # 可选：实例 id（缺省 = name；重复 id 冲突会 fail loud）
  config: {...}         # 可选：插件配置（YAML 原样转 JSON 传给 apply）
  inject: [tools]       # 可选：额外声明的服务依赖（叠加在插件自身 inject 之上）
  disabled: true        # 可选：跳过装载（同 dsh 的 disabled）
```

---

## 2. 内置插件与配置

### `opencode-provider` —— opencode Provider 插件（套餐端点 + 内置模型目录）

**作用**：把 `"opencode"` 工厂注册进 LlmRegistry，并注册**内置模型目录**
（[`BUILTIN_MODELS`]——go/zen 套餐的可用模型，每个模型独立带端点/api 风格/预算）。
`build` 时**三级合并**：插件级套餐兜底 < 模型级目录 < 条目 config。它是 `--llm-*` 与
`kind: opencode` 可用的前提；llm 配置里用 `plugin: opencode-provider` 直接引用本插件
（kind 自动解析，模型从目录选择）。

| 配置字段 | 缺省 | 说明 |
| --- | --- | --- |
| `plan` | `go` | 套餐兜底端点：`go`（OpenCode Go 订阅制，`https://opencode.ai/zen/go/v1`）/ `zen`（Zen 按量，`https://opencode.ai/zen/v1`）；**模型目录命中时以目录为准** |
| `base_url` | 套餐端点 | 端点覆盖（优先级低于模型目录条目、高于套餐兜底） |
| `api_key` | — | **插件级 api_key**：provider 条目可省略（条目仍可覆盖）；支持 `${ENV_VAR}` 展开（如 `${OPENCODE_API_KEY}`，缺失 fail loud），密钥不进文件 |
| `models` | 内置目录 | 模型目录**追加/覆盖**（同名模型后者生效）：`- { model: <id>, group?/defaults: { base_url, api_style, api_key, streaming, max_tokens, input_content, ... } }`；`group` 为分组标签（如 `go`/`zen` 套餐，llm 条目的 `group:` 选择用） |

内置目录（示例性，可按需扩展）：

| 模型 | 套餐 | 端点 | api_style |
| --- | --- | --- | --- |
| `deepseek-v4-flash` | go | `https://opencode.ai/zen/go/v1` | openai |
| `deepseek-v4-flash-free` | zen | `https://opencode.ai/zen/v1` | openai |

`api_style` 字段已预留（当前适配器实现 `openai`；其余风格待适配器扩展——同一 Provider
下不同模型走不同端点/风格时可在此声明）。

```yaml
- name: opencode-provider
  config:
    plan: go
    # 新模型 / 私有网关：追加或覆盖目录条目（同名后者生效）
    models:
      - { model: my-model, defaults: { base_url: "https://my-gateway/v1",
                                       api_style: "openai", streaming: false } }
```

### `deepseek-provider` —— DeepSeek 官方 API Provider 插件（内置模型目录）

**作用**：把 `"deepseek"` 工厂注册进 LlmRegistry（复用 OpenAI 兼容 `chat/completions`
适配器——官方 API 流式稳定，`reasoning_content` 推理内容独立成 Thinking 块），并注册
**内置模型目录**（[`BUILTIN_MODELS`]——官方模型清单，**无分组**：官方就两个模型，
`group:` 在本 Provider 上不可用，报错会列出可用模型）。`build` 时**三级合并**：
插件级官方端点 < 模型级目录 < 条目 config。llm 配置里用 `plugin: deepseek-provider`
直接引用本插件（kind 自动解析，模型从目录选择）。

| 配置字段 | 缺省 | 说明 |
| --- | --- | --- |
| `base_url` | `https://api.deepseek.com` | 端点覆盖（优先级低于模型目录条目、高于官方兜底） |
| `api_key` | — | **插件级 api_key**：provider 条目可省略（条目仍可覆盖）；支持 `${ENV_VAR}` 展开（如 `${DEEPSEEK_API_KEY}`，缺失 fail loud），密钥不进文件 |
| `models` | 内置目录 | 模型目录**追加/覆盖**（同名模型后者生效）：`- { model: <id>, defaults: { base_url, api_style, api_key, streaming, max_tokens, ... } }` |

内置目录（示例性，可按需扩展）：

| 模型 | 端点 | streaming | max_tokens |
| --- | --- | --- | --- |
| `deepseek-v4-flash` | `https://api.deepseek.com` | true | 1000000 |
| `deepseek-v4-pro` | `https://api.deepseek.com` | true | 1000000 |

```yaml
- name: deepseek-provider
  config:
    api_key: "${DEEPSEEK_API_KEY}"   # 插件级：条目可省略
```

### `custom-provider` —— 自定义 Provider 插件（纯配置接任意 OpenAI 兼容端点）

**作用**：注册 `kind: custom` 工厂（复用 OpenAI 兼容 `chat/completions` 适配器）——
**无需写代码**即可接任意端点；`config.defaults` 可把公共字段（base_url 等）下沉为
默认值（支持 `${ENV_VAR}` 展开）；`config.models` 可声明**模型目录**（同一网关下
不同模型走不同端点/风格，如视觉模型带 `input_content: [text, image]`）。
llm 配置里用 `plugin: custom-provider` 直接引用本插件。
代码级自定义（新适配器 crate + 封装插件）的空间仍然保留（见 `docs/third-party-dev.md` §4.4）。

| 配置字段 | 缺省 | 说明 |
| --- | --- | --- |
| `defaults` | — | 公共字段默认值（浅合并到每个 `kind: custom` 条目的 config 之上，条目覆盖） |
| `api_key` | — | **插件级 api_key**：provider 条目可省略（条目仍可覆盖）；支持 `${ENV_VAR}` 展开 |
| `models` | — | 模型目录：`- { model: <id>, group?/defaults: { base_url, api_style, ... } }`（按 model 查默认；`group` 为分组标签） |

```yaml
- name: custom-provider
  config:
    defaults: { base_url: "https://my-gateway/v1" }
    api_key: "${MY_API_KEY}"
    models:
      - { model: my-vision, defaults: { base_url: "https://gw-b/v1",
                                        input_content: [text, image] } }
```

### `llm` —— LLM 提供商统一管理（providers + 后备链）

| 配置字段 | 类型 | 说明 |
| --- | --- | --- |
| `providers` | 数组 | 每条 `{ id, plugin, group?/model?/models? }`：引用 Provider 插件条目 + 可选模型选择（见下） |
| `chains` | 数组 | 每条 `{ id, providers: [id...] }`：后备链（主 provider 未产出即失败 → 自动切下一个） |

provider 条目（**推荐 `plugin:` 引用**）——**模型由插件代码维护，配置按需裁剪**：

```yaml
providers:
  # 省略模型 + group: <组> → 只展开该套餐分组的模型（如 go/zen，组间不串）
  - { id: go, plugin: opencode-provider, group: go }
  - { id: zen, plugin: opencode-provider, group: zen }
  # 省略模型（无 group）→ 插件目录全量展开（聚合默认）：每个模型注册 <id>.<model>，条目 id 成为组链
  # - { id: all, plugin: opencode-provider }
  # 单模型 → 注册 id = 条目 id
  - { id: main, plugin: opencode-provider, model: deepseek-v4-flash }
  # 显式批量 → 只展开列出的模型
  - { id: free, plugin: opencode-provider, models: [deepseek-v4-flash-free] }
  # kind 直接指定（向后兼容）：模型未命中目录时回落到插件级默认
  - { id: fallback, kind: opencode }
```

- **可用模型查询**（`get_available_models`）：运行时 `LlmRegistry::available_models(kind)`；
  插件 crate 层 `plugin_opencode::available_models()`（内置目录 + `config.models` 扩展后的
  模型 id 列表）——模型列表**在 Provider 插件代码里维护**（如 opencode-provider 的
  `BUILTIN_MODELS`），配置面 `config.models` 只做追加/覆盖；
- **分组查询**（`get_available_groups`）：`LlmRegistry::available_groups(kind)` /
  `plugin_opencode::available_groups()` 列出全部组标签；`models_in_group(kind, group)`
  查某组内模型——`group:` 选择与错误提示共用（未知组 fail loud 列出可用分组）；
- 显式选择的模型必须命中目录（fail loud 列出可用模型）；组 id 可在 `chains` 里引用，
  自动展开为组内模型按序；

条目 `config` 与 Provider 插件注册的**默认配置三级浅合并**（条目覆盖默认）：插件级
（套餐兜底端点/api_key）< 模型级（`models` 目录按 `config.model` 命中）< 条目——
所以 `base_url`/`api_key` 等公共字段可省略，`model` 命中目录时连 `streaming`/`max_tokens`
都免填。opencode/custom 共用的字段：

| 字段 | 缺省 | 说明 |
| --- | --- | --- |
| `base_url` | Provider 插件默认 | 端点（不带 `/chat/completions` 后缀） |
| `api_key` | — | 密钥；支持 `${ENV_VAR}` 展开（如 `${OPENCODE_API_KEY}`，缺失 fail loud） |
| `model` | — | 模型 id（plugin 引用时须在插件目录中） |
| `streaming` | `false` | 网关流式不稳定时建议 false（非流式，失败自动兜底） |
| `max_tokens` | `4096` | 输出预算（推理模型建议给足，否则正文可能被裁空） |
| `input_content` | `[text]` | 能力标注；视觉模型声明 `[text, image]` |

```yaml
- name: opencode-provider
  config:
    api_key: "${OPENCODE_API_KEY}"   # 插件级：条目可省略
- name: llm
  config:
    providers:
      - { id: main, plugin: opencode-provider }   # 一行：端点/模型/api_key 全来自插件
    chains:
      - { id: main, providers: [main] }   # 消费者按链 id 引用
```

### `todo` / `bash` / `rpc` —— 工具与 RPC 插件（无配置）

```yaml
- name: todo      # todo_write 工具 + "todo-store" 服务
- name: bash      # bash 工具（经 cos-shell 接缝前台执行）
- name: rpc       # --rpc 委托插件（未声明时宿主回退内置 stdio，功能不丢）
```

### `memory` —— 关系层记忆

| 配置字段 | 缺省 | 说明 |
| --- | --- | --- |
| `db_path` | `memory.db` | SQLite 路径 |
| `llm` | `"default"` | 使用的提供商/链 id（LLM 统一管理） |
| `max_context_chars` | `6000` | 上下文压缩阈值（模型可见消息总字符数） |
| `keep_tail` | `6` | 压缩时保留的尾部消息条数 |
| `digest_every` | `8` | 会话中每隔多少 turn 做一次 digest |

```yaml
- name: memory
  config:
    db_path: sessions/memory.db
    llm: main          # 复用上面的链
```

> 记忆插件解析不到 LLM 时**软降级**（记忆禁用 + stderr 提示，会话照常）。

### B 形态（dlopen）条目

`name` 以 `./` 或 `dlopen:` 开头 → 运行时 dlopen 加载独立 cdylib；`config` 原样以 JSON
传给插件（见 `docs/b-abi.md`）。

```yaml
- name: ./target/debug/plugin_todo_dlopen.dll   # Linux: .so / macOS: .dylib
  config:
    marker: target/dlopen-disposed.txt
```

---

## 3. 顺序约束（插件类型优先级，yml 顺序基本无关）

装配顺序由 loader 注册前**扫描全部插件的类型**决定：**Provider < Core < Other**，
同类型保持配置顺序（稳定）；显式 `inject` 依赖边优先于类型（硬约束）。内置分配：

| 类型 | 插件 | 说明 |
| --- | --- | --- |
| `Provider`（最先） | `opencode-provider` / `deepseek-provider` / `custom-provider` | 注册 LLM 工厂——yml 里写在哪都行，自动排到 Core/Other 之前 |
| `Core` | `llm` | 装配枢纽——自动排在所有 Provider 之后、Other 之前 |
| `Other`（最后） | `todo` / `bash` / `memory` / `rpc` | 工具/记忆/RPC——memory 自动在 `llm` 之后（解析 `config.llm` 时注册表已就绪） |

```yaml
- name: memory              # Other：自动排到最后（无需关心与 llm 的相对顺序）
- name: llm                 # Core
- name: opencode-provider   # Provider：位置任意，自动最先装载
```

> 查看扫描结果：`cos --config cordis.yml --dump-config`，每个条目带 `tier` 字段。
> 第三方插件：实现 `Plugin::tier()` 声明类型（缺省 `Other`）；Provider 封装插件
> 声明 `Provider` 即自动排到 `llm` 之前（见 `docs/third-party-dev.md` §4.4）。

---

## 4. 关键组件（CLI 参数，不进 yml）

- **LLM 必须有**：三种接入方式任选其一——① yml 声明 `opencode-provider` + `llm`（见上）；
  ② yml 声明 `opencode-provider` + 命令行 `--llm-base-url/--llm-model/--llm-api-key`
  （或 `COS_LLM_*` 环境变量，注册为 "default"）；③ 全无 → 启动失败并提示。
- **agent 驱动**：`--agent-driver <id>`（或 `COS_AGENT_DRIVER`），缺省 `loop`；
  未知驱动 → 启动失败并列出可用驱动。
- **主 agent 的 LLM 选择**：`--agent-llm <id>` > `--llm-*` 的 "default" > yml `main` 链 >
  yml 恰好一个提供商。

---

## 5. 完整示例

### 最小可用（命令行接 LLM）

```yaml
# cordis.yml
- name: opencode-provider
- name: todo
```
```bash
cos --config cordis.yml --llm-base-url https://.../v1 --llm-model deepseek-v4-flash --llm-api-key $KEY
```

### 全 yml 配置（真实 LLM + 工具 + 记忆）

```yaml
- name: opencode-provider
  config:
    api_key: "${OPENCODE_API_KEY}"     # 密钥放环境变量，不进文件
- name: llm
  config:
    providers:
      - { id: go, plugin: opencode-provider, group: go }    # 按套餐分组展开（组间不串）
      - { id: zen, plugin: opencode-provider, group: zen }
    chains:
      - { id: main, providers: [go, zen] }      # 组 id 自动展开为组内模型按序
- name: todo
- name: bash
- name: rpc
- name: memory
  config:
    db_path: sessions/memory.db
    llm: main
```

### 禁用某个插件

```yaml
- name: opencode-provider
- name: llm
  config: { ... }
- name: bash
  disabled: true        # 模型失去 shell 执行能力，其余不受影响
```

> 参考：`examples/demo.yml`（最小）、`examples/llm.yml`（多 provider + 链）、
> `examples/memory.yml`（记忆）、`examples/dlopen.yml`（B 形态）。
