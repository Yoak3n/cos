//! cos-contract —— 版本化契约 crate（B 形态地基，见 PLAN.md §6 / P7）。
//! 版本号自 P0 起存在；接缝 trait 对象安全审计与服务方法窄化于 P7 冻结。

/// 契约 crate 版本（semver，随 crate 发布）。
pub const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ABI 世代号：P7 冻结 B 形态 HostApi 时递增。
pub const ABI_GENERATION: u32 = 0;
