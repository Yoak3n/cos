//! DlopenPluginSource（P8 试点，P9 服务桥接）：运行期加载独立 cdylib 插件（B 形态）。
//!
//! 装载链：libloading 打开 → 解析 `cos_plugin_abi_version` → 版本握手
//! （不兼容 → fail loud，可读错误）→ `cos_plugin_apply(host, ctx, config, err_buf, err_len)`。
//!
//! HostApi 桥（本模块实现宿主侧）：
//! - `get_service`/`service_call`（P9 桥接）：按名查 [`cos_core::BridgeRegistry`] 快照，
//!   返回不透明句柄；`service_call` 以 method + args JSON 调用（身份校验防伪造/悬垂）；
//! - `emit`/`on`（JSON 载荷事件 `PluginEvent`；非 JSON 载荷事件对 dlopen 插件不可见）；
//! - `register_effect`/`free`（fiber 效果，卸载逆序调用 disposer）；
//! - `register_tool`（P8 试点能力：C 回调工具，执行时 ToolRun JSON → C → ToolOutcome JSON）。
//!
//! P9 生命周期：宿主状态（HostCtx 指向）与插件实例同生命周期——插件可自持 ctx/host
//! 指针并在卸载前调用 host 函数（工具回调内服务直连等）。
//!
//! P8 简化（见 docs/b-abi.md）：能力不按 inject 裁剪（所有 dlopen 插件获得同一能力集）；
//! 事件名跨边界泄漏（`&'static str` 契约所致，插件级事件名数量有限）。

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cos_contract::{
    API_VERSION, ContractVersion, Disposer, ErrorCode, EventCallback, Handle, HostApi, HostCtx,
    PLUGIN_ENTRY_ABI_VERSION, PLUGIN_ENTRY_APPLY, PluginAbiVersion, PluginApply, ToolExecute,
};
use cos_core::{Context, EventPayload, JsonBridge};
use cos_session::ToolError;
use cos_tools::{Tool, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use serde_json::Value;

use crate::error::LoadError;

/// 一个已加载的 dlopen 插件（持有 Library：Drop 即卸载；函数指针随 Arc 存活）。
pub struct DlopenPlugin {
    /// 工厂名（yml `name`，原样）。
    pub name: String,
    _library: libloading::Library,
    apply: PluginApply,
    /// 宿主状态（apply 时创建；与实例同生命周期——插件自持的 ctx/host 指针
    /// 在卸载前始终有效，P9）。
    state: Mutex<Option<Arc<PluginHostState>>>,
}

impl DlopenPlugin {
    /// 按路径加载：dlopen → 版本握手 → 解析 apply 入口。
    pub fn load(name: &str, path: &str) -> Result<Arc<DlopenPlugin>, LoadError> {
        let library =
            unsafe { libloading::Library::new(path) }.map_err(|source| LoadError::DlopenOpen {
                name: name.to_string(),
                path: path.to_string(),
                detail: source.to_string(),
            })?;
        unsafe {
            let abi_version: libloading::Symbol<PluginAbiVersion> = library
                .get(PLUGIN_ENTRY_ABI_VERSION.as_bytes())
                .map_err(|source| LoadError::DlopenSymbol {
                    name: name.to_string(),
                    symbol: PLUGIN_ENTRY_ABI_VERSION.to_string(),
                    detail: source.to_string(),
                })?;
            let plugin_version = ContractVersion::decode(abi_version());
            check_version(name, plugin_version)?;
            let apply: libloading::Symbol<PluginApply> = library
                .get(PLUGIN_ENTRY_APPLY.as_bytes())
                .map_err(|source| LoadError::DlopenSymbol {
                    name: name.to_string(),
                    symbol: PLUGIN_ENTRY_APPLY.to_string(),
                    detail: source.to_string(),
                })?;
            // 拷贝函数指针、结束 Symbol 借用后再移动 Library
            let apply: PluginApply = *apply;
            Ok(Arc::new(DlopenPlugin {
                name: name.to_string(),
                _library: library,
                apply,
                state: Mutex::new(None),
            }))
        }
    }

    /// 调用插件 apply（HostApi 桥 + 配置 JSON + 错误缓冲）。
    pub fn apply(&self, ctx: &Context, _entry_id: &str, config: Value) -> Result<(), LoadError> {
        // P9：宿主状态与实例同生命周期——插件可自持 ctx/host 指针并在卸载前
        // 调用 host 函数（服务直连、事件等）。桥快照保证 get_service 返回的
        // 指针在插件生命周期内稳定（注册方卸载不会使指针悬垂）。
        let bridges = ctx
            .get::<cos_core::BridgeRegistry>()
            .map(|registry| registry.snapshot())
            .unwrap_or_default();
        let state = Arc::new(PluginHostState {
            ctx: ctx.clone(),
            host_api: Box::new(build_host_api()),
            bridges,
        });
        *self.state.lock().unwrap() = Some(state.clone());
        let config_json =
            CString::new(serde_json::to_string(&config).map_err(|error| {
                LoadError::Other(format!("dlopen 插件配置序列化失败: {error}"))
            })?)
            .map_err(|_| LoadError::Other("配置含 NUL".into()))?;
        let mut error_buf = vec![0u8; 4096];
        let code = unsafe {
            (self.apply)(
                state.host_api.as_ref(),
                Arc::as_ptr(&state) as HostCtx,
                config_json.as_ptr(),
                error_buf.as_mut_ptr() as *mut c_char,
                error_buf.len(),
            )
        };
        match ErrorCode::from_i32(code) {
            Some(ErrorCode::Ok) => Ok(()),
            _ => {
                *self.state.lock().unwrap() = None;
                let message = unsafe { CStr::from_ptr(error_buf.as_ptr() as *const c_char) }
                    .to_string_lossy()
                    .into_owned();
                Err(LoadError::DlopenApply {
                    name: self.name.clone(),
                    code,
                    message,
                })
            }
        }
    }
}

/// 版本握手（纯函数，单元测试覆盖不匹配路径）。
pub fn check_version(name: &str, plugin: ContractVersion) -> Result<(), LoadError> {
    if plugin.compatible_with(&API_VERSION) {
        Ok(())
    } else {
        Err(LoadError::AbiMismatch {
            name: name.to_string(),
            host: API_VERSION,
            plugin,
        })
    }
}

/// 插件宿主状态（HostCtx 指向它；与插件实例同生命周期——apply 后插件
/// 自持 ctx/host 指针仍可调用 host 函数，P9）。
struct PluginHostState {
    ctx: Context,
    /// HostApi 函数表（与状态同生命周期；插件只读）。
    host_api: Box<HostApi>,
    /// JSON 桥快照：get_service 返回的指针指向快照内 Arc 的分配，插件生命周期内稳定。
    bridges: Vec<(&'static str, Arc<dyn JsonBridge>)>,
}

/// HostCtx → 宿主状态（空指针 → None）。
fn state_of(ctx: HostCtx) -> Option<&'static PluginHostState> {
    if ctx.is_null() {
        return None;
    }
    unsafe { (ctx as *const PluginHostState).as_ref() }
}

/// 写宿主缓冲（NUL 结尾；超长截断安全）。
fn write_result(buf: *mut c_char, len: usize, text: &str) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = text.as_bytes();
    let copy_len = bytes.len().min(len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, copy_len);
        *buf.add(copy_len) = 0;
    }
}

/// JSON 载荷事件（dlopen 插件 emit/on 的载荷形态）。
pub struct PluginEvent(pub Value);

/// userdata 的 Send/Sync 包装（契约：插件保证其指向的对象可跨线程或自同步）。
#[derive(Clone, Copy)]
struct SendUserdata(*mut c_void);
unsafe impl Send for SendUserdata {}
unsafe impl Sync for SendUserdata {}

#[allow(dead_code)]
fn _send_proof() {
    fn assert_send<T: Send>() {}
    assert_send::<SendUserdata>();
    assert_send::<EventCallback>();
    assert_send::<Disposer>();
}

// ---------------------------------------------------------------------------
// HostApi 桥（extern "C" 实现）
// ---------------------------------------------------------------------------

extern "C" fn host_get_service(ctx: HostCtx, name: *const c_char) -> *const c_void {
    let Some(state) = state_of(ctx) else {
        return ptr::null_mut();
    };
    if name.is_null() {
        return ptr::null_mut();
    }
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .to_string();
    // P9 桥接：按名查桥快照；未注册 → 空指针。返回的指针指向快照内 Arc 的分配，
    // 与插件实例同生命周期（状态随实例存活，卸载后不再可调用）。
    state
        .bridges
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, bridge)| Arc::as_ptr(bridge) as *const c_void)
        .unwrap_or(ptr::null_mut())
}

extern "C" fn host_service_call(
    ctx: HostCtx,
    service: *const c_void,
    method: *const c_char,
    args_json: *const c_char,
    result_buf: *mut c_char,
    result_len: usize,
) -> i32 {
    let Some(state) = state_of(ctx) else {
        return ErrorCode::InvalidHandle as i32;
    };
    if service.is_null() || method.is_null() || result_buf.is_null() || result_len == 0 {
        return ErrorCode::InvalidHandle as i32;
    }
    // 身份校验：指针必须来自本宿主状态的 get_service（防伪造/悬垂）
    let Some(bridge) = state
        .bridges
        .iter()
        .map(|(_, bridge)| bridge)
        .find(|bridge| Arc::as_ptr(*bridge) as *const c_void == service)
    else {
        return ErrorCode::InvalidHandle as i32;
    };
    let method = unsafe { CStr::from_ptr(method) }
        .to_string_lossy()
        .to_string();
    let args = if args_json.is_null() {
        Value::Null
    } else {
        let text = unsafe { CStr::from_ptr(args_json) }
            .to_string_lossy()
            .to_string();
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    match bridge.call(&method, args) {
        Ok(result) => {
            let json = serde_json::to_string(&result).unwrap_or_else(|_| "{}".into());
            write_result(result_buf, result_len, &json);
            ErrorCode::Ok as i32
        }
        Err(error) => {
            write_result(result_buf, result_len, &error.to_string());
            ErrorCode::CallFailed as i32
        }
    }
}

extern "C" fn host_emit(ctx: HostCtx, name: *const c_char, payload: *const c_char) {
    if ctx.is_null() || name.is_null() {
        return;
    }
    let Some(state) = (unsafe { (ctx as *const PluginHostState).as_ref() }) else {
        return;
    };
    let name = leak_cstr(name);
    let payload = unsafe { CStr::from_ptr(payload) }
        .to_string_lossy()
        .to_string();
    let value = serde_json::from_str(&payload).unwrap_or(Value::String(payload));
    state
        .ctx
        .emit(name, std::sync::Arc::new(PluginEvent(value)));
}

extern "C" fn host_on(
    ctx: HostCtx,
    name: *const c_char,
    callback: EventCallback,
    userdata: *mut c_void,
) -> Handle {
    if ctx.is_null() || name.is_null() {
        return 0;
    }
    let Some(state) = (unsafe { (ctx as *const PluginHostState).as_ref() }) else {
        return 0;
    };
    let name = leak_cstr(name);
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    // 注意：精确捕获分析会捕获 `userdata.0` 字段而非整个包装 → 裸指针操作全部移出闭包
    let userdata = SendUserdata(userdata);
    let _ = state.ctx.on(name, move |payload: &EventPayload| {
        let json = match payload.downcast_ref::<PluginEvent>() {
            Some(event) => serde_json::to_string(&event.0).unwrap_or_else(|_| "{}".into()),
            None => "{}".into(),
        };
        let json = CString::new(json).unwrap_or_default();
        invoke_callback(callback, userdata, &json);
    });
    handle
}

/// 事件回调调用（裸指针操作，闭包外）。
fn invoke_callback(callback: EventCallback, userdata: SendUserdata, json: &CString) {
    unsafe {
        callback(userdata.0, json.as_ptr());
    }
}

extern "C" fn host_register_effect(
    ctx: HostCtx,
    disposer: Disposer,
    userdata: *mut c_void,
) -> Handle {
    if ctx.is_null() {
        return 0;
    }
    let Some(state) = (unsafe { (ctx as *const PluginHostState).as_ref() }) else {
        return 0;
    };
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let userdata = SendUserdata(userdata);
    let effect = cos_core::EffectHandle::new(move || invoke_disposer(disposer, userdata));
    state.ctx.fiber().push(effect);
    handle
}

/// disposer 调用（裸指针操作，闭包外）。
fn invoke_disposer(disposer: Disposer, userdata: SendUserdata) {
    unsafe {
        disposer(userdata.0);
    }
}

extern "C" fn host_free(_ctx: HostCtx, _handle: Handle) {
    // P8 简化：效果/监听句柄随 fiber 卸载自动回收，free 为空操作（插件卸载前调用无害）
}

extern "C" fn host_register_tool(
    ctx: HostCtx,
    name: *const c_char,
    description: *const c_char,
    parameters_json: *const c_char,
    execute: ToolExecute,
    userdata: *mut c_void,
) -> Handle {
    if ctx.is_null() || name.is_null() {
        return 0;
    }
    let Some(state) = (unsafe { (ctx as *const PluginHostState).as_ref() }) else {
        return 0;
    };
    let name = leak_cstr(name);
    let description = leak_cstr(description);
    let parameters = unsafe { CStr::from_ptr(parameters_json) }
        .to_string_lossy()
        .to_string();
    let parameters: Value = serde_json::from_str(&parameters).unwrap_or(Value::Null);
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let tool = DlopenTool {
        name,
        description,
        parameters,
        execute,
        userdata,
    };
    match state.ctx.get::<ToolRegistry>() {
        Ok(registry) => {
            let _ = registry.register(std::sync::Arc::new(tool));
        }
        Err(_) => {
            // tools 服务未装配：工具注册失败（无副作用；加载结果由插件侧检查）
        }
    }
    handle
}

/// 工具句柄计数器（on / register_effect / register_tool 共用）。
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// C 字符串 → 泄漏的 `&'static str`（跨边界 'static 契约；插件级名称数量有限）。
fn leak_cstr(ptr: *const c_char) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    let cstr = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    Box::leak(cstr.into_boxed_str())
}

/// C 回调包装的工具（执行时 ToolRun JSON → C → ToolOutcome JSON）。
struct DlopenTool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    execute: ToolExecute,
    userdata: *mut c_void,
}

// 指针 userdata 可跨线程（契约：插件保证其指向的对象 Send + Sync 或自同步）。
unsafe impl Send for DlopenTool {}
unsafe impl Sync for DlopenTool {}

impl Tool for DlopenTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn execute(
        &self,
        _ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, ToolError>> {
        let execute = self.execute;
        let userdata = SendUserdata(self.userdata);
        let run_json = serde_json::to_string(run).unwrap_or_default();
        Box::pin(async move {
            let run_json = CString::new(run_json).unwrap_or_default();
            // 宿主分配结果缓冲（契约：插件写入、NUL 结尾；大小 64 KiB，超限截断并报错）
            let mut result_buf = vec![0u8; 64 * 1024];
            let code = invoke_tool(execute, userdata, &run_json, &mut result_buf);
            let raw = unsafe { CStr::from_ptr(result_buf.as_ptr() as *const c_char) }
                .to_string_lossy()
                .into_owned();
            let outcome = serde_json::from_str(&raw).unwrap_or_else(|error| {
                ToolOutcome::error(
                    format!("dlopen 工具结果无效（code={code}）: {error}"),
                    ToolError {
                        name: "DlopenTool".into(),
                        code: "INVALID_RESULT".into(),
                    },
                )
            });
            Ok(outcome)
        })
    }
}

/// 工具执行调用（裸指针操作，async 块外）。
fn invoke_tool(
    execute: ToolExecute,
    userdata: SendUserdata,
    run_json: &CStr,
    result_buf: &mut [u8],
) -> i32 {
    unsafe {
        execute(
            userdata.0,
            run_json.as_ptr(),
            result_buf.as_mut_ptr() as *mut c_char,
            result_buf.len(),
        )
    }
}

/// 构造 HostApi 函数表（桥实现 + 当前 API 版本）。
fn build_host_api() -> HostApi {
    HostApi {
        api_version: API_VERSION.encode(),
        get_service: host_get_service,
        emit: host_emit,
        on: host_on,
        register_effect: host_register_effect,
        free: host_free,
        register_tool: host_register_tool,
        service_call: host_service_call,
    }
}

#[cfg(test)]
mod tests {
    use super::check_version;
    use cos_contract::ContractVersion;

    #[test]
    fn version_handshake_accepts_compatible_and_rejects_mismatch() {
        let host = cos_contract::API_VERSION;
        // 相同版本 → 通过
        assert!(check_version("p", host).is_ok());
        // 同 major、更低 minor → 通过
        let older = ContractVersion {
            major: host.major,
            minor: host.minor.saturating_sub(1),
            patch: 0,
        };
        assert!(check_version("p", older).is_ok());
        // major 不同 → 拒绝（可读错误）
        let other = ContractVersion {
            major: host.major + 1,
            minor: 0,
            patch: 0,
        };
        let error = check_version("bad-plugin", other).unwrap_err().to_string();
        assert!(error.contains("bad-plugin"), "{error}");
        assert!(error.contains("ABI"), "{error}");
        // 插件 minor 超前 → 拒绝
        let ahead = ContractVersion {
            major: host.major,
            minor: host.minor + 1,
            patch: 0,
        };
        assert!(check_version("ahead", ahead).is_err());
    }

    // -----------------------------------------------------------------------
    // P9 get_service / service_call 桥（直构宿主状态，不加载真实 cdylib）
    // -----------------------------------------------------------------------

    use super::*;
    use cos_core::{CoreError, CoreResult, JsonBridge, Service};
    use std::ffi::CString;

    /// 测试桥：`ping` 回显 method + args；其余方法报错（模拟真实桥的未知方法路径）。
    struct EchoBridge;
    impl Service for EchoBridge {
        const NAME: &'static str = "echo";
    }
    impl JsonBridge for EchoBridge {
        fn call(&self, method: &str, args: Value) -> CoreResult<Value> {
            if method == "ping" {
                Ok(serde_json::json!({ "method": method, "args": args }))
            } else {
                Err(CoreError::Other(format!("未知 echo 桥方法: {method}")))
            }
        }
    }

    fn state_with_echo() -> Arc<PluginHostState> {
        Arc::new(PluginHostState {
            ctx: Context::root(),
            host_api: Box::new(build_host_api()),
            bridges: vec![("echo", Arc::new(EchoBridge) as Arc<dyn JsonBridge>)],
        })
    }

    fn cptr(text: &str) -> *const c_char {
        CString::new(text).unwrap().into_raw()
    }

    #[test]
    fn get_service_returns_null_for_unknown_name() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let service = host_get_service(ctx, cptr("nope"));
        assert!(service.is_null());
        // 空 ctx / 空 name → 空指针
        assert!(host_get_service(std::ptr::null_mut(), cptr("echo")).is_null());
        assert!(host_get_service(ctx, std::ptr::null()).is_null());
    }

    #[test]
    fn service_call_roundtrip() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let service = host_get_service(ctx, cptr("echo"));
        assert!(!service.is_null());
        let mut buf = vec![0u8; 4096];
        let code = host_service_call(
            ctx,
            service,
            cptr("ping"),
            cptr(r#"{"a":1}"#),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        );
        assert_eq!(code, ErrorCode::Ok as i32);
        let raw = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
            .to_string_lossy()
            .into_owned();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["method"], "ping");
        assert_eq!(value["args"]["a"], 1);
    }

    #[test]
    fn service_call_rejects_foreign_or_null_pointers() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        // 伪造指针 → InvalidHandle
        let mut buf = vec![0u8; 4096];
        let code = host_service_call(
            ctx,
            &1u8 as *const u8 as *const c_void,
            cptr("ping"),
            cptr("{}"),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        );
        assert_eq!(code, ErrorCode::InvalidHandle as i32);
        // 空指针 / 空缓冲 → InvalidHandle
        assert_eq!(
            host_service_call(
                ctx,
                std::ptr::null(),
                cptr("m"),
                cptr("{}"),
                buf.as_mut_ptr() as *mut c_char,
                buf.len()
            ),
            ErrorCode::InvalidHandle as i32
        );
        assert_eq!(
            host_service_call(
                ctx,
                std::ptr::null(),
                cptr("m"),
                cptr("{}"),
                std::ptr::null_mut(),
                0
            ),
            ErrorCode::InvalidHandle as i32
        );
        // 空 ctx → InvalidHandle
        assert_eq!(
            host_service_call(
                std::ptr::null_mut(),
                std::ptr::null(),
                cptr("m"),
                cptr("{}"),
                buf.as_mut_ptr() as *mut c_char,
                buf.len()
            ),
            ErrorCode::InvalidHandle as i32
        );
    }

    #[test]
    fn service_call_failure_writes_error_text() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let service = host_get_service(ctx, cptr("echo"));
        let mut buf = vec![0u8; 4096];
        // 未知方法 → CallFailed + 错误文本入缓冲
        let code = host_service_call(
            ctx,
            service,
            cptr("nope"),
            cptr("{}"),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        );
        assert_eq!(code, ErrorCode::CallFailed as i32);
        let raw = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
            .to_string_lossy()
            .into_owned();
        assert!(raw.contains("nope"), "错误文本应含方法名: {raw}");
    }
}
