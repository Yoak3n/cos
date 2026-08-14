# cos —— 插件化主干（思想借鉴 dsh）

Rust 再实现插件化主干："一切皆插件" —— Context 服务仓库、inject/provide 依赖、
事件扩展、可逆效果、会话日志唯一事实源、turn/step 主循环；启动时由 cordis.yml 装配插件树。

- 完整实施计划：`PLAN.md`
- 配置指南：`docs/configuration.md`
- 设计决策记录：`docs/decisions.md`
- 语义权威参考（JS 仓库）：`E:\GitVault\deepseek-harness`（`vendor/cordis`、`packages/core/*`）

## 布局

```text
crates/   # 核心与接缝 crate（cos-core、cos-loader、cos-session、cos-llm、cos-memory、…）
          # cos-test-support：测试支持（MockAdapter + 本地回环 chat/completions 服务器，仅 dev-deps）
plugins/  # A 形态插件（plugin-todo、plugin-bash、plugin-llm、plugin-opencode、plugin-memory、plugin-rpc）
src/      # cos CLI 宿主（P6 前为占位二进制）
```

依赖方向铁律：`plugins/*` 与 `cos-agent-loop` 只依赖各接缝的 Definition crate
（cos-llm / cos-tools / cos-agent / cos-shell），不得依赖具体 Provider 或 cos-agent-loop 本身；
cos-core 不依赖任何上层 crate。

## 开发

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check   # CI 内执行（本地需 cargo-deny）
```

## 使用（CLI 三形态）

```bash
# 启动（默认 ./cordis.yml；未配置任何 LLM → 启动失败，提示接入方式）
cos

# 交互式 REPL（显式指定配置 + 命令行接入真实 LLM；需 yml 声明 opencode Provider 插件）
cos --config examples/memory.yml --llm-base-url ... --llm-model ... --llm-api-key ... --llm-no-stream

# stdio JSON-RPC 服务（供外部程序调用；每行一个请求/响应）
cos --config examples/demo.yml --rpc --llm-base-url ... --llm-model ... --llm-api-key ...
# 方法：ping / chat {message, images?} / session / exit / help

# 一次性（演示/脚本；examples/demo.yml 声明了 opencode-provider 插件，需 --llm-* 指向你的端点）
cos --config examples/demo.yml --prompt "帮我记一条演示 todo" --llm-base-url ... --llm-model ... --llm-api-key ...
```

主 agent 的 LLM 解析优先级：`--agent-llm <id>` > `--llm-*` 的 "default" > yml `main` 链/提供商 >
yml 唯一提供商；**无任何 LLM 配置 → 启动失败**。**LLM Provider 是声明式插件**：`--llm-*`
与 `kind: opencode` 都要求 yml 声明 `- name: opencode-provider`（Provider 插件类型
优先级最高，自动排到 `llm` 之前，条目顺序任意；
`config.plan: go|zen` 选套餐、base_url 可覆盖，provider 条目无需再填 base_url）——cos 不再
内置任何 Provider（含 mock，测试用本地回环服务器，见 `crates/cos-test-support`）；任意
OpenAI 兼容端点可用 `custom-provider` 插件纯配置接入（`kind: custom`，无需写代码）；
DeepSeek 官方 API 用 `deepseek-provider` 插件（`plugin: deepseek-provider`，内置
`deepseek-v4-flash`/`deepseek-v4-pro` 目录，无分组）。
opencode 提供商 `streaming` 缺省 false（该网关流式不稳定）；配置值支持 `${ENV_VAR}` 展开
（如 `api_key: "${OPENCODE_API_KEY}"`，密钥不进文件）。

主 agent 驱动（可替换设计）：`--agent-driver <id>`（或 `COS_AGENT_DRIVER`）从 `agent_factory!`
注册表选择驱动器，缺省 `loop`（cos-agent-loop）；未知驱动 = 关键组件缺失 → 启动失败并列出可用驱动。
自定义驱动 = 新 crate + `cos_agent::agent_factory!("<id>", build)` 注册 + 锚点（参考 `src/plugins.rs`）。

三种形态共用同一装配（`assemble`：内置服务 + LLM 注册表 + 插件树 + 主 agent）与收尾
（`finish`：不变量校验 + 会话末 digest + JSONL 落盘 + 优雅卸载）；每轮交互经
`run_turn`（followup → 等 idle，可被 Ctrl-C 取消 → 总结该 turn 的回复与工具轨迹）。

## 作为库嵌入（零插件）

框架核心与插件解耦：`assemble` 传 `config_path: None` 即**零插件装配**——不读 yml、
不装载任何插件，只提供内置服务（Context 事件总线 / 服务仓库 / 工具注册表 / LLM 注册表 /
agent 注册表 / 会话日志）。你自己的项目里可以程序化注册服务与工具、实现自研
`LlmAdapter`（或经 `LlmRegistry` 挂接 provider 插件），再创建 agent 跑 turn：

```toml
[dependencies]
cos = { path = ".." }   # 或 git 依赖
```

```rust
let config = cos::RunConfig { config_path: None, session_id: "my-app".into(), ..Default::default() };
let app = cos::assemble(&config).await?;
// 注册自定义工具 → 注册/实现 LlmAdapter → AgentRegistry::create → run_turn
// 收尾：agent 为自行创建 → cos::finish_with(&app, &agent, &config)
```

完整可运行示例：`cargo run --example embed`（EchoAdapter + 自定义工具，零网络依赖）。
各层经 `cos::core / session / llm / tools / agent / loader / memory / shell / invariants /
rpc / contract` 模块别名暴露；现有插件（todo / bash / memory / llm / opencode / deepseek /
custom-provider / rpc）即"如何写插件"的参考实现，需要时经 cordis.yml 声明装载。

### feature 门控

插件全部按 feature 门控（默认 `full` = 全插件，CLI 形态）。库嵌入可只取框架核心：

```toml
[dependencies]
cos = { path = "..", default-features = false }   # 或 git 依赖
# 需要时按需启用单个插件，如：
# cos = { path = "..", default-features = false, features = ["plugin-memory"] }
```

注意：`--llm-*` 快捷方式依赖 `plugin-opencode` feature（`config_path: None` 的纯库用法
不受影响——适配器由你自行实现/注册）。

## 状态

- P0：workspace 骨架 + cos-core 契约初稿 + hello 插件验收 ✅
- P1：cos-core 完成 ✅ —— 五种分发单测钉死（14）、scope 路由（13）、卸载审计（6）
- P2：cos-loader ✅ —— cordis.yml → 工厂解析（plugin! + inventory）→ 拓扑排序 → 挂载，10 组合测试（依赖序/环/缺依赖/重复 provide 全部 fail loud）
- P3：会话日志 + LLM 接缝 + mock ✅ —— SessionEvent 封闭枚举 + Custom、derive_messages、JSONL 逐字节回放；LlmAdapter 对象安全接缝 + 脚本化 mock
- P4：agent + agent-loop ✅ —— turn/step 主循环（wake_driver → kick → turn → pre-step waterfall → step → turn/end，每步先写日志），Agent 句柄/注册表/Inbox/with_initiator 因果链；端到端单轮快照测试
- P5：tools 管线 + system-prompt + plugin-todo ✅ —— pre/guards/execute/post 瀑布、tool/call 先写日志、结果回流下一 step（derive_messages 含完整 call/result 对）、prompt 装配快照
- P6：A 形态收口 ✅ —— cos CLI（--config/--dump-config/优雅退出逆序卸载）、cos-shell 接缝 + plugin-bash、不变量注册表（模型可见 ⟺ 已记录等 5 条）；demo 端到端快照 + 重放 + 卸载审计全绿，**A 形态 DoD 达成**
- P7 起：B 形态准备与生态（按需推进，详见 PLAN.md §5）

## 桌面陪伴 agent（阶段 2，进行中）

设计文档：`docs/memory-plugin.md`（双层记忆：events append-only 真相源 + topics 可合并状态行 +
关系卡常驻注入；"宁可晚合并，不可错合并"；遗忘曲线做删除，agent 只加强/减弱；诚实出口）。

- M1：记忆内核 + 插件接线 ✅ —— `crates/cos-memory`（rusqlite bundled 五表 schema、apply_turn
  提取→编号消解→合并、遗忘曲线/唤醒、四工具 remember/recall/inventory/demote）+ `plugins/plugin-memory`
  （提供 `memory` 服务 + 注册四工具）；脚本化 mock 生命周期验收 8 测试（新建/别名合并/保守新建/
  correct 取代/衰减与复活/诚实出口/重开持久化/工具），workspace 94 测试全绿
- M2：接 agent 读/写路径 + 真实 LLM ✅ —— `agent/pre-step` 挂钩（每 turn 第一步消化上一 turn，
  记忆失败不阻塞对话）+ `agent/request` 挂钩（Mode A 主动 recall / Mode B 最近聊过 / 关系卡常驻
  注入 system）；`cos-llm` 的 `openai` feature（OpenAI 兼容适配器：流式 SSE 优先、服务端失败自动非流式
  兜底）；cos CLI `--llm-base-url/--llm-model/--llm-api-key`（或 `COS_LLM_*` 环境变量）启用真实
  LLM；`examples/memory.yml` 演示清单；mock 双脚本验收 + 本地回环 SSE 5 测试；实端点冒烟通过
  （不变量全过、逆序卸载）
- M2 端点实测（opencode）：**用户订阅端点 base URL = `https://opencode.ai/zen/go/v1`**（OpenCode Go
  订阅制网关，OpenAI 兼容；请求格式与 bifrost/GoModel 两个独立实现一致）。实测现象：`/zen/go/v1`
  的 `models` 正常，`chat/completions` 当前对该 key **服务端恒 500**（所有模型/鉴权头/请求形状均试
  过，属服务端问题，需查订阅状态或稍后重试）；`/zen/v1`（Zen 按量网关）非流式可用，`deepseek-v4-flash`
  付费模型余额不足（401），**测试期用 `deepseek-v4-flash-free`**；两边网关当前 `stream:true` 均 500
  → 适配器的非流式自动兜底保证可用。base URL 是纯配置项，代码无需改动即可切换
- M3：上下文自动压缩 + digest 慢路径 + 自我认知 ✅ —— `agent/request` 超阈值（`max_context_chars`）
  时旧消息压进**滚动摘要**（`session_state` KV 持久化，`keep_tail` 尾部窗口保原文；压缩失败宁可长
  不可丢），摘要注入 system（进 request/header 日志，不变量不受影响）；digest 慢路径 = 统计
  （事件/主题/跨度地面真值）+ 转录头 → 卡三段注记（高门槛保守，agent_model 含认知缺口→主动追问），
  会话中每 `digest_every` turn 节流触发 + cos 关闭时收尾；修复实端点 429 冒烟暴露的 loop 缺陷
  （LLM 失败路径漏记 step/end，step-pairing 违规，回归测试钉死）；kernel digest/压缩/KV 4 测试 +
  压缩全链路测试；实端点冒烟：主 turn 通过 + digest 遇免费额度限流**失败软降级**（报错不崩、
  不变量全过）
- M3+（后续）：promises 表接线、情绪趋势、晋升机制、多会话持久化（可继续按需推进）
- LLM 统一管理 ✅ —— `cos-llm::LlmRegistry`（服务 `"llm"`，同 ToolRegistry/AgentRegistry 构型：
  工厂 inventory 收集 + 按 id 注册/取用 + 后备链）+ `plugins/plugin-llm`（yml 配置装配
  providers/chains，示例 `examples/llm.yml`）+ `cos_llm::llm_factory!` 工厂注册
  （opencode/mock 已注册，新 provider = 新 crate + 工厂 + 注册，插件树零改动）；
  `FallbackAdapter` 纯 futures 组合子：主 provider 未产出即失败（错误/空流）自动切下一个，
  已产出后失败不切换（防内容重复）；记忆插件按 `llm:` 配置从注册表解析（缺省 "default"），
  cos `--agent-llm <id>` 指定主 agent 提供商/链；测试 11 新增（切换/不切换/全败/注册表/
  插件装配 fail loud）
- LLM 可输入内容标注 + 图片传输 ✅ —— `InputContent { Text, Image }`：适配器声明能力
  （`LlmAdapter::input_content`，缺省 text；opencode 配置 `input_content: [text, image]` 声明
  视觉模型），`FallbackAdapter` 能力 = 成员并集，注册表 `capabilities/supports/by_capability`
  支持按能力路由；`UserMessage.images` 承载图片（URL/data URL，serde default 兼容旧 JSONL），
  opencode 适配器映射为 OpenAI 多部分 content（text + image_url）
