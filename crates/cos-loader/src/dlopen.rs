//! DlopenPluginSource（P8 试点）：运行期加载独立 cdylib 插件（B 形态）。
//!
//! 装载链：libloading 打开 → 解析 `cos_plugin_abi_version` → 版本握手
//! （不兼容 → fail loud，可读错误）→ `cos_plugin_apply(host, ctx, config, err_buf, err_len)`。
//!
//! HostApi 桥（本模块实现宿主侧）：`get_service`（P8 未桥接 → 恒空指针）、
//! `emit`/`on`（JSON 载荷事件 `PluginEvent`；非 JSON 载荷事件对 dlopen 插件不可见）、
//! `register_effect`/`free`（fiber 效果，卸载逆序调用 disposer）、
//! `register_tool`（P8 试点能力：C 回调工具，执行时 ToolRun JSON → C → ToolOutcome JSON）。
//!
//! P8 简化（见 docs/b-abi.md）：能力不按 inject 裁剪（所有 dlopen 插件获得同一能力集）；
//! 事件名跨边界泄漏（`&'static str` 契约所致，插件级事件名数量有限）。

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cos_contract::{
    API_VERSION, ContractVersion, Disposer, ErrorCode, EventCallback, Handle, HostApi, HostCtx,
    PLUGIN_ENTRY_ABI_VERSION, PLUGIN_ENTRY_APPLY, PluginAbiVersion, PluginApply, ToolExecute,
};
use cos_core::{Context, EventPayload};
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
            }))
        }
    }

    /// 调用插件 apply（HostApi 桥 + 配置 JSON + 错误缓冲）。
    pub fn apply(&self, ctx: &Context, _entry_id: &str, config: Value) -> Result<(), LoadError> {
        let state = PluginHostState { ctx: ctx.clone() };
        let host = build_host_api();
        let config_json =
            CString::new(serde_json::to_string(&config).map_err(|error| {
                LoadError::Other(format!("dlopen 插件配置序列化失败: {error}"))
            })?)
            .map_err(|_| LoadError::Other("配置含 NUL".into()))?;
        let mut error_buf = vec![0u8; 4096];
        let code = unsafe {
            (self.apply)(
                &host,
                &state as *const PluginHostState as HostCtx,
                config_json.as_ptr(),
                error_buf.as_mut_ptr() as *mut c_char,
                error_buf.len(),
            )
        };
        match ErrorCode::from_i32(code) {
            Some(ErrorCode::Ok) => Ok(()),
            _ => {
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

/// 插件宿主状态（HostCtx 指向它；apply 期间存活）。
struct PluginHostState {
    ctx: Context,
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

extern "C" fn host_get_service(_ctx: HostCtx, _name: *const c_char) -> *const c_void {
    // P8 未桥接：get_service 恒空指针（能力裁剪留 P9）
    ptr::null_mut()
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
}
