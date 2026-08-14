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
- **cordis.yml 形态（P2）**：`Profile = Vec<Entry>`（cordis.patch.yml 顶层数组语义，v1 无层叠）；条目 `{id?, name, config?, inject?, disabled?}`，config 原位解析为 `serde_json::Value`（§6：B 形态 wire 格式）；错误风格照 JS `failed to apply loader entry <id> (<name>): detail`。装载失败靠 RAII 自动回滚（已 apply 的 fork Context 随栈展开 drop → fiber 逆序反注册）。
- **会话 wire 形状（P3）**：`SessionEvent` 信封 `{seq, time, type, data}`（flatten 的 tag/content 枚举）；事件名含 `/` 分隔符（`turn/start` 等）——serde 的 `rename_all` 只能做词法转换、会损毁 `/`，故逐变体显式 `rename`；版本钉 0、不兼容日志直接拒绝（同 dsh 无迁移语义）。serde 带 tag 枚举**不支持 newtype 变体**，`ContentBlock`/`ChunkDelta` 用 struct 变体（`Text { text }`）。
- **derive_messages 投影（P3）**：surface = user/message、assistant/message、tool/result 按 seq 序；`Custom` 原样透传为 `Message::Custom`（决策 D4）；chunk/边界/请求头/工具调用为 log-only，不参与投影。
- **LlmAdapter 接缝（P3）**：对象安全（同步方法 + boxed `LlmStream`）；usage 随末块携带（`StreamChunk.usage`），agent-loop（P4）装配进 assistant/message。
- **waterfall 载荷/返回值分离（P4，修订 P1 定型）**：`Decision<P, V>` —— 载荷 `P` 经 `set_value` 变换、链返回值 `V`；veto = 不调 `next()` 直接返回 `V`（ControlFlow 退役）。P1 的单类型 `Decision<T>` 无法表达 dsh 的真实用法（pre-step 载荷是消息、链返回决策）；P1 测试同步迁移。监听器按 `(TypeId::of::<P>, TypeId::of::<V>)` 类型对配对。
- **Inbox claim 语义（P4，照 dsh inbox.ts）**：总是先取光 next-step，`NextTurn` 目标再额外取 next-turn 队列的**恰好一条**——每个排队 turn 消费一条，同 turn 内可再消费 steering/注入。
- **Session 内部可变性（P4）**：`Session` 改为 `Arc<Mutex<Inner>>`（廉价 Clone）：Agent 句柄对外共享 `&Session` 只读视图，loop 以 `&self` 追加（写路径单写者）。
- **驱动器相位与因果链（P4，照 agent.ts/AgentRegistry）**：`Phase { Idle, Running{turn,step,aborted}, Maintenance{wake_requested} }`；wake 闩在维护收敛后重放；`Running` 状态事件在调用方任务发出（同 dsh），`Idle` 在驱动器任务内（with_initiator 边界内，发起者可见）。`run_maintenance` 用类型擦除的 `Maintenance`（FnOnce + AbortSignal），对象安全。
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

- **真实 LLM 适配器（cos-llm-opencode）**：OpenAI 兼容 `chat/completions`；`LlmAdapter::stream` 是同步方法 → 内部 `tokio::spawn` + unbounded channel 转发（调用方须在 runtime 内，同 mock 语义）。**流式优先、自动非流式兜底**：SSE 在未产出任何 chunk 前遇服务端失败（HTTP 5xx / `{"type":"error"}` 块）→ 重发 `stream:false` 单次请求，`choices[0].message.content`（空则 `reasoning_content`）+ usage 合成一个 chunk；4xx（鉴权/余额）不重试原样报错；已产出部分 chunk 后失败不再兜底（避免重复内容）。错误一律作为流内 `Err` 交付，不进 stderr。
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
  消费者按 id/链 id 解析。工厂经 `cos_llm::llm_factory!("kind", build_fn)`（inventory 静态收集，
  同 loader `plugin!` 模式，fn 指针 const 可构造）；新 provider = 新 Provider crate + 工厂注册，
  插件树零改动。Provider crate 只依赖 Definition（cos-llm），插件不得依赖 Provider（接缝纪律保持）。
- **后备链语义（FallbackAdapter）**：纯 futures unfold 状态机（无 spawn、cos-llm 不引 tokio）；
  主 provider 在产出任何 chunk 前失败（错误/空流）→ 记错误并切下一个；**已产出后失败 → 原样传播
  不切换**（避免内容重复）；全部未产出 → 交付最后错误。这正是 opencode 网关抖动/流式不稳定的通用解。
- **loader 宿主服务边界**：loader 的 inject 校验只认插件 provide 表，宿主装配的服务不可见——
  plugin-llm/plugin-memory 不声明 inject，靠 apply 时 `ctx.get` fail loud（同 plugin-todo）；
  无依赖边时拓扑排序保持配置顺序，故 **llm 条目须在 memory 之前**（文档化）。
- **记忆插件 LLM 解析**：`MemoryConfig.llm`（provider/链 id，缺省 "default"）→ 注册表解析；
  原 `MemoryLlmProvider` 服务删除（回归测试改注册表装配）。cos 无 `--llm-*` 时注册
  "default" = 空脚本 mock（记忆失败软降级不变）；`--agent-llm <id>` 指定主 agent 提供商/链，
  `--llm-*` 仍注册 "default" 快捷方式。
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
  （`tests/rpc_e2e.rs`，demo mock 确定性脚本）。
