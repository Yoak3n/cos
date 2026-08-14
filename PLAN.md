<!-- 编辑注（非计划原文）：本仓库（E:\Project\RustProject\cos，当前 crate 名 cos）即本计划的 "dsh-rust/" 工作区根，§2 的目录树直接映射到本仓库根；JS 语义权威参考位于 E:\GitVault\deepseek-harness。 -->

# dsh-rust 实施计划（A 形态主干 → B 形态生态）

## 0. 目标与非目标

**目标**：用 Rust 复刻 dsh 的插件化主干 —— "一切皆插件"的语义（Context 服务仓库、inject/provide 依赖、事件扩展、可逆效果、会话日志为唯一事实源、turn/step 主循环），启动/重启时由 cordis.yml 装配插件树。

**非目标**（明确不做，避免 agent 过度设计）：

- 运行时可插拔 / HMR / 配置热重载（只支持启动或重启生效）
- agent 自写插件即时挂载（无 vm 沙箱）
- 声明合并式开放联合类型（事件用运行时开放策略，见决策 D1）
- 多进程、跨平台 GUI（headless CLI 即可）

## 1. 语义继承清单（从 dsh 借什么）

| dsh 语义 | 保留方式 |
| --- | --- |
| 插件 = inject/provide/Config/apply | trait Plugin（见 §3） |
| 注册即效果、卸载逆序回滚 | RAII：Fiber 持有 disposer 列表，Drop 逆序执行 |
| 依赖就绪才激活 | 简化为拓扑排序（无运行时替换，不需要响应式重载） |
| 事件 5 分发模式 | emit/parallel/serial/bail/waterfall 五方法 |
| 会话日志唯一事实源、模型可见 ⟺ 已记录 | SessionEvent 追加日志 + derive_messages()，同款 invariant |
| 能力接缝三件套（Definition/Provider/Consumer） | 每能力 = dsh-<cap> 定义 trait + dsh-<cap>-local 实现 + plugin-tool-* 消费 |
| turn/step 主循环、pre-step/request waterfall | agent-loop 同款状态机，默认行为作最内层 next |
| cordis.yml → patch 层叠组合 | Entry {id, name, config, inject, disabled}，v1 不做多 bundle 层叠（见 P2） |

参考映射（JS 仓库是语义权威，agent 实现时对照读）：

| Rust crate | JS 参考文件 |
| --- | --- |
| dsh-core | vendor/cordis/src/{context,registry,fiber,events,reflect}.ts、packages/core/scope/src/index.ts |
| dsh-loader | vendor/loader/src/config/{entry,tree,group}.ts、packages/boot/app-boot/src/profile.ts |
| dsh-session | packages/core/session/src/ |
| dsh-agent / dsh-agent-loop | packages/core/agent/src/types.ts、packages/core/agent-loop/src/{agent,index,tool-calls}.ts |
| dsh-tools | packages/core/tools/src/、docs/tool-execution-pipeline.md |
| dsh-system-prompt | packages/core/system-prompt/src/ |

## 2. 工作区布局

```text
dsh-rust/  (Cargo workspace)
├── crates/
│   ├── dsh-core/          # Context、服务注册表、事件总线、Plugin trait、Fiber/effect、scope
│   ├── dsh-loader/        # cordis.yml 解析 → 工厂解析 → 拓扑排序 → 挂载
│   ├── dsh-session/       # SessionEvent 追加日志、derive_messages、JSONL 持久化
│   ├── dsh-llm/           # LLM 接缝：Message/ContentBlock、LlmAdapter trait、stream
│   ├── dsh-llm-mock/      # 确定性脚本化 mock 适配器（测试与回放）
│   ├── dsh-tools/         # 工具注册表 + 执行管线（pre/guards/execute/post）
│   ├── dsh-system-prompt/ # prompt 段 + 工具 schema 装配
│   ├── dsh-agent/         # Agent trait、注册表、Inbox（接缝）
│   ├── dsh-agent-loop/    # turn/step 驱动器（实现 dsh-agent）
│   ├── dsh-shell/         # shell 接缝 + local 实现 + tool-bash（可推迟到 P6）
│   └── dsh-contract/      # (P7 起) 版本化契约 crate —— B 形态的地基
├── plugins/               # A 形态插件 = workspace crate（独立 crate 才证明接缝纪律）
│   ├── plugin-todo/       # todo_write
│   ├── plugin-bash/       # bash 工具
│   └── plugin-demo/       # echo 类演示工具（供 e2e）
└── app/                   # dsh 可执行：读 profile/cordis.yml → 启动
```

依赖方向铁律（照抄 dsh）：plugins/* 和 dsh-agent-loop 只能依赖各接缝的 Definition crate（dsh-llm/dsh-tools/dsh-agent/dsh-shell），不得依赖具体 Provider 或 dsh-agent-loop 本身；dsh-core 不依赖任何上层 crate。

## 3. 核心契约（签名级，agent 按此实现）

```rust
// dsh-core —— 插件
pub trait Plugin: Send + Sync {
    const ID: &'static str;
    type Config: serde::de::DeserializeOwned + Validate;  // serde + 校验（决策 D3）
    fn inject(&self) -> &[&'static str];        // 依赖的服务名（无则空）
    fn provide(&self) -> &[&'static str];       // 提供的服务名（无则空）
    fn apply(&self, ctx: &Context, cfg: &Self::Config) -> Result<(), CoreError>;
    // apply 内用 ctx.register_*() 注册服务/监听/工具，句柄进 ctx.fiber 的 disposer 列表
}

// 服务注册表：类型化 TypeMap（TypeId 键），同一名字只允许一个实现（同 dsh: 重复 provide 报错）
impl Context {
    pub fn provide<T: Service + 'static>(&self, value: T) -> EffectHandle;   // 卸载即反注册
    pub fn get<T: Service + 'static>(&self) -> Result<Arc<T>, CoreError>;   // 未就绪/未注册 → 报错
    pub fn fiber(&self) -> &Fiber;                // 当前插件实例
    pub fn on(&self, name: &'static str, listener: Listener) -> EffectHandle;
    pub fn emit(&self, name: &'static str, payload: Arc<dyn Any + Send + Sync>);
    pub fn serial(...) / bail(...) / parallel(...) / waterfall(...);        // 五种分发
}

// Fiber：每插件实例一个；Drop 时逆序执行 disposers（同 dsh fiber._unload）
pub struct Fiber { disposers: Vec<EffectHandle>, ... }

// 事件名 + 载荷：运行时开放（决策 D1）
pub type EventName = &'static str;
pub type Payload = Arc<dyn Any + Send + Sync>;

// dsh-session —— 封闭枚举 + Custom 逃生舱（决策 D4）
pub enum SessionEvent {
    TurnStart { turn: u32 }, TurnEnd { turn: u32, reason: TurnEndReason },
    StepStart { turn: u32, step: u32 }, StepEnd { turn: u32, step: u32 },
    UserMessage(UserMessage), AssistantChunk { turn, step, chunk },
    AssistantMessage { turn, step, message, usage? },
    ToolCall { turn, step, call_id, name, arguments },
    ToolResult { turn, step, call_id, message, error? },
    RequestHeader { header }, RequestContext { ctx },
    Custom { name: EventName, data: serde_json::Value },   // 第三方模型的逃生舱
}
pub fn derive_messages(&self) -> Vec<Message>;  // 从日志投影，同 dsh

// dsh-agent（接缝）—— 同 dsh 的 Agent 句柄
pub trait Agent: Send + Sync {
    fn id(&self) -> &SessionId;
    fn send(&self, msg: UserMessage, target: InboxTarget, wake: bool);
    fn followup / steer / inject(...);
    fn cancel(&self, cause: CancelCause, keep_inbox: bool);
    fn when_idle(&self) -> impl Future;  fn run_maintenance(...);
}
// dsh-agent-loop：实现该 trait，注册为 ctx.agents 的工厂

// dsh-tools —— 执行管线
pub struct ToolDefinition { name, description, parameters: JsonSchema, execute: Box<dyn Fn(ToolRunCtx, Value) -> ...> }
// 管线顺序（同 docs/tool-execution-pipeline.md）：
// tools/pre-execute(waterfall) → 单调守卫 → approval(可空实现) → tools/execute(waterfall)
// → 工具体 → tools/post-execute(waterfall) → tool/result 事件
```

## 4. 关键设计决策（先拍板，agent 不得自行改向）

| # | 决策 | 选项 | 推荐 |
| --- | --- | --- | --- |
| D1 | 事件类型化 | a) 运行时开放：name + Arc<dyn Any> + 监听器 downcast；b) linkme 收集全工作区事件变体生成编译期枚举 | a。b 的编译期穷尽性收益小、实现复杂度高；a 与 B 形态的 FFI 天然兼容 |
| D2 | 异步运行时 | tokio / async-std / 无异步 | tokio。stream 与工具并发需要；async-trait 处理 trait 方法 |
| D3 | 配置校验 | serde + 手写校验 / schemars(生成 JSON Schema) | serde + schemars。生成的 schema 到 B 形态直接作 wire 契约 |
| D4 | 会话事件可扩展性 | 封闭枚举 / 封闭 + Custom 逃生舱 | 封闭 + Custom。第三方程插件若要新增模型可见输入，走 Custom + 插件自己的投影；derive_messages 对 Custom 原样透传 |
| D5 | 错误类型 | 每 crate 具体枚举 / anyhow | 边界用具体枚举（CoreError/SessionError…），插件内部实现可 anyhow。对应 dsh "边界类型化" 纪律 |
| D6 | scope 是否现在就做 | 先单 agent / 现在就做事件按 scope 路由 | 现在就做（dsh-scope 那套 ScopeKey + 父链 + 事件向上流），子 agent/subagent 是主干的一部分 |
| D7 | 效果卸载语义 | 仅 Drop / Drop + 显式 async 卸载 | Drop 触发同步反注册 + 返回可 await 句柄；async disposer 由 fiber 统一收集等待 |

## 5. 阶段路线（每阶段给验收标准）

### P0 脚手架与契约定型（S）

workspace + 各 crate 空壳 + dsh-core 内 Plugin/Context/Fiber/EventBus trait 初稿 + 决策文档 docs/decisions.md
CI：cargo test、clippy、rustfmt、cargo deny（license/依赖审计）

✅ 验收：一个 hello 插件能 apply 并在 Context 上 provide 一个服务、get 取回

### P1 dsh-core 完成（M）

Context（TypeMap + provide/get + 同名单冲突报错）、Fiber（disposer 逆序回滚）、EventBus（emit/parallel/serial/bail/waterfall，waterfall 的 next() 委托语义照 dsh：不调 next() 即短路）、scope（ScopeKey/scopeTarget/父链，事件按 scope 过滤）

✅ 验收：单测覆盖 5 种分发模式、waterfall 短路、fiber 卸载逆序执行、scope 事件路由；ctx.on 在 fiber 卸载后自动失效

### P2 dsh-loader（M）

YAML 解析（serde_yaml）→ Vec<Entry>；resolve_factory(name) 查静态注册表（plugin! 宏 + inventory 或手写注册）；配置校验；inject/provide 建图 → 拓扑排序（环/缺依赖 → 启动即报错，带 plugin 名）；按序 apply
v1 不做 bundle/patch 层叠：cordis.yml 就是单一清单（保留 disabled、config 字段）

✅ 验收：loader-composition 集成测试 —— 读真实 yaml 装配 3-4 个插件，断言服务存在、agent-loop 等依赖在 llm 之后激活、环配置报错信息可读

### P3 会话日志 + LLM 接缝 + mock（M）

dsh-session：SessionEvent 追加日志、seq、derive_messages()、JSONL 持久化（session/flush 同款 checkpoint 可后置）
dsh-llm：Message/ContentBlock、LlmAdapter trait（stream(request) -> Stream<Chunk>）、dsh-llm-mock：脚本化确定性回复（按输入哈希或序号选择预设输出）

✅ 验收：derive_messages 投影正确性单测；mock 适配器可流式产出 chunk；持久化 → 重载 → 日志逐字节一致

### P4 agent + agent-loop（L，主干核心）

dsh-agent：Agent trait、注册表、Inbox（next_turn/next_step）、with_initiator（task_local 因果链）
dsh-agent-loop：turn/step 状态机照 agent.ts —— wake_driver → kick → turn → pre_step(waterfall) → step(build_request → stream → assistant 事件 → 工具) → turn_end；每步先写日志再行动

✅ 验收：端到端单轮对话：mock LLM 回复一条 assistant 消息 → 会话日志包含 turn/start, user/message, assistant/chunk*, assistant/message, turn/end；derive_messages 重放与原始一致（快照测试）

### P5 tools 管线 + system-prompt（M）

dsh-tools：注册表 + pre-execute/execute/post-execute waterfall + tool/call、tool/result 事件先写日志再执行；dsh-system-prompt：prompt 段装配 + 工具 schema 收集
第一个真实工具：plugin-todo（todo_write，同 dsh 的 session 态清单）

✅ 验收：模型回一条带 tool-call 的消息 → 工具执行 → 结果事件回流 → 下一 step 的 derive_messages 含完整 call/result 对；装配出的 prompt 文本快照测试

### P6 A 形态收口（M）

app/ 可执行：dsh --config cordis.yml 启动、--dump-config、优雅退出（全 fiber 逆序卸载）
dsh-shell 接缝 + local + plugin-bash（v1 简化：前台执行、无后台 job、无 sandbox 后端）
不变量注册表（dsh-invariants，同 dsh ctx.invariants）：模型可见 ⟺ 已记录、事件 seq 单调等

✅ 验收：A 形态 DoD 全绿（见 §7）；dsh --dump-config 输出与装载一致；卸载顺序日志可审计

### P7 B 形态准备（契约冻结，M）

提炼 dsh-contract：版本化（semver）契约 crate；接缝 trait 做对象安全审计（消除公开泛型方法）；服务方法"小而窄"审计（参数/返回值可 JSON 序列化）
输出 docs/b-abi.md：HostApi 能力函数表草案（api_version、get_service/emit/on/register_effect/free、谁分配谁释放、能力按 inject 裁剪）、abi_stable vs 纯 C ABI 的最终选型

✅ 验收：dsh-contract 独立发布构建通过；所有接缝 trait 过 object_safe 检查；B-ABI 文档评审通过

### P8 B 形态试点（L）

DlopenPluginSource（libloading）接入 resolve_factory；薄壳 cdylib（转发到静态实现）验证 FFI 机制；移植 plugin-todo 为独立 cdylib

✅ 验收：cordis.yml 用 name: ./plugins/todo_v1.so 装载 todo 插件且行为与静态版一致；版本不匹配 → 启动拒绝并给出可读错误

### P9 B 形态生态（M，按需）

插件 SDK 脚手架（cargo 模板 + 示例 + 测试 harness）、验签/哈希校验、CI 平台矩阵（Windows/macOS/Linux 的符号可见性）、插件清单格式与版本协商

✅ 验收：独立仓库的第三方插件可在三平台装载并过能力门控

## 6. A 阶段就要为 B 做的（防止返工清单）

- resolve_factory 从第一天就是接口，不是静态查表内联（B 只加一个 source）
- 接缝 trait 一律对象安全、方法窄参数（P7 只是审计，不是重写）
- 配置序列化用 serde_json/schemars（到 B 就是 wire 格式）
- dsh-contract 版本号从 P0 就存在（即使只有内部使用）
- 不要在 A 阶段引入：运行时重载、事件编译期枚举、插件文件加载、任何 unsafe 跨界（留到 P8）

## 7. A 形态完成定义（DoD）

- cargo test 全绿；test:coverage 等价物（每 crate 关键路径覆盖，不强求 100%）
- 端到端：dsh --config examples/demo.yml 跑通"用户消息 → mock LLM → 工具调用 → 回复"，会话日志可重放
- 快照测试：确定性 LLM 回复下的完整会话转录与预期逐事件一致（对应 dsh 的 keyless snapshot）
- 卸载测试：Ctrl-C/退出时全插件逆序卸载，无泄漏、无 panic
- 不变量：模型可见 ⟺ 已记录 断言在 test/e2e 中触发并全过
- 文档：每 crate README + docs/decisions.md 记录 D1-D7 及理由

## 8. 风险与应对

| 风险 | 应对 |
| --- | --- |
| waterfall 的 next() 委托在 Rust 闭包所有权下难写 | 参照 fiber._reload 之外的做法：监听器签名为 Fn(&mut Decision) -> ControlFlow，默认行为作为链尾；P1 用单测钉死语义 |
| async Drop / 异步 disposer | 决策 D7：同步反注册 + fiber 持有可 await 句柄，卸载时 join_all |
| 循环依赖或缺失依赖难调试 | loader 报错带完整依赖路径（同 dsh "fail loud at load"）；P2 测试覆盖 |
| Arc<dyn Any> 事件载荷 downcast 失败 | 事件名 + 载荷类型注册时配对校验，dev 模式 panic、release 模式记日志跳过 |
| B 形态的 ABI 决策拖到 P8 才发现 A 的设计挡路 | P7 是硬里程碑，P0-P6 的接缝审计项已在 §6 前置 |
