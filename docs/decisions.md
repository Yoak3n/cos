# 设计决策记录

> 与 PLAN.md §4 对应。D1–D7 已在计划中拍板，此处记录理由与实现落点；
> P0 补充决策（计划未覆盖到签名以下的实现细节）见文末。
> 语义权威参考（JS 仓库）：`E:\GitVault\deepseek-harness`。

## D1 事件类型化 —— 运行时开放（选 a）

`EventName = &'static str` + `EventPayload = Arc<dyn Any + Send + Sync>`，监听器内 downcast。

- 理由：编译期枚举（linkme 收集变体）穷尽性收益小、跨 crate 收集机制复杂；运行时开放与 B 形态 FFI（字符串事件名 + 二进制载荷）天然兼容。
- 风险应对：waterfall 按 事件名 + 载荷 `TypeId` 配对（`ListenerBody::Waterfall { ty, .. }`）；downcast 失败 dev 模式 panic / release 记日志跳过（P1 硬化）。
- 落点：`crates/cos-core/src/events.rs`。

## D2 异步运行时 —— tokio

stream 与工具并发需要；async-trait 处理 trait 对象。

- P0 暂不在 cos-core 引入 tokio（库本身只含类型与 futures 组合子）；测试与驱动器侧（P1 起）接入。
- 落点：`[workspace.dependencies] tokio`；cos-core 仅 dev-dependencies。

## D3 配置校验 —— serde + schemars

serde 反序列化 + `Validate` trait 手写校验；schemars 生成 JSON Schema（P2 接入 loader 校验；B 形态直接作 wire 契约）。

- P0 落点：`Plugin::Config: DeserializeOwned + Validate`（`Validate` 默认 `Ok`）。

## D4 会话事件 —— 封闭 + Custom 逃生舱

`SessionEvent` 封闭枚举 + `Custom { name, data }`；`derive_messages` 对 Custom 原样透传。（P3 实现）

## D5 错误类型 —— 边界具体枚举

cos-core 公开 API 一律返回 `CoreError`（thiserror）；插件内部实现可自由使用 anyhow。

## D6 scope —— A 形态就做

子 agent / subagent 是主干的一部分。P0 定型 `ScopeKey` / `ScopeTarget` 类型；事件按 scope 路由与父链向上流在 P1 实现（语义权威：`packages/core/scope/src/index.ts`）。

## D7 效果卸载 —— 同步反注册 + 可 await 句柄

- `EffectHandle::dispose` 幂等同步反注册；
- `Fiber::Drop` 逆序执行（RAII 兜底）+ `Fiber::dispose_async()` 供 loader 优雅卸载时等待异步 disposer（`push_async`）。

## P0 补充决策（实现落点，计划未覆盖）

- **Service::NAME**：类型化 TypeMap 需要 名字 → 类型 的桥。`Service` trait 带 `const NAME`，与 `Plugin::provide()` 声明的名字一致；`Context::provide` 据此做同名冲突检测（`DuplicateService`），`get` 用 `TypeId`。
- **监听器按分发模式注册**：`ctx.on*` 注册时声明 `DispatchKind`（同 cordis `ctx.on(name, l, { type })`），`ctx.emit/serial/bail/parallel/waterfall` 只分发对应模式的监听器。五种语义照 `vendor/cordis/src/events.ts`：
  - `emit`：同步逐个调用，返回值忽略；
  - `parallel`：全并发 await，任一失败 → 聚合错误（`ListenerAggregate`，同 `AggregateError`）；
  - `serial`：异步按序 await，监听器返回 `Ok(Some(v))`（bail 值）即停止并返回 `v`；
  - `bail`：同步按序，第一个非 `None` 返回；
  - `waterfall`：监听器包裹剩余链，不调 `next()` 即 veto，链尾为调用方提供的默认行为。
- **waterfall 签名**（按计划风险表应对）：`Fn(&mut Decision<T>) -> BoxFuture<ControlFlow<T>>`；`Break(v)` 短路、`Continue(())` 且未调 `next()` 静默短路（沿用当前值）、默认行为作为链尾写入决策值。P1 单测钉死。
- **监听器随 fiber 失效**：`ctx.on*` 内部即 `fiber.push(remove-handle)`（同 dsh `fiber.effect`），fiber 卸载后监听器自动移除；返回的 `EffectHandle` 亦可提前 dispose（幂等）。
- **EffectHandle 语义**：dispose 幂等、**显式触发**——丢弃句柄克隆是无操作（同 dsh 返回的 disposer 函数：调用才生效）；自动逆序回滚只由 `Fiber::Drop` / `dispose_async` 承担。fiber 持有每个注册效果的句柄克隆，插件也可保留克隆以便提前反注册。
- **scope 路由语义（P1 落地，照 `packages/core/scope/src/index.ts`）**：监听器注册时记录其上下文的 scope tag；`ScopeTarget::Key(k)` 分发时无 tag 监听器全收、tag 位于 `k` 祖先链（含 `k`）的监听器收——事件只向上流（祖先监听子孙，子孙不监听祖先）；`ScopeTarget::None` 对应 dsh 无 key 的 unkeyed carrier（排除一切带 tag 监听器）；父链绑定每 key 一次、拒绝成环（`bindScopeParent` 语义，无 rebind——A 形态不做运行时重组）。`fork_scoped(key)` = `createScope` 的 scoped ctx；`fork()` 继承 tag。
- **注册纪律（P1）**：`ctx.on*` / `ctx.provide` 返回 `CoreResult<EffectHandle>`；fiber 已卸载后注册 → `CoreError::InactiveFiber`（同 dsh `INACTIVE_EFFECT`，fail loud）。
- **loader 静态注册表（P2）**：`plugin!("name", MyPlugin)` 宏 + inventory 收集。`PluginRegistrar` 全部字段为函数指针（const 可构造，`inventory::submit!` 直接收集，无运行时初始化）；`P: Default`，实例装载期构造。`Plugin::inject/provide` 因此改为返回 `&'static [&'static str]`（P0 签名细化）。
- **cordis.yml 形态（P2，P13 修订）**：`Profile = Vec<Entry>`（cordis.patch.yml 顶层数组语义；~~v1 无层叠~~ **P13 起支持 patch 层叠**，见文末 P13 条目）；条目 `{id?, name, config?, inject?, disabled?}`，config 原位解析为 `serde_json::Value`（§6：B 形态 wire 格式）；错误风格照 JS `failed to apply loader entry <id> (<name>): detail`。装载失败靠 RAII 自动回滚（已 apply 的 fork Context 随栈展开 drop → fiber 逆序反注册）。
- **会话 wire 形状（P3）**：`SessionEvent` 信封 `{seq, time, type, data}`（flatten 的 tag/content 枚举）；事件名含 `/` 分隔符（`turn/start` 等）——serde 的 `rename_all` 只能做词法转换、会损毁 `/`，故逐变体显式 `rename`；版本钉 0、不兼容日志直接拒绝（同 dsh 无迁移语义）。serde 带 tag 枚举**不支持 newtype 变体**，`ContentBlock`/`ChunkDelta` 用 struct 变体（`Text { text }`）。
- **derive_messages 投影（P3）**：surface = user/message、assistant/message、tool/result 按 seq 序；`Custom` 原样透传为 `Message::Custom`（决策 D4）；chunk/边界/请求头/工具调用为 log-only，不参与投影。
- **LlmAdapter 接缝（P3）**：对象安全（同步方法 + boxed `LlmStream`）；usage 随末块携带（`StreamChunk.usage`），agent-loop（P4）装配进 assistant/message。
- **waterfall 载荷/返回值分离（P4，修订 P1 定型）**：`Decision<P, V>` —— 载荷 `P` 经 `set_value` 变换、链返回值 `V`；veto = 不调 `next()` 直接返回 `V`（ControlFlow 退役）。P1 的单类型 `Decision<T>` 无法表达 dsh 的真实用法（pre-step 载荷是消息、链返回决策）；P1 测试同步迁移。监听器按 `(TypeId::of::<P>, TypeId::of::<V>)` 类型对配对。
- **Inbox claim 语义（P4，照 dsh inbox.ts）**：总是先取光 next-step，`NextTurn` 目标再额外取 next-turn 队列的**恰好一条**——每个排队 turn 消费一条，同 turn 内可再消费 steering/注入。
- **Session 内部可变性（P4）**：`Session` 改为 `Arc<Mutex<Inner>>`（廉价 Clone）：Agent 句柄对外共享 `&Session` 只读视图，loop 以 `&self` 追加（写路径单写者）。
- **驱动器相位与因果链（P4，照 agent.ts/AgentRegistry）**：`Phase { Idle, Running{turn,step,aborted}, Maintenance{wake_requested} }`；wake 闩在维护收敛后重放；`Running` 状态事件在调用方任务发出（同 dsh），`Idle` 在驱动器任务内（with_initiator 边界内，发起者可见）。`run_maintenance` 用类型擦除的 `Maintenance`（FnOnce + AbortSignal），对象安全。
- **turn 配对不变量（P4 测试钉死）**：`TurnStart` 之后的一切提前返回（含取消）必须写 `TurnEnd{Aborted}` 收束。cancel_race 测试套（`crates/cos-agent-loop/tests/cancel_race.rs`：流中取消 / 调度前取消 / 工具执行中取消 / keep_inbox / 双重取消幂等）发现入口 `check_abort` 的 `?` 泄漏未闭合的 TurnEnd（配对不变量违规）——修复为收束前显式写 `TurnEnd{Aborted}`；`assert_pairing_balanced` 断言 StepStart↔StepEnd、TurnStart↔TurnEnd 配对平衡。
- **工具管线（P5，照 tool-execution-pipeline.md）**：`tool/call` 先写日志（loop）→ `tools/pre-execute`(waterfall, Allow/Deny) → 单调守卫 → `tools/execute`(waterfall，链尾=工具体) → `tools/post-execute`(waterfall) → `tool/result` 写日志 + `tools/result` 实时通知。P5 简化：顺序执行（无并发池/屏障）、无 approval 服务（pre-execute veto 代替）、无 additionalContexts；参数解析照 dsh（空串→`{}`，非法 JSON→原串文本）。
- **工具结果回流（P5，照 agent.ts）**：run_step 返回 `Option<TurnEndReason>`——`None` = 已执行工具、turn 继续；下一 step 以**空消息**进入（无新 user/message），derive_messages 含 Tool 结果回流模型。
- **todo_write（P5）**：整表替换、最后写入胜出；会话态经 `current_initiator()` 因果链写 `todo/write`（log-only 事件，新增入封闭枚举）。
- **cos 宿主（P6）**：根 package `cos` 即 CLI（`--config`/`--dump-config`/`--session`/`--prompt`/`--no-save`）；内置服务（tools/system-prompt/invariants/shell/agents）在装载前装配；`loader::plan()` 供 `--dump-config` 与装载共用同一路径（输出与装载一致）；优雅退出 = `LoadedApp::dispose_async`（apply 逆序，顺序可审计）；Ctrl-C 经取消信号 → 活动 turn 以 aborted 收束 → 卸载。插件 crate 需被显式引用（`builtin_plugin_ids` 锚点），否则 MSVC 链接器丢弃 object、inventory 收集不到注册表。
- **P6 简化的 shell**：`cmd /C` 前台执行、无后台 job、无 sandbox（PLAN.md 明示 v1 范围）；`ShellProvider(Arc<dyn Shell>)` 包装服务，插件消费接缝不绑 LocalShell。
- **根 crate = cos**：计划 §2 的 `app/` 由根 package `cos` 承担（用户拍板"根就叫 cos"）；CLI 可执行即 cos 二进制（P6 落地）。
- **依赖 vendored 方案（已取消）**：早期本环境无法访问 crates.io（SSL 受限），依赖曾经 `cargo vendor --offline` 落入仓库内 `vendor/`、由 `.cargo/config.toml` 的 source replacement 生效；网络恢复后用户拍板取消——已删除 `vendor/` 与 `.cargo/config.toml`，恢复标准 crates.io 解析（Cargo.lock 不变，构建实测通过）。tokio 1.52 默认 features 为空，需显式声明 `["macros", "rt-multi-thread", "time", "sync"]`。
- **edition 2024 / resolver 3**：仓库原为 `cargo new` 默认（edition 2024），沿用。
- **rust-toolchain**：pin stable（rustfmt + clippy 组件），CI 与本地一致。

## 阶段 2（桌面陪伴 agent）—— M1 记忆插件决策

- **存储选型**：SQLite（rusqlite 0.40 `bundled`，静态编译 sqlite3.c，无运行时环境依赖）；五表 schema：`events`（append-only 真相源 + superseded 标记）、`topics`（recall 只查这层）、`relation_card`（单行常驻）、`promises`（M2 填充）、`self_history`（agent 自我行为审计：demote 等）。
- **分层与衰减**：`Tier { Episodic(0.05/天, 阈值 0.02), Trivia(0.15/天, 阈值 0.10) }`；删除只发生在 `apply_decay`（打开时 + 每轮 apply 后 + recall 前），agent 侧只有 remember/demote（加强/减弱），demote = `weight * 0.3` + self_history 记账，可被再次提起复活（activate：激活 +1、时间刷新、`weight*1.1` 封顶 1.0）。
- **编号消解（resolve_topic）**：Stage 1 词法阻塞（canonical/alias 精确直通；字符 bigram Jaccard 近邻 top-3 收候选）→ Stage 2 LLM 仲裁（`{"merge": "<id>" | "none"}`）；候选为空 → `Create{uncertain:false}`，仲裁 none → `Create{uncertain:true}`——"宁可晚合并，不可错合并"，假阴性可恢复、假阳性不可恢复。
- **提取窄而弱**：三类事实（user/self/relation）× 三动作（new/extend/correct）只抄字面；correct 走"旧陈述 superseded + 状态整段替换"路径。合并/仲裁/卡维护全部 JSON 契约（```` ```json ```` 围栏剥壳解析），经 `LlmAdapter` 接缝注入，测试用脚本化 mock（cos-llm-mock，按调用序号出栈）。
- **诚实出口**：recall 无命中（词法 < 0.05 不参与）→ `RecallOutcome { none: true }`，模型可见文本"无相关记忆"。
- **服务装配**：`MemoryLlmProvider`（`memory-llm`，包装 `Arc<dyn LlmAdapter>`）与 `MemoryStore`（`memory`）均为 `Service`；`plugins/plugin-memory` apply 时打开存储 + 注册四工具（remember/recall/inventory/demote），工具执行期经 `ctx.get::<MemoryStore>()` 取共享实例（storage 在 `Arc` 内、`Mutex<Connection>`）。
- **update_topic_merged 事务纪律**：别名簿记在**同一事务**内查 canonical（SQL）比较——`std::sync::Mutex<Connection>` 不可重入，禁止持锁回调 `self.*`（M1 曾因 `canonical_of` 嵌套加锁死锁，回归修复）。
- **events() 形状**：公开为 `(event_id, statement, ts, superseded)` 四元组——验收"correct 取代旧陈述"需要 superseded 可见；真相源永不删行。
- **M1 范围**：内核 + 接线 + 8 验收测试（新建/别名合并/保守新建/correct 取代/衰减与复活/诚实出口/重开持久化/四工具）；M2 才接 agent-loop（turn 挂钩、关系卡常驻注入、pre-step 主动 recall、真实 LLM 适配器）。

## 阶段 2（桌面陪伴 agent）—— M2 决策

- **真实 LLM 适配器（cos-llm 的 `adapters` feature，原 `openai`）**：OpenAI 兼容 `chat/completions`；`LlmAdapter::stream` 是同步方法 → 内部 `tokio::spawn` + unbounded channel 转发（调用方须在 runtime 内，同 mock 语义）。**流式优先、自动非流式兜底**：SSE 在未产出任何 chunk 前遇服务端失败（HTTP 5xx / `{"type":"error"}` 块）→ 重发 `stream:false` 单次请求，`choices[0].message.content`（空则 `reasoning_content`）+ usage 合成一个 chunk；4xx（鉴权/余额）不重试原样报错；已产出部分 chunk 后失败不再兜底（避免重复内容）。错误一律作为流内 `Err` 交付，不进 stderr。
- **opencode 端点（用户确认 + 实测）**：**订阅网关 base URL = `https://opencode.ai/zen/go/v1`**（OpenCode Go，
  订阅制，OpenAI 兼容；models.dev、bifrost、GoModel 三方实现一致：Bearer + `/v1/chat/completions`，支持 SSE）。
  实测（当日）：`/zen/go/v1/models` 正常；`/zen/go/v1/chat/completions` 对该 key **服务端恒 500**
  （全部模型/鉴权头/请求形状变体均试），需查订阅状态或稍后重试；`/zen/v1`（Zen 按量网关）非流式可用、
  `deepseek-v4-flash` 余额不足（401 CreditsError）、**测试期用 `deepseek-v4-flash-free`**；两边网关
  `stream:true` 当日均 500 → 靠非流式兜底保证可用。base URL 为纯配置（`--llm-base-url`），代码零改动可切换。
- **agent 读/写挂钩（plugin-memory）**：写 = `agent/pre-step` waterfall（step 1、turn > 1 时消化上一 turn）：`current_initiator()` 取会话 → 按 `TurnStart` 跟踪 turn 号重建 user/assistant 文本（UserMessage 事件无 turn 字段）→ `apply_turn` **内联 await**（正确性优先：下一请求前记忆已就绪；M3 digest 再优化时延）。记忆失败只落 stderr、不阻塞对话。读 = `agent/request` waterfall（`next()` 委托后改 `system`）：查询 = 请求里最后一条用户消息；Mode A 命中 →【相关记忆】段，否则 Mode B `recent_feed(3)` →【最近聊过】段；关系卡（profile/agent_model/relationship）有内容时常驻注入【关系卡】段；原 system 保留在注入段之后。注入发生在 `request/header` 日志之前 → 模型可见 ⟺ 已记录不变量继续成立。两个监听器随插件 fiber 卸载自动失效。
- **MemoryStore::open 建父目录**：`sessions/memory.db` 等路径父目录不存在时 `create_dir_all`（新增 `MemoryError::Io`）；`/sessions` 运行时产物入 .gitignore。
- **M2 验收**：本地回环 TCP 打桩 5 测试（流式增量 + usage、4xx 不重试、5xx 兜底、error 块兜底、兜底双败报错）+ agent 双 mock 全链路测试（turn 消化 → recall/关系卡注入 system）+ 实端点冒烟（75 事件、不变量全过、逆序卸载）。

## 阶段 2（桌面陪伴 agent）—— M3 决策

- **上下文自动压缩（agent/request，M3）**：模型可见消息总字符数超 `max_context_chars`（默认 6000）→
  旧消息压进**滚动摘要**（LLM 步：旧摘要 + 新增对话 → 新摘要，要点式 ≤300 字），保留尾部 `keep_tail`
  （默认 6）条原文；摘要经 `session_state` KV 表按 `summary:{agent_id}` 持久化（重启可续）。**压缩失败
  宁可长不可丢**：不截断、不降级（与"宁可晚合并不可错合并"同源）。摘要注入 system 而非伪造消息 →
  进 request/header 日志，模型可见 ⟺ 已记录不变量不受影响；旧消息从请求剔除是安全的（不变量只要求
  可见者必有日志，不要求日志全可见）。
- **digest 慢路径（推断层，M3）**：`MemoryStore::digest(transcript, ts)` = 统计（事件数/主题数/高频
  主题/时间跨度，确定性地面真值）+ 转录头（确定性截断 12k 字符，非 LLM 压缩）→ `digest_notes` 三段
  注记（高门槛保守：只写有统计或转录直接支撑的结论；认知缺口对照模板坐标系写进 agent_model）→
  `merge_card_section` 逐段落库。触发双轨：会话中 `agent/status`→Idle 按 turn 进度节流（每
  `digest_every` 默认 8 turn 一次，避免每次空闲都打 LLM——曾误触发于每个 turn 间隙，mock 脚本被
  抢消耗费）+ **宿主收尾**：cos 在 when_idle 后对会话末段显式 `digest`（单会话 CLI 的"会话末"就在这）。
  失败只落 stderr、guard 不推进（下次重试），陪伴不因 digest 崩。
- **M3 实端点暴露的 loop 缺陷**：LLM 请求失败路径（429 等）旧实现提前 return 漏记 `step/end` →
  step-pairing 不变量违规；修复为失败分支同样先写 StepEnd 日志再收束（回归测试
  `crates/cos-agent-loop/tests/error_path.rs` 钉死）。
- **实端点限流观测**：免费模型 `deepseek-v4-flash-free` 有 FreeUsageLimitError（429），当日测试消耗
  较快；主 turn 与 digest 失败均**软降级**（流内 Err → 日志 → 继续），digest 失败不设 guard 下回重试。

## LLM 统一管理（plugin-llm）

- **构型**：`cos-llm::LlmRegistry`（服务 `"llm"`）与 ToolRegistry/AgentRegistry 同构——宿主装配空
  注册表，`plugins/plugin-llm` 按 yml 配置填充（providers 按 kind 实例化 + chains 后备链），
  消费者按 id/链 id 解析。~~工厂经 `cos_llm::llm_factory!("kind", build_fn)` inventory 注册~~。
  **Provider 插件化（P9 修订）**：`llm_factory!` 注册移除——适配器 crate 只提供实现与
  `build_*`/`KIND` 常量；**Provider 封装插件**（`plugin-opencode` 范本）在 apply 时经
  `LlmRegistry::register_factory(kind, build)`（程序化注册，inventory 之外的声明式路径）注册；
  yml 不声明对应插件则 `kind` 不可用（fail loud，plugin-llm 错误列出可用 kinds）。
  新 provider = 适配器 crate + 封装插件 + 锚点（`builtin_plugin_ids`），宿主核心零 Provider 引用。
- **后备链语义（FallbackAdapter）**：纯 futures unfold 状态机（无 spawn、cos-llm 不引 tokio）；
  主 provider 在产出任何 chunk 前失败（错误/空流）→ 记错误并切下一个；**已产出后失败 → 原样传播
  不切换**（避免内容重复）；全部未产出 → 交付最后错误。这正是 opencode 网关抖动/流式不稳定的通用解。
- **loader 宿主服务边界**：loader 的 inject 校验只认插件 provide 表，宿主装配的服务不可见——
  plugin-llm/plugin-memory 不声明 inject，靠 apply 时 `ctx.get` fail loud（同 plugin-todo）；
  无依赖边时按**插件类型优先级**排序（见下条），故 **llm 自动先于 memory**（memory 是
  Other、llm 是 Core）。
- **插件类型（tier，装配优先级）**：`Plugin::tier()` 声明类型（`PluginTier`：
  **Provider < Core < Other**，缺省 Other）——loader 注册前**先扫描全部插件**，Kahn
  拓扑排序的就绪集按 `(tier, 配置下标)` 出队：跨层按类型（Provider 最先注册 LLM 工厂，
  其次 Core 装配枢纽如 plugin-llm，最后 Other 工具/记忆/RPC），同层保持配置顺序
  （稳定）；`inject` 边仍是**硬约束**（优先级只作用于无依赖边的节点间）。取代
  ~~可选依赖边~~（`Plugin::optional_inject` + `provider:*` 标记，已移除）：内置
  Provider 插件（opencode/deepseek/custom）声明 `Provider` 类型、plugin-llm 声明
  `Core`——yml 条目顺序写反也能正确装载；**第三方 Provider 插件声明 `Provider`
  类型即自动排到 llm 前**，无需再改 plugin-llm 的硬编码列表。`--dump-config` 输出
  各条目 `tier` 字段（扫描结果可见）。
- **记忆插件 LLM 解析**：`MemoryConfig.llm`（provider/链 id，缺省 "default"）→ 注册表解析；
  原 `MemoryLlmProvider` 服务删除（回归测试改注册表装配）。~~cos 无 `--llm-*` 时注册
  "default" = 空脚本 mock~~（**已废弃（P9）**：隐式 mock 兜底移除；`--llm-*` 在插件树之后
  注册 "default"，故记忆插件解析失败改为**软降级**（记忆禁用 + stderr 提示，会话照常——
  "记忆失败不阻塞对话"语义从运行时扩展到装配期））；
  `--agent-llm <id>` 指定主 agent 提供商/链，`--llm-*` 仍注册 "default" 快捷方式（经 opencode 工厂）。
- **可输入内容标注（text/image）**：`cos_llm::InputContent { Text, Image }`（serde lowercase，
  可扩展）；`LlmAdapter::input_content() -> &[InputContent]` 缺省 `[Text]`（对象安全、零破坏），
  视觉模型经配置声明 `input_content: [text, image]`（opencode 提供商配置透传）；
  `FallbackAdapter::new` 计算成员**并集**。注册表查询面：`capabilities(id)`（提供商或链，
  链 = 并集）、`supports(id, content)`、`by_capability(content)`（提供商路由查询）。
  图片传输：`UserMessage.images: Vec<String>`（URL/data URL，`#[serde(default)]` → 旧 JSONL
  兼容、不 bump 版本）；opencode 适配器把带图用户消息映射为 OpenAI 多部分 content
  （`[{type:text}, {type:image_url}]`），纯文本仍为字符串（线上形态不变）。
  能力标注与传输解耦：标注用于路由/校验，传输由消息承载。
- **CLI 三形态（REPL / RPC / 一次性）**：共用 `assemble`（内置服务 + LLM 注册表 + 插件树 +
  主 agent 创建）、`run_turn`（followup → 等 idle，可被取消信号中断 → 从会话日志总结该 turn
  的助手文本与工具轨迹）与 `finish`（不变量/digest/落盘+重放校验/逆序卸载）。形态选择：
  无 `--prompt` 默认 REPL（TTY 显示提示符与横幅，管道输入逐行对话、EOF 结束）；
  `--rpc` = stdio JSON-RPC 2.0 子集（每行一请求/响应：`ping`/`chat`{message,images?}/
  `session`/`exit`/`help`，非法行 -32700、未知方法 -32601）；`--prompt` 保持一次性。
  Ctrl-C 语义：一次性 = 取消后退出；REPL = 回复中取消当前 turn 回提示符（消费信号位）、
  提示符处退出；RPC = 取消进行中的 chat、空闲时退出。e2e：spawn 真实二进制走管道协议
  （`tests/rpc_e2e.rs`，`--llm-*` 指向本地回环 chat/completions 服务器——cos-test-support，
  真实适配器协议离线驱动）。
- **零参数启动**：`--config` 缺省 `./cordis.yml`；主 agent LLM 解析优先级
  `--agent-llm` > `--llm-*` 的 "default" > yml `main`（链或提供商）> **yml 恰好一个非 default
  提供商**（不猜多个）> ~~确定性演示脚本~~（**已废弃（P9）**：`demo_mode` 隐式 mock 兜底移除，
  无任何 LLM 配置 → 启动失败并提示接入方式；Provider 一律声明式插件
  （`- name: opencode-provider` 等），demo/回放等确定性链路由测试用本地回环服务器承担）。
  opencode 工厂 `streaming` 缺省 **false**（该网关流式不稳定）；plugin-llm 配置支持
  `${ENV_VAR}` 展开（`api_key: "${OPENCODE_API_KEY}"`，缺失 fail loud），密钥不进文件。
  个人 `cordis.yml`（含真实密钥）入 .gitignore。
- **agent 驱动可替换（"一切皆插件"与"首先是 agent"的平衡）**：turn/step 主循环不再是
  宿主的硬编码依赖——`cos_agent::agent_factory!`（inventory 静态收集，同 `llm_factory!`
  构型）注册驱动器工厂；`AgentRegistry::set_driver(id, config)` 按 id 选择并构建活动工厂
  （`set_factory` 保留为程序化入口）。cos-agent-loop 注册默认驱动 `"loop"`；宿主
  `--agent-driver <id>`（或 `COS_AGENT_DRIVER`）选择。**关键组件纪律**：agent 驱动与 LLM
  同属装配期关键组件，缺失/未知 → 报警退出（错误列出可用清单与接入方式），不静默降级；
  可选能力（记忆/RPC 等）缺失不影响启动。
- **Provider 默认配置下沉（"少填字段"）**：`LlmRegistry` 工厂槽位增加 `defaults`——
  `register_factory_with_defaults(kind, build, defaults)` 注册，`build(kind, config)` 时
  浅合并（条目 config 覆盖默认）。plugin-opencode 据此把**套餐端点**下沉为默认
  `base_url`（`config.plan: go|zen`，`opencode.ai/zen/go/v1` / `opencode.ai/zen/v1`；
  `base_url` 可显式覆盖）——provider 条目只需填 `model`/`api_key` 等差异字段。
  `${ENV_VAR}` 展开上移为 `cos_llm::expand_env`（plugin-llm 与 Provider 插件的 defaults
  共用）。
- **模型目录（Provider 内置可用模型清单）**：`LlmRegistry::register_factory_with_catalog`
  在 `defaults` 之上增加**模型级**默认——`ModelDefaults { model, group, defaults }` 目录按
  `config.model` 命中，`build` 三级浅合并（插件级 < 模型级 < 条目）。plugin-opencode 内置
  `BUILTIN_MODELS`（go/zen 套餐模型清单**不一致**：`deepseek-v4-flash` → go 端点、
  `deepseek-v4-flash-free` → zen 端点；每模型独立声明 `base_url`/`api_style`/`streaming`/
  `max_tokens`——同一 Provider 下各模型端点与 api 风格可不同），`config.models` 追加/覆盖
  （同名后者生效，BTreeMap 收集时覆盖）；`api_style` 字段为适配器扩展点（后落地
  openai/anthropic/responses 三种内置风格，见"api style 分发"条目）。custom-provider
  同样支持 `config.models`。
- **provider 条目按插件引用（去掉 kind 重复）**：`LlmRegistry` 增加 `provider_plugin!`
  静态映射（yml 插件名 → kind，inventory 收集，`kind_of_plugin`/`plugin_names` 查询）；
  plugin-llm 的 provider 条目改 `{ id, plugin: <插件名>, config: { model } }`——kind 不再
  需要用户写（插件名即语义：`opencode-provider`/`custom-provider`），`model` 必须命中该
  插件模型目录（fail loud 列出可用模型，`available_models` 查询；目录为空即插件未 apply
  时跳过校验交 build 报错）。`kind:` 字段保留为向后兼容（模型未命中目录时回落插件级
  默认）；两者互斥（同时给出 fail loud）。id 由用户自取（`main`/`zen-free`），语义自明。
- **模型列表由 Provider 插件代码维护（"无需在配置里逐个添加模型"）**：模型目录
  （`BUILTIN_MODELS` + `config.models` 扩展）是 Provider 插件的固有部分，公开查询面
  `get_available_models` = 运行时 `LlmRegistry::available_models(kind)` + 插件 crate 的
  `available_models()` 纯函数。plugin-llm 的 provider 条目**省略 model/models = 插件目录
  全量展开**（聚合 Provider 一行声明；每个模型注册 `<id>.<model>`，条目 id 成为组链，
  chains 引用组自动展开）；`model`/`models` 仅作显式裁剪（须命中目录，fail loud 列出）。
  配置量从"每条模型两三行"降到"每个聚合组一行"。
- **目录按套餐分组（go/zen 分开）**：`ModelDefaults.group` 给每个模型打分组标签——
  目录可整体展开，也可**按组选择**：provider 条目 `group: <组>`（如 `{ id: go, plugin:
  opencode-provider, group: go }`）只展开该组的模型（组间不串，各套餐的模型/端点/预算
  互不污染）；未知组 fail loud 列出可用分组。查询面新增 `available_groups(kind)` /
  `models_in_group(kind, group)`（与 `available_models` 同构，插件 crate 层有对应纯函数）。
  分组与模型清单一样**由 Provider 插件代码维护**（`group:` 无需在配置里定义，只做选择）。
- **custom-provider 插件（纯配置自定义 vs 代码级自定义）**：新增 `plugins/plugin-custom-provider`
  （yml 工厂名 `custom-provider`）注册 `kind: custom`——复用 OpenAI 兼容 `chat/completions`
  适配器（cos-llm 的 `adapters` feature），`config.defaults` 可下沉公共字段（含 `${ENV_VAR}` 展开）。
  覆盖"任意 OpenAI 兼容端点、不想写代码"的场景；"新适配器协议"仍走代码级路径
  （适配器 crate + 封装插件，plugin-opencode 范本）。两条路径并列，插件树其余部分零改动。
- **deepseek-provider 插件（官方 API 也是一个 Provider 插件）**：新增 `plugins/plugin-deepseek`
  （yml 工厂名 `deepseek-provider`）注册 `kind: deepseek`——DeepSeek 官方 API
  （`api.deepseek.com`，OpenAI 兼容）同样复用 cos-llm 的 `adapters` feature 适配器（流式稳定 +
  `reasoning_content` → Thinking 块）。内置模型目录**无分组**（官方就
  `deepseek-v4-flash` / `deepseek-v4-pro` 两个模型，无需套餐/家族分组——`group:` 在此
  Provider 上报错并列出可用模型）；插件级 `api_key`（`${ENV_VAR}` 展开）、
  `config.models` 扩展目录与 opencode/custom 一致。适配器 id 沿复用实现为 "openai"
  （kind 才是 "deepseek"）——仅影响 list() 展示。**内置 Provider 的接入成本 = 一个新
  封装插件 crate + 一个锚点条目**（`src/plugins.rs::builtin_plugin_ids`），宿主与
  plugin-llm 零改动——插件化装配的兑现。
- **适配器并入 cos-llm（feature 门控；原 cos-llm-openai crate 删除）**：仅有一个真实
  适配器（OpenAI 兼容）时，独立 crate 的边际价值低于"少一个目录"的直观性——把
  `build_openai`/`OpenAiAdapter` 移入 `cos-llm/src/openai.rs`，由 **`adapters` feature**
  （默认关，随 feature 引入 reqwest/tokio）门控；三个 Provider 封装插件与
  cos-test-support 开启该 feature。**代价（记录在案）**：feature 在 workspace 叠加
  生效，宿主二进制实际总是带上 reqwest+TLS（名义 lean）；接缝 crate 从此"默认零网络
  依赖，开 feature 背实现"。若未来出现第二个协议适配器（Anthropic/Gemini 等），
  独立 crate 模板仍是首选（`cos-llm` 的 openai 模块与其并列，或按需再拆出）。
- **dsh 依赖驱动加载的对齐分析（决策：暂只记录，不改代码）**：dsh 的加载模型 =
  依赖驱动（`inject` 的插件等待必需服务就绪）+ **响应式生命周期**（依赖服务消失 →
  ACTIVE→DISPOSED，恢复后自动重载）。对照 cos：
  - **静态一半已一致**：inject/provide 建图 + 拓扑排序 = "依赖就绪才激活"；缺依赖/
    环/重复 provide 全部装载期 fail loud（dsh 的静态语义等价）。
  - **差距①宿主服务不可 inject**：loader 的 provider 表只收插件 `provide()` 名单，
    宿主装配的服务（tools/llm/shell/agents/invariants/bridges/rpc-providers/
    system-prompt）不可见——插件声明 `inject: ['tools']` 会 MissingDependency
    （"宿主服务边界"决策的代价）。低成本修复（暂缓）：loader 预置宿主服务名单，
    命中即视为恒就绪（依赖显式化 + 宿主变更早失败）。
  - **差距②无响应式生命周期**：cos 是启动时一次性装配（cordis.yml 静态清单），无
    运行时插件热插拔 → 重载没有触发源。基础设施半现成（Fiber/EffectHandle RAII 卸载
    支持运行时销毁），缺运行时插件管理（load/unload 入口）、服务变更事件、re-apply
    循环——属"运行时插件管理"阶段的大功能。
  - **关键洞察：inject 表达不了 provider 聚合副作用**——plugin-llm 需要的不是
    "LlmRegistry 服务存在"（宿主提供、恒就绪）而是"Provider 插件已 apply（工厂已
    注册）"；dsh 对此的兜底正是响应式重载（新 provider 出现 → llm 重新 apply）。
    静态 loader 下该隐含顺序无法用服务图表达，**这就是 tier（插件类型优先级）存在的
    理由**——dsh 的响应式重载可以取代 tier，但代价是整套运行时插件管理。
  - **未来路径（记录，不承诺）**：进入运行时插件管理阶段时——先做宿主服务可
    inject（小步）；再评估服务变更事件 + 自动重载（届时 tier 降级为纯文档性软约束，
    硬顺序由重载语义接管）。
- **对齐 dsh 的 LLM 边界协议（LlmError 稳定码 + finish 分片）**：对比
  `docs/develop/practice/llm-adapter`（dsh 把适配器注册进 `llm` 服务的模型）后落地
  两项（架构仍保持"装配期目录校验 + 静态 chains"，不引入请求级模型路由）：
  - **`LlmError` 结构化**：`Failure(String)` → `{ code: LlmErrorCode, message,
    facts?: ProviderFacts }`（对齐 dsh 的 LlmError code/provider facts）。码分类：
    InvalidRequest/Auth/Quota/RateLimit/Server/Network/Protocol/NotFound/Other；
    facts 带 HTTP status（429 可扩展 retryAfterMs）。**`is_retryable()`**（Server/
    RateLimit/Network/Other 可重试）同时驱动两处兜底：适配器内流式→非流式重试、
    **FallbackAdapter 切换决策**（4xx 鉴权/配额/参数/协议/路由不切换——切下一个也
    会同样失败，原样传播；5xx/限流/网络才切）。
  - **`ChunkDelta::Finish { reason }`**（Stop/ToolCalls）：适配器流尾显式发出终结
    分片（对齐 dsh 的 finish chunk）——agent loop 的"是否执行工具轮"决策显式化
    （仍以 ToolUse 块为准，finish 兜底空工具块防死循环）；**控制分片不入会话日志**
    （JSONL/事件计数不变）、RPC 转发跳过。脚本化 mock 不发 finish → 消费方按 Stop
    兜底（协议向后兼容）。
  - 未采纳（记录）：dsh 的请求级模型路由（`GenerateOptions.provider/model` +
    `resolveModel` 运行时解析）与响应式适配器注册（disposer/HMR）——与"装配期
    fail loud + 静态装配"哲学冲突，已在"dsh 依赖驱动加载的对齐分析"条目内记录。
- **api style 分发（"把 api style 适配交给插件"）**：`LlmRegistry`/`cos-llm` 增加
  **风格注册表**（`cos_llm::register_api_style(style, build)`，全局；`api_styles()`
  查询）与**分发构建函数** [`cos_llm::build_with_style`]——Provider 插件注册 kind 时
  用它作 `build`，合并配置里的 `api_style` 字段决定走哪个风格构建器（缺省 `"openai"`，
  未知风格 fail loud 列出已注册）。内置风格随 `adapters` feature 预注册：
  - `openai`（`/chat/completions`，原有）；
  - `anthropic`（`/messages`：system 顶层、tool_result 在 user 角色、SSE
    content_block_* 事件、error.type 分类）；
  - `responses`（`/responses`：instructions 顶层、input_text/function_call 块、
    output_text.delta/function_call_arguments.delta 事件）。
  三者共用错误码分类与"可重试失败 → 非流式兜底"语义。**模型目录按模型声明风格**
  （opencode-go 官方文档：GLM/Kimi/DeepSeek/MiMo/Hy3 → openai、MiniMax/Qwen →
  anthropic、Grok/GPT → responses——同一 base_url，路径由风格决定）；custom-provider
  条目/默认也可声明 `api_style`。**第三方风格适配器插件** = 新 crate 实现
  `fn(&Value) -> Result<Arc<dyn LlmAdapter>, LlmError>` + apply 里
  `register_api_style` 注册，任何 Provider 的目录条目即可选用。
- **模型清单拉取（`fetch_models`）**：`cos_llm::fetch_models(endpoint, api_style)`
  （阻塞式，opt-in）GET Provider 端点（如 `https://opencode.ai/zen/go/v1/models`），
  容忍常见响应形状（字符串数组 / `{data|models: [...]}` / `{data: [{id}]}`），映射为
  目录条目并入（内置 < 拉取 < `config.models` 显式覆盖）；网络失败 fail loud。
  plugin-opencode 配置 `models_endpoint` + `models_api_style` 启用——模型清单可
  **从 Provider 拉取而非只靠代码维护**，两类来源并存（dsh `listModels`/`discoverModels`
  的静态等价物）。

## B 形态清单一等公民（P10，dlopen）

（对应 `docs/b-abi.md` §5 / §10.6；实现 `crates/cos-loader/src/dlopen.rs` + `compose.rs`。）

- **动机**：P8/P9 的 dlopen 插件是"二等公民"——不参与 loader 依赖图（无法声明
  inject/provide），`get_service` 也不裁剪（任何服务按名可取）。这与 A 形态
  "声明式依赖 + 能力最小化"的哲学不一致；B 形态清单一等公民是生态开放的前提
  （用户优先序③"先处理 leak_cstr/host_free 再谈生态开放"）。
- **清单符号 `cos_plugin_manifest`**（`cos-contract`，`API_VERSION` 0.3.0 → 0.4.0）：
  B 形态插件导出 `fn() -> *const c_char`，返回 NUL 结尾 JSON
  （`{id?, version?, api?, inject?: [...], provide?: [...]}`）。**缺失符号 = 空清单 =
  旧行为**（不参与依赖图、不裁剪能力）——向后兼容，旧 cdylib 照常装载。
- **依赖图参与**：loader 把清单 `inject`/`provide` 并入 `compose::plan` 的拓扑排序
  （与 A 形态同等约束：缺依赖/环/重复 provide fail loud）；**dlopen 的 inject 命中
  宿主服务（无插件 provider）不构成 MissingDependency**——宿主桥服务（tools/llm 等）
  对 B 形态视为恒就绪（A 形态"宿主服务边界"决策不变，两侧语义差记录在案）。
- **能力裁剪**：`get_service` 只对清单 `inject` 声明的服务返回句柄，未注入的服务
  返回空指针（插件侧 fail loud）——"最小能力"跨 A/B 形态一致；事件/工具注册等
  通用能力不受裁剪影响（清单只管服务面）。
- **装配可见**：`--dump-config` 输出 dlopen 条目的 `inject`/`provide`（与装载共用
  plan 路径，输出 = 装载）。
- **试点更新**：`plugin-todo-dlopen` 导出清单（`inject: ["todo-store"]`）——e2e
  （`tests/dlopen_e2e.rs`）断言依赖图生效（todo 先于 dlopen apply、卸载逆序
  dlopen 先于 todo）、dump 含清单；工具改名 `dlopen_todo`（与 A 形态 `todo_write`
  区分）。
- **P10 未做（记录在案，见 b-abi.md §10.6）**：`cos_plugin_validate` 入口；服务句柄
  按名缓存与跨插件复用优化；验签/哈希校验、沙箱、运行时重载（PLAN §0 非目标）。

## B 形态资源生命周期（P11，leak_cstr / host_free）

（对应 `docs/b-abi.md` §10.7；实现 `crates/cos-loader/src/dlopen.rs` + `cos-tools`。
生态开放前置——用户优先序③"先处理 leak_cstr/host_free 再谈生态开放"。）

- **动机**：P8 的两项简化是生态开放的拦路虎——`leak_cstr` 每次调用 `Box::leak`
  一份（`emit` 循环高频事件名无限增长，卸载/重载不回收）；`free` 空操作（句柄
  API 名不副实，插件无法提前撤销资源）。B 形态对第三方开放前必须处理干净。
- **字符串驻留区**：`PluginHostState.strings`（`Mutex<Vec<Box<str>>>`）**去重驻留**
  跨边界 `&'static str`（事件名/工具名/描述——`EventName` 与 `Tool` trait 都要求
  `'static`），随插件状态 drop（卸载）释放。**安全不变量 = 字段顺序**：`ctx`
  （首字段，drop 时 `Fiber::Drop` 逆序注销监听器与工具注销效果）必须先于
  `strings`（末字段）drop——消费者引用消失后才释放字符串；apply 失败路径
  **保留状态到实例 drop**（不再提前置 None：失败前注册的监听器/工具持有驻留
  字符串，提前释放会悬垂）。
- **`free` 诚实回收**：`handles: HashMap<Handle, HandleKind>`（`Listener/Effect/Tool`
  各持 [`EffectHandle`] 克隆）——`free` 按句柄分发：监听/效果提前 dispose、工具
  注销；未知/外来/重复/0 句柄 = 幂等无操作（跨插件句柄天然隔离：注册表按状态）。
  与 fiber 卸载路径幂等共存（dispose 幂等，fiber 克隆随后无操作）。
- **工具自动注销（无僵尸工具）**：`host_register_tool` 成功后把**注销效果**挂到
  插件 fiber——卸载/回滚自动 `ToolRegistry::unregister`（新增）；修复旧缺陷：
  卸载后工具仍留注册表（执行回调指向已卸载库 = 潜在 UAF），且重载同名工具
  duplicate 失败。tools 服务缺失/同名重复 → 返回 0（fail loud，插件侧检查）。
- **空指针守卫**：`emit` 载荷 / `register_tool` 参数 JSON 空指针安全处理；fn 指针
  （callback/disposer/execute）Rust 保证非空（`useless_ptr_null_checks`），不设守卫。
- **插件侧零泄漏**：`plugin-todo-dlopen` 的 `Box::leak` `cstr()` 辅助改为 C 字符串
  字面量（`c"..."` / `cr#"..."#`）。
- **验证**：cos-loader 单测 7 项（free 提前注销监听/效果、外来句柄幂等、free 注销
  工具、fiber dispose 注销工具 + 重载再注册、字符串去重驻留、空指针安全、
  效果提前 dispose）；e2e 全链路断言不变。

## B 形态暴露面审计（P12）

（对应 `docs/b-abi.md` §12；用户优先序④"审计 B 形态其余对外暴露面"。生态开放前
的收尾审计：逐面核对"契约声明 vs 实现"，修复三处空转/缺陷，其余记录在案。）

- **`cos_plugin_validate` 死契约 → 兑现**：契约声明了导出符号与签名（b-abi §4、
  third-party-dev §5.2 均列出），但 loader 从不查找/调用——第三方实现了也是静默
  无操作。修复：`DlopenPlugin` 解析可选符号，apply **之前**调用（配置 JSON + 错误
  缓冲）；非零返回 → `LoadError::DlopenValidate`（fail loud，插件写的错误文本透出）；
  缺失 = 跳过（向后兼容）。薄壳导出 `cos_plugin_validate`（非对象配置 →
  `ConfigInvalid`），e2e `dlopen_validate_rejects_bad_config` 断言启动失败 + 错误文本。
- **清单 `api` 字段声明不兑现 → 强制**：`PluginManifest::api_version()` 只在测试里
  存在，装载路径从不校验。修复：`check_manifest_api`（load 时调用）——解析出版本
  且不兼容 → `AbiMismatch`（fail loud，可读错误）；缺省/非法字符串 = 按当前版本
  对待（契约 docstring 语义）。单测 `manifest_api_field_is_enforced` 覆盖
  缺省/一致/旧版/不兼容/非法五种。
- **`ErrorCode::from_i32` 缺 `CallFailed`(7)**：`service_call` 返回 7 但映射落
  `_ => None`，b-abi §8 错误码表同样漏 7（文档漂移）。修复：补 `7 => CallFailed` +
  往返测试 + §8 文档。
- **RAII 卸载顺序潜伏缺陷（P11 起，审计实测 0xC0000005）**：`DlopenPlugin` 字段序
  `_library` 先于 `state`——显式卸载路径（`LoadedApp::dispose`/`dispose_async`）
  先 fiber 逆序注销再 drop 实例，顺序正确；但**纯 RAII 路径**（装载后装配失败
  错误返回、未调 finish 直接 drop）库先卸载、`state.ctx` 的 `Fiber::Drop` 随后才
  逆序执行插件 disposer → **执行已卸载库的代码** → 访问违例（写违例弹窗实测）。
  e2e 从未触发（总走 `finish` 的显式 dispose）。修复：`impl Drop for DlopenPlugin`
  在字段 drop（库卸载）前兜底 dispose 状态 fiber（幂等，显式路径不受影响）；
  回归 e2e `dlopen_raii_unload_without_llm_fails_cleanly`（无 LLM → 干净失败 +
  marker 文件证明 disposer 在库卸载前已执行）。P11-era 旧 cdylib（无 validate）
  向后兼容装载验证通过。
- **审计矩阵其余项（记录在案）**：HostApi 字符串参数空指针守卫 P11 已全；fn 指针
  Rust 保证非空（不设守卫）；error_buf/result_buf 越界写 = 无沙箱下的固有风险
  （非目标，契约已声明"不扩容"）；**B 提供服务给 A 形态是已知差距**（`provide`
  仅声明性，B 无法注册 JsonBridge/类型化服务——未来可加宿主函数 `register_bridge`）；
  热重载非目标（PLAN §0）。

## cordis.yml patch 层叠（P13，P2 修订）

（实现 `crates/cos-loader/src/profile.rs` + `--patch` CLI；语义参考 dsh 的
`cordis.patch.yml` 体系——`packages/boot/app-boot/src/profile.ts` +
`vendor/include/src/index.ts` 的 `applyEntryPatches`，静态装载变体。）

- **动机（用户拍板）**：B 形态插件作为第三方插件，应经 `cordis.patch.yml` 与主
  `cordis.yml` 组装成完整插件列表——主 yml 不再包含所有插件，**完整列表需要在
  其他地方列出**。dsh 的做法：profile `package.json` 的 `dsh.profile.bundles`
  名单 + 各层 patch 文件组合；权威输出 = `--dump-config`（与装载共用
  `applyEntryPatches` 同一条路径，"dump 永不偏离装载"）。
- **主 yml 双形态**：顶层数组（v1 兼容，等价对象形态省略包装）或对象
  `{ patch?: [路径], entries?: [条目] }`——`patch:` 声明相对主 yml 目录解析，
  主 yml 保持"完整清单声明处"心智（条目 + patch 引用 = 完整集合）。
- **层叠顺序**（后覆盖先，按 id/name 定位）：主 yml 条目 → 主 yml `patch:` 声明
  文件（按序）→ 同目录 `cordis.patch.yml`（**自动应用**；被显式声明时去重防双应用）
  → CLI `--patch <file>`（可多次，按 argv 顺序，相对 cwd）。
- **patch 语义**（对标 dsh，**fail loud 变体**）：`{ id, config }` 覆盖配置、
  `{ id, disabled }` 禁用、`{ id, name }` 名称校验（只校验不覆盖，同 dsh）、
  `{ insert: [...] }` 追加（无 id = 列表尾；**带 id = v1 无 group，fail loud**）；
  后 patch 可定位先 patch insert 的条目（同层增量索引）。**定位不到目标 / 名称
  不匹配 → 启动失败**（错误列出可用条目 id）——dsh warn+skip 是热重载的妥协
  （patch 文件热改、临时缺行可容忍），cos 静态装载没有这个理由，fail loud 与
  "零配置直接启动失败"哲学一致（用户拍板）。
- **来源标注**：Entry 增加 `source`（`cordis.yml` / patch 文件路径，insert 条目标
  注注入来源）；`--dump-config` 每条目输出 `source`——**完整列表的权威处**。
- **第三方交付形态**：第三方包 = cdylib + 自带 `cordis.patch.yml`（insert 自己的
  dlopen 条目）；用户经主 yml `patch:` 或 CLI `--patch` 启用（详见
  third-party-dev.md §5.4.1）。
- **验证**：profile 单测 6 项（双形态/覆盖/insert/定位失败/名称校验/增量索引/
  patch 形状）+ 集成测试 4 项（层叠顺序、自动应用去重、第三方 insert、缺失文件
  fail loud）+ e2e `dlopen_patch_injects_third_party_plugin`（第三方 patch 注入
  dlopen 条目 → dump 完整列表 + source 标注 + 清单参与依赖图）。
