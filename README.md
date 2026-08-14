# cos —— dsh-rust

Rust 复刻 dsh 的插件化主干："一切皆插件" —— Context 服务仓库、inject/provide 依赖、
事件扩展、可逆效果、会话日志唯一事实源、turn/step 主循环；启动时由 cordis.yml 装配插件树。

- 完整实施计划：`PLAN.md`
- 设计决策记录：`docs/decisions.md`
- 语义权威参考（JS 仓库）：`E:\GitVault\deepseek-harness`（`vendor/cordis`、`packages/core/*`）

## 布局

```text
crates/   # 核心与接缝 crate（dsh-core、dsh-loader、dsh-session、dsh-llm、dsh-llm-opencode、dsh-memory、…）
plugins/  # A 形态插件（plugin-todo、plugin-bash、plugin-demo、plugin-memory）
src/      # cos CLI 宿主（P6 前为占位二进制）
```

依赖方向铁律：`plugins/*` 与 `dsh-agent-loop` 只依赖各接缝的 Definition crate
（dsh-llm / dsh-tools / dsh-agent / dsh-shell），不得依赖具体 Provider 或 dsh-agent-loop 本身；
dsh-core 不依赖任何上层 crate。

## 开发

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check   # CI 内执行（本地需 cargo-deny）
```

## 状态

- P0：workspace 骨架 + dsh-core 契约初稿 + hello 插件验收 ✅
- P1：dsh-core 完成 ✅ —— 五种分发单测钉死（14）、scope 路由（13）、卸载审计（6）
- P2：dsh-loader ✅ —— cordis.yml → 工厂解析（plugin! + inventory）→ 拓扑排序 → 挂载，10 组合测试（依赖序/环/缺依赖/重复 provide 全部 fail loud）
- P3：会话日志 + LLM 接缝 + mock ✅ —— SessionEvent 封闭枚举 + Custom、derive_messages、JSONL 逐字节回放；LlmAdapter 对象安全接缝 + 脚本化 mock
- P4：agent + agent-loop ✅ —— turn/step 主循环（wake_driver → kick → turn → pre-step waterfall → step → turn/end，每步先写日志），Agent 句柄/注册表/Inbox/with_initiator 因果链；端到端单轮快照测试
- P5：tools 管线 + system-prompt + plugin-todo ✅ —— pre/guards/execute/post 瀑布、tool/call 先写日志、结果回流下一 step（derive_messages 含完整 call/result 对）、prompt 装配快照
- P6：A 形态收口 ✅ —— cos CLI（--config/--dump-config/优雅退出逆序卸载）、dsh-shell 接缝 + plugin-bash、不变量注册表（模型可见 ⟺ 已记录等 5 条）；demo 端到端快照 + 重放 + 卸载审计全绿，**A 形态 DoD 达成**
- P7 起：B 形态准备与生态（按需推进，详见 PLAN.md §5）

## 桌面陪伴 agent（阶段 2，进行中）

设计文档：`docs/memory-plugin.md`（双层记忆：events append-only 真相源 + topics 可合并状态行 +
关系卡常驻注入；"宁可晚合并，不可错合并"；遗忘曲线做删除，agent 只加强/减弱；诚实出口）。

- M1：记忆内核 + 插件接线 ✅ —— `crates/dsh-memory`（rusqlite bundled 五表 schema、apply_turn
  提取→编号消解→合并、遗忘曲线/唤醒、四工具 remember/recall/inventory/demote）+ `plugins/plugin-memory`
  （提供 `memory` 服务 + 注册四工具）；脚本化 mock 生命周期验收 8 测试（新建/别名合并/保守新建/
  correct 取代/衰减与复活/诚实出口/重开持久化/工具），workspace 94 测试全绿
- M2：接 agent 读/写路径 + 真实 LLM ✅ —— `agent/pre-step` 挂钩（每 turn 第一步消化上一 turn，
  记忆失败不阻塞对话）+ `agent/request` 挂钩（Mode A 主动 recall / Mode B 最近聊过 / 关系卡常驻
  注入 system）；`crates/dsh-llm-opencode`（OpenAI 兼容适配器：流式 SSE 优先、服务端失败自动非流式
  兜底）；cos CLI `--llm-base-url/--llm-model/--llm-api-key`（或 `COS_LLM_*` 环境变量）启用真实
  LLM（openecode zen：`https://opencode.ai/zen/v1`，免费模型 `deepseek-v4-flash-free` 实测可用）；
  `examples/memory.yml` 演示清单；mock 双脚本验收 + 本地回环 SSE 5 测试；实端点冒烟通过（75 事件、
  不变量全过、逆序卸载）
- M3：上下文自动压缩 + digest 慢路径 + 自我认知 ✅ —— `agent/request` 超阈值（`max_context_chars`）
  时旧消息压进**滚动摘要**（`session_state` KV 持久化，`keep_tail` 尾部窗口保原文；压缩失败宁可长
  不可丢），摘要注入 system（进 request/header 日志，不变量不受影响）；digest 慢路径 = 统计
  （事件/主题/跨度地面真值）+ 转录头 → 卡三段注记（高门槛保守，agent_model 含认知缺口→主动追问），
  会话中每 `digest_every` turn 节流触发 + cos 关闭时收尾；修复实端点 429 冒烟暴露的 loop 缺陷
  （LLM 失败路径漏记 step/end，step-pairing 违规，回归测试钉死）；kernel digest/压缩/KV 4 测试 +
  压缩全链路测试；实端点冒烟：主 turn 通过 + digest 遇免费额度限流**失败软降级**（报错不崩、
  不变量全过）
- M3+（后续）：promises 表接线、情绪趋势、晋升机制、多会话持久化（可继续按需推进）
