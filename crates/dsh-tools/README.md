# dsh-tools

工具注册表 + 执行管线 pre/guards/execute/post（P5）

依赖方向铁律（PLAN.md §2）：plugins/* 与 dsh-agent-loop 只依赖接缝 Definition crate；
dsh-core 不依赖任何上层 crate。

设计决策见 [../docs/decisions.md](../docs/decisions.md)；实施计划见 [../PLAN.md](../PLAN.md)。

