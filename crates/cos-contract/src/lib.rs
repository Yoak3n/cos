//! cos-contract —— B 形态版本化契约 crate（PLAN.md P7 冻结）。
//!
//! 内容：B-ABI 版本协商、HostApi 能力函数表（Rust 侧规范形态，P8 以 `extern "C"`
//! 导出/导入）、插件导出入口、错误码、插件清单（JSON）。设计详见 `docs/b-abi.md`。
//!
//! 铁律：本 crate 零运行时依赖（仅 serde，用于清单 JSON）；不依赖任何接缝/插件 crate。
//! 字段顺序即 ABI：HostApi 只许追加、不许重排、不许删改既有字段（major 递增）。

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// 契约 crate 版本（semver，随 crate 发布）。
pub const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// B-ABI 版本（HostApi 函数表语义版本；与 crate 版本解耦）。
/// 兼容规则：major 相同且插件 minor ≤ 宿主 minor 即兼容（见 [`ContractVersion::compatible_with`]）。
pub const API_VERSION: ContractVersion = ContractVersion {
    major: 0,
    minor: 3,
    patch: 0,
};

/// ABI 世代号：B-ABI 定稿时递增（`API_VERSION.major` 的简写）。
pub const ABI_GENERATION: u32 = API_VERSION.major;

/// 版本协商结构（semver 三元组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractVersion {
    /// 主版本：不兼容变更（字段重排/删改、语义变更）→ 递增。
    pub major: u32,
    /// 次版本：向后兼容追加（新字段/新函数追加到表尾）→ 递增。
    pub minor: u32,
    /// 修订：内部修正，不影响兼容性。
    pub patch: u32,
}

impl ContractVersion {
    /// 兼容判定：major 必须相等；插件 minor 不得超过宿主 minor。
    pub fn compatible_with(&self, host: &ContractVersion) -> bool {
        self.major == host.major && self.minor <= host.minor
    }

    /// 编码为 u32（`major<<16 | minor<<8 | patch`），供 C ABI 握手。
    pub fn encode(&self) -> u32 {
        (self.major << 16) | (self.minor << 8) | self.patch
    }

    /// 从 u32 解码（编码的逆运算；高位截断不校验）。
    pub fn decode(encoded: u32) -> ContractVersion {
        ContractVersion {
            major: encoded >> 16,
            minor: (encoded >> 8) & 0xff,
            patch: encoded & 0xff,
        }
    }
}

// ---------------------------------------------------------------------------
// B-ABI 基础类型（C 兼容）
// ---------------------------------------------------------------------------

/// 宿主侧不透明上下文（由宿主分配、插件原样回传；插件不得解引用）。
pub type HostCtx = *mut std::ffi::c_void;

/// 宿主资源句柄（on / register_effect 的返回；free 回收）。
pub type Handle = u64;

/// 错误缓冲（apply 失败时插件写入 UTF-8 错误文本；宿主分配、宿主读取后释放）。
pub type ErrorBuf = *mut std::ffi::c_char;

/// 事件回调（插件经 `on` 注册；`event_json` 在回调返回后失效，不得持有）。
pub type EventCallback =
    unsafe extern "C" fn(userdata: *mut std::ffi::c_void, event_json: *const std::ffi::c_char);

/// 效果释放回调（插件经 `register_effect` 注册；插件卸载时宿主逆序调用）。
pub type Disposer = unsafe extern "C" fn(userdata: *mut std::ffi::c_void);

/// HostApi 能力函数表（B-ABI 核心）。
///
/// 宿主在 apply 前构造并填充；插件**只读**使用。字段顺序即 ABI——
/// 追加新能力 = minor 递增、在表尾加字段；重排/删改 = major 递增。
///
/// 能力按 inject 裁剪：`get_service` 只对插件清单 `inject` 声明的服务生效，
/// 未注入的服务返回空指针（插件不得绕过）。
#[repr(C)]
pub struct HostApi {
    /// `API_VERSION.encode()`。
    pub api_version: u32,
    /// 按名取服务（返回实现侧 opaque 指针；未注入/未注册 → 空指针）。
    pub get_service: unsafe extern "C" fn(
        ctx: HostCtx,
        name: *const std::ffi::c_char,
    ) -> *const std::ffi::c_void,
    /// 广播事件（`payload_json` 在调用返回后失效；同步分发）。
    pub emit: unsafe extern "C" fn(
        ctx: HostCtx,
        name: *const std::ffi::c_char,
        payload_json: *const std::ffi::c_char,
    ),
    /// 注册事件监听（返回句柄；free 注销）。
    pub on: unsafe extern "C" fn(
        ctx: HostCtx,
        name: *const std::ffi::c_char,
        callback: EventCallback,
        userdata: *mut std::ffi::c_void,
    ) -> Handle,
    /// 注册效果（卸载时逆序调用 disposer；返回句柄；free 可提前撤销）。
    pub register_effect: unsafe extern "C" fn(
        ctx: HostCtx,
        disposer: Disposer,
        userdata: *mut std::ffi::c_void,
    ) -> Handle,
    /// 释放宿主资源句柄（on / register_effect 的返回值）。
    pub free: unsafe extern "C" fn(ctx: HostCtx, handle: Handle),
    /// 注册工具（0.2.0 追加；P8 试点能力——todo 薄壳经此提供 todo_write）。
    /// 执行时宿主把 ToolRun 序列化为 JSON 调 `execute`，
    /// 插件把 ToolOutcome JSON 写入 `result_buf`（宿主分配，NUL 结尾）。
    pub register_tool: unsafe extern "C" fn(
        ctx: HostCtx,
        name: *const std::ffi::c_char,
        description: *const std::ffi::c_char,
        parameters_json: *const std::ffi::c_char,
        execute: ToolExecute,
        userdata: *mut std::ffi::c_void,
    ) -> Handle,
    /// Call a host service (0.3.0 addition; P9 bridge - invoke the opaque handle
    /// returned by [`HostApi::get_service`]).
    /// `service` must be a value returned by `get_service` (identity checked;
    /// forged/dangling pointers -> [`ErrorCode::InvalidHandle`]).
    /// `method` + `args_json` are dispatched to the host-side JSON bridge
    /// (see `cos_core::JsonBridge`); the result JSON is written to `result_buf`
    /// (host-allocated, NUL-terminated); on failure an error text is written and
    /// a non-zero [`ErrorCode`] is returned.
    pub service_call: unsafe extern "C" fn(
        ctx: HostCtx,
        service: *const std::ffi::c_void,
        method: *const std::ffi::c_char,
        args_json: *const std::ffi::c_char,
        result_buf: *mut std::ffi::c_char,
        result_len: usize,
    ) -> i32,
}

/// 工具执行回调（register_tool 的 execute；`run_json`/`result_buf` 仅在调用期间有效）。
pub type ToolExecute = unsafe extern "C" fn(
    userdata: *mut std::ffi::c_void,
    run_json: *const std::ffi::c_char,
    result_buf: *mut std::ffi::c_char,
    result_len: usize,
) -> i32;

// ---------------------------------------------------------------------------
// 插件导出入口（cdylib 必须导出）
// ---------------------------------------------------------------------------

/// 导出符号：返回 `API_VERSION.encode()`（宿主先调它做版本握手）。
pub const PLUGIN_ENTRY_ABI_VERSION: &str = "cos_plugin_abi_version";
/// 导出符号：`apply(host, config_json, error_buf, error_len) -> ErrorCode`。
pub const PLUGIN_ENTRY_APPLY: &str = "cos_plugin_apply";
/// 导出符号（可选，P9）：`validate(config_json, error_buf, error_len) -> ErrorCode`。
pub const PLUGIN_ENTRY_VALIDATE: &str = "cos_plugin_validate";

/// `cos_plugin_abi_version` 的函数签名。
pub type PluginAbiVersion = unsafe extern "C" fn() -> u32;

/// `cos_plugin_apply` 的函数签名。
/// `ctx`（HostCtx）在 apply 期间有效；插件后续调用 HostApi 函数时原样回传。
pub type PluginApply = unsafe extern "C" fn(
    host: *const HostApi,
    ctx: HostCtx,
    config_json: *const std::ffi::c_char,
    error_buf: ErrorBuf,
    error_len: usize,
) -> i32;

/// `cos_plugin_validate`（可选）的函数签名。
pub type PluginValidate = unsafe extern "C" fn(
    config_json: *const std::ffi::c_char,
    error_buf: ErrorBuf,
    error_len: usize,
) -> i32;

// ---------------------------------------------------------------------------
// 错误码
// ---------------------------------------------------------------------------

/// B-ABI 错误码（`cos_plugin_apply` 等返回；`i32` 与 C 兼容）。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// 成功。
    Ok = 0,
    /// 版本不兼容（宿主与插件 ABI 不匹配）。
    AbiMismatch = 1,
    /// 配置 JSON 无效。
    ConfigInvalid = 2,
    /// 请求的服务未注入/未注册。
    ServiceUnavailable = 3,
    /// apply 执行失败（详情见 error_buf）。
    ApplyFailed = 4,
    /// 效果注册失败。
    EffectRegistrationFailed = 5,
    /// 句柄无效。
    InvalidHandle = 6,
    /// 服务调用失败（0.3.0 追加；详情文本写入 result_buf / error_buf）。
    CallFailed = 7,
}

impl ErrorCode {
    /// 从 i32 映射（未知值 → None）。
    pub fn from_i32(code: i32) -> Option<ErrorCode> {
        Some(match code {
            0 => ErrorCode::Ok,
            1 => ErrorCode::AbiMismatch,
            2 => ErrorCode::ConfigInvalid,
            3 => ErrorCode::ServiceUnavailable,
            4 => ErrorCode::ApplyFailed,
            5 => ErrorCode::EffectRegistrationFailed,
            6 => ErrorCode::InvalidHandle,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// 插件清单（JSON；与 cordis.yml entry 对齐）
// ---------------------------------------------------------------------------

/// 插件清单（B 形态：随 cdylib 交付的元数据；A 形态由 `plugin!` 宏提供等价信息）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 插件 id（同 cordis plugin id / [`cos_core::Plugin::ID`]）。
    pub id: String,
    /// 插件自身版本（semver 字符串）。
    #[serde(default)]
    pub version: String,
    /// 声明的 B-ABI 版本（缺省 = 当前 [`API_VERSION`]；格式 `major.minor.patch`）。
    #[serde(default)]
    pub api: Option<String>,
    /// 依赖的服务名（宿主按此裁剪 HostApi 能力）。
    #[serde(default)]
    pub inject: Vec<String>,
    /// 提供的服务名。
    #[serde(default)]
    pub provide: Vec<String>,
}

impl PluginManifest {
    /// 解析 `api` 字段（`"major.minor.patch"`；缺省/非法 → None，宿主按当前版本对待并告警）。
    pub fn api_version(&self) -> Option<ContractVersion> {
        self.api.as_deref().and_then(parse_version)
    }
}

/// 解析 `major.minor.patch` 字符串。
pub fn parse_version(text: &str) -> Option<ContractVersion> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(ContractVersion {
        major,
        minor,
        patch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_roundtrips_through_encoding() {
        let version = ContractVersion {
            major: 1,
            minor: 2,
            patch: 3,
        };
        assert_eq!(ContractVersion::decode(version.encode()), version);
    }

    #[test]
    fn compatibility_rules() {
        let host = API_VERSION;
        assert!(host.compatible_with(&host));
        // major 相同、插件 minor ≤ 宿主 minor → 兼容
        let older = ContractVersion {
            major: host.major,
            minor: host.minor.saturating_sub(1),
            patch: 0,
        };
        assert!(older.compatible_with(&host));
        // major 不同 → 不兼容
        let other_major = ContractVersion {
            major: host.major + 1,
            minor: 0,
            patch: 0,
        };
        assert!(!other_major.compatible_with(&host));
        // 插件 minor 超前 → 不兼容
        let ahead = ContractVersion {
            major: host.major,
            minor: host.minor + 1,
            patch: 0,
        };
        assert!(!ahead.compatible_with(&host));
    }

    #[test]
    fn error_codes_roundtrip() {
        for code in [
            ErrorCode::Ok,
            ErrorCode::AbiMismatch,
            ErrorCode::ConfigInvalid,
            ErrorCode::ServiceUnavailable,
            ErrorCode::ApplyFailed,
            ErrorCode::EffectRegistrationFailed,
            ErrorCode::InvalidHandle,
        ] {
            assert_eq!(ErrorCode::from_i32(code as i32), Some(code));
        }
        assert_eq!(ErrorCode::from_i32(99), None);
    }

    #[test]
    fn manifest_api_parsing() {
        assert_eq!(
            parse_version("0.1.0"),
            Some(ContractVersion {
                major: 0,
                minor: 1,
                patch: 0
            })
        );
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("x.y.z"), None);
        assert_eq!(parse_version("1.2.3.4"), None);

        let manifest: PluginManifest = serde_json::from_str(
            r#"{"id":"todo","version":"1.0.0","api":"0.1.0","inject":["tools"],"provide":[]}"#,
        )
        .unwrap();
        assert_eq!(manifest.id, "todo");
        assert_eq!(manifest.api_version().unwrap().minor, 1);
        // api 缺省 → None（宿主按当前版本对待）
        let bare: PluginManifest = serde_json::from_str(r#"{"id":"todo"}"#).unwrap();
        assert!(bare.api_version().is_none());
    }
}
