# cos-llm-mock

确定性脚本化 mock 适配器（测试与回放，P3）

依赖方向铁律（PLAN.md §2）：plugins/* 与 cos-agent-loop 只依赖接缝 Definition crate；
cos-core 不依赖任何上层 crate。

设计决策见 [../docs/decisions.md](../docs/decisions.md)；实施计划见 [../PLAN.md](../PLAN.md)。

