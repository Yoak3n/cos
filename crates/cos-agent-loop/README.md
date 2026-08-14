# cos-agent-loop

turn/step 驱动器（Provider，实现 cos-agent；P4/P5）

依赖方向铁律（PLAN.md §2）：plugins/* 与 cos-agent-loop 只依赖接缝 Definition crate；
cos-core 不依赖任何上层 crate。

设计决策见 [../docs/decisions.md](../docs/decisions.md)；实施计划见 [../PLAN.md](../PLAN.md)。

