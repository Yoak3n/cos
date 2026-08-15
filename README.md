# cos —— Rust 插件化 agent 框架

> 思想借鉴 dsh / cordis 的"一切皆插件"主干，Rust 再实现：**Context 服务仓库、事件总线、
> 可逆效果（fiber 卸载）、会话日志唯一事实源、turn/step 主循环**；启动时由 `cordis.yml`
> 声明插件树。既是一个 CLI（REPL / RPC / 一次性），也是一个**可零插件嵌入**的库。

## 特性

- **插件化主干**：`inject`/`provide` 依赖注入、五种事件分发（`emit`/`parallel`/`serial`/`bail`/`waterfall`）、
  scope 路由、效果逆序回滚（RAII，卸载可审计）；
- **会话日志唯一事实源**：模型可见的一切先写日志（`SessionEvent` 追加式、JSONL 可逐字节重放），
  不变量注册表校验"模型可见 ⟺ 已记录"等纪律；
- **LLM Provider 是声明式插件**：框架不内置任何 Provider——opencode / DeepSeek 官方 /
  任意 OpenAI 兼容端点（纯配置）都是 yml 里的一个插件条目；
- **可替换主循环**：agent 驱动（`agent_factory!` 注册表，缺省 `loop`）、LLM 后备链（`FallbackAdapter`）、
  记忆插件（双层记忆 + 上下文压缩 + digest）；
- **库嵌入零插件**：`config_path: None` 装配即得核心服务（事件总线/工具/LLM/agent 注册表/会话），
  工具与服务全部程序化注册；
- **B 形态预留**：`cos-contract`（零运行时依赖的版本化 ABI）+ dlopen 插件试点（P8）。

## 快速开始（CLI）

```bash
cargo run --release -- --config examples/demo.yml --dump-config   # 看装载计划（不启动）
cargo run --release -- --config examples/demo.yml --prompt "你好" \
  --llm-base-url <url> --llm-model <model> --llm-api-key <key>    # 一次性
cargo run --release -- --config examples/memory.yml               # 交互式 REPL（需 LLM 配置）
cargo run --release -- --config examples/demo.yml --rpc           # stdio JSON-RPC 服务
```

CLI 三形态共用同一装配（`assemble`）与收尾（`finish`）：

| 形态 | 触发 | 说明 |
| --- | --- | --- |
| REPL | 无 `--prompt`/`--rpc` | 交互式对话，流式显示文本与工具轨迹；`/help`、Ctrl-C 取消当前回复 |
| 一次性 | `--prompt <text>` | 一轮即收尾；会话默认落盘 `sessions/demo.jsonl`（`--no-save` 关闭） |
| RPC | `--rpc` | stdio 每行一个 JSON-RPC 请求/响应（`ping`/`chat {message, images?}`/`session`/`exit`/`help`，协议见 `docs/rpc.md`） |

常用参数：`--config <cordis.yml>`（缺省 `./cordis.yml`）、`--session <id>`、`--dump-config`、
`--llm-base-url/--llm-model/--llm-api-key`（或 `COS_LLM_*` 环境变量）、`--llm-no-stream`、
`--agent-llm <id>`、`--agent-driver <id>`。

### LLM 配置与解析优先级

主 agent 的 LLM 解析优先级：`--agent-llm <id>` > `--llm-*` 的 `"default"` > yml `main`
链/提供商 > yml 唯一提供商。**无任何 LLM 配置 → CLI 启动失败**并给出接入引导
（库嵌入则 `agent` 为 `None`，可自行装配适配器，见下文）。

- **`--llm-*` 快捷方式**：指向 OpenAI 兼容端点，但**要求 yml 声明 `- name: opencode-provider`**
  （Provider 工厂由插件在 apply 时注册）；
- **plugin-llm（`examples/llm.yml`）**：yml 装配多 provider + 后备链
  （`providers`/`chains`，主 provider 未产出即失败自动切换，已产出后失败不切换防重复）；
- **Provider 插件**（`Provider` 类型，装配优先级最高，yml 顺序任意）：

| 插件名（yml） | kind | 说明 |
| --- | --- | --- |
| `opencode-provider` | `opencode` | opencode 网关；内置 go/zen 套餐模型目录（`config.plan`、`config.models` 可扩展），`streaming` 缺省 false |
| `deepseek-provider` | `deepseek` | DeepSeek 官方 API；内置 `deepseek-v4-flash`/`deepseek-v4-pro` 目录 |
| `custom-provider` | `custom` | 任意 OpenAI 兼容端点**纯配置接入**（无需写代码） |

- 配置值支持 `${ENV_VAR}` 展开（如 `api_key: "${OPENCODE_API_KEY}"`，密钥不进文件）；
- 完整配置说明见 `docs/configuration.md`。

### agent 驱动（可替换）

`--agent-driver <id>`（或 `COS_AGENT_DRIVER`）从 `agent_factory!` 注册表选择驱动器，缺省
`loop`（cos-agent-loop）；未知驱动 = 关键组件缺失 → 启动失败并列出可用驱动。
自定义驱动 = 新 crate + `cos_agent::agent_factory!("<id>", build)` 注册 + 锚点（参考 `src/plugins.rs`）。

## 作为库嵌入（零插件）

框架核心与插件解耦：`assemble` 传 `config_path: None` 即**零插件装配**——不读 yml、
不装载任何插件，只提供内置服务（Context 事件总线 / 服务仓库 / 工具注册表 / LLM 注册表 /
agent 注册表 / 会话日志）。工具与服务全部程序化注册，适配器可自研：

```toml
[dependencies]
cos = { git = "https://github.com/Yoak3n/cos", default-features = false }
```

```rust
let config = cos::RunConfig { session_id: "my-app".into(), ..Default::default() };
let app = cos::assemble(&config).await?;                     // config_path: None

// 注册自定义工具（Tool trait）
app.root.get::<cos::tools::ToolRegistry>()?.register(Arc::new(MyTool))?;

// 实现 LlmAdapter（或经 LlmRegistry 挂接 provider 插件）→ 创建主 agent
let agent = app.root.get::<cos::agent::AgentRegistry>()?
    .create(cos::agent::CreateAgentOptions {
        session_id: "my-app".into(),
        options: cos::agent::AgentOptions::default(),
        adapter: Arc::new(MyAdapter),
    }).await?;

// 跑一轮 + 收尾（agent 为自行创建 → finish_with 指定它）
let summary = cos::run_turn(&agent, cos::UserMessage::new("你好"), None).await;
let report = cos::finish_with(&app, &agent, &config).await?;
```

- **完整可运行示例**：`cargo run --example embed`（EchoAdapter + 自定义工具，零网络依赖）；
- 各层经模块别名暴露：`cos::core / session / llm / tools / agent / loader / memory / shell /
  invariants / rpc / contract`（如 `cos::core::Context`、`cos::llm::LlmAdapter`）；
- `Assembled` 暴露 `root`（Context）、`app`（插件树）、`agent`（无 LLM 时为 `None`）。

### feature 门控

插件全部按 feature 门控（默认 `full` = 全插件，CLI 形态；`cargo install cos` 即完整版）。
库嵌入可只取框架核心，按需启用单个插件：

```toml
[dependencies]
cos = { git = "https://github.com/Yoak3n/cos", default-features = false }
# 需要记忆插件时：
# cos = { git = "https://github.com/Yoak3n/cos", default-features = false, features = ["plugin-memory"] }
```

可用插件 feature：`plugin-todo` / `plugin-bash` / `plugin-memory` / `plugin-llm` /
`plugin-opencode` / `plugin-deepseek` / `plugin-custom-provider` / `plugin-rpc`。
注意：`--llm-*` 快捷方式依赖 `plugin-opencode` feature（纯库用法不受影响——适配器自行实现/注册）。

## 编写插件（第三方）

任何"插件"都是一个普通 crate：实现 `Plugin` trait（`id`/`inject`/`provide`/`apply`），
`cos_loader::plugin!("name", MyPlugin)` 注册，依赖方向遵守铁律（见下）。现有插件
（todo / bash / memory / llm / opencode / deepseek / custom-provider / rpc）即参考实现。

**依赖方向铁律**：`plugins/*` 与 `cos-agent-loop` 只依赖各接缝的 Definition crate
（cos-core / cos-llm / cos-tools / cos-agent / cos-shell / cos-session），不得开启
`cos-llm` 的 `adapters` feature（内置适配器族，随 feature 引入 reqwest/tokio；只有 Provider
封装插件开它），不得依赖 cos-agent-loop 本身；cos-core 不依赖任何上层 crate；
cos-contract 零运行时依赖（仅 serde），是 B 形态插件的唯一契约。

**完整开发指南**：`docs/third-party-dev.md`（插件 / LLM Provider / 工具 / 测试 harness /
RPC 集成，约 600 行）。

## 架构与布局

```text
src/       # cos 包：库（assemble/finish/run_turn + 门面 re-export）+ CLI 三形态
crates/    # 14 个核心与接缝 crate（见下）
plugins/   # 9 个官方插件（A 形态静态注册；plugin-todo-dlopen = B 形态 cdylib 试点）
examples/  # 演示 yml（demo/llm/memory/dlopen）+ embed.rs（库嵌入示例）
docs/      # configuration / third-party-dev / decisions / b-abi / rpc / memory-plugin
```

| crate | 角色 |
| --- | --- |
| `cos-core` | 内核：Context 服务仓库、事件总线（5 种分发）、Fiber 可逆效果、scope 路由 |
| `cos-contract` | B 形态版本化契约：B-ABI 握手、HostApi 函数表（零运行时依赖） |
| `cos-session` | 会话日志唯一事实源：`SessionEvent` 封闭枚举、derive_messages、JSONL 重放 |
| `cos-tools` | 工具注册表 + 执行管线（pre/guards/execute/post 瀑布） |
| `cos-llm` | LLM 接缝：`LlmAdapter`、Message/ContentBlock、`LlmRegistry` 后备链；`adapters` feature = 内置适配器族（openai/anthropic/responses 风格 + api style 分发） |
| `cos-agent` | `Agent` trait、注册表、Inbox、`agent_factory!` 驱动注册 |
| `cos-shell` | shell 接缝（plugin-bash 用） |
| `cos-loader` | cordis.yml → 工厂解析 → 拓扑排序 → 挂载；`DlopenPluginSource`（B 形态） |
| `cos-agent-loop` | turn/step 主循环（wake → kick → turn → pre-step → step → turn/end，每步先写日志） |
| `cos-memory` | 记忆内核：rusqlite 五表、apply_turn 合并、遗忘曲线、压缩/digest |
| `cos-rpc` | stdio JSON-RPC 协议引擎（`docs/rpc.md`） |
| `cos-system-prompt` / `cos-invariants` | prompt 装配 / 不变量注册表 |
| `cos-test-support` | 测试支持：脚本化 mock + 本地回环 chat/completions 服务器（仅 dev-deps） |

## 开发与 CI

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check        # 本地需安装 cargo-deny
```

GitHub Actions（`.github/workflows/ci.yml`）四个 job：`test` / `fmt` / `clippy` / `deny`。
稳定版 Rust（`rust-toolchain.toml`），edition 2024（MSRV ≥ 1.85）。

## 文档索引

| 文档 | 内容 |
| --- | --- |
| `PLAN.md` | 实施计划（P0-P9 形态演进）与 DoD |
| `docs/configuration.md` | cordis.yml 配置指南（内置插件字段、示例） |
| `docs/third-party-dev.md` | 第三方插件 / Provider / 集成开发指南 |
| `docs/decisions.md` | 设计决策记录（D1-D7 + 实现落地） |
| `docs/b-abi.md` | B 形态 FFI 契约（B-ABI） |
| `docs/rpc.md` | stdio JSON-RPC 协议 |
| `docs/memory-plugin.md` | 记忆插件设计（双层记忆） |

## 状态

- P0-P6：workspace 骨架 → cos-core → cos-loader → 会话/LLM 接缝 → agent/loop → tools →
  A 形态收口 ✅（CLI 三形态、不变量注册表、demo 端到端快照 + 重放 + 卸载审计全绿，**A 形态 DoD 达成**）
- P7：B 形态准备（cos-contract 版本化契约 + `docs/b-abi.md`）✅ 主体完成
- P8：dlopen 试点 ✅（`DlopenPluginSource` + `plugin-todo-dlopen` cdylib + 端到端
  （工具经 C 回调执行、结果回流、disposer 卸载链），`examples/dlopen.yml`）
- P9：B 形态生态（插件 SDK / 验签 / 平台矩阵，按需推进）
- 库化（本阶段）✅：`config_path: None` 零插件装配、`agent` 可选、`finish_with`、
  门面 re-export、8 插件 feature 门控、`examples/embed.rs`、CI 四件套全绿

### 桌面陪伴 agent（阶段 2）

设计文档：`docs/memory-plugin.md`（双层记忆：events append-only 真相源 + topics 可合并状态行 +
关系卡常驻注入；"宁可晚合并，不可错合并"；遗忘曲线做删除，agent 只加强/减弱；诚实出口）。

- M1：记忆内核 + 插件接线 ✅（五表 schema、apply_turn 合并、四工具 remember/recall/inventory/demote）
- M2：接 agent 读/写路径 + 真实 LLM ✅（pre-step/request 挂钩、`--llm-*` 或 `COS_LLM_*`、
  `examples/memory.yml`；实端点冒烟通过）
- M3：上下文自动压缩 + digest 慢路径 + 自我认知 ✅（滚动摘要、卡三段注记、429 限流软降级）
- LLM 统一管理 ✅（`LlmRegistry` + plugin-llm providers/chains + `FallbackAdapter` 后备链 +
  `InputContent` 能力标注/图片传输）
- M3+（后续）：promises 表接线、情绪趋势、晋升机制、多会话持久化（按需推进）
