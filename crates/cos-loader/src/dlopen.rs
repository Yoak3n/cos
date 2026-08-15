//! DlopenPluginSource（P8 试点，P9 服务桥接，P10 清单一等公民，P11 资源生命周期，
//! P12 暴露面审计：validate 入口兑现 + 清单 api 强制）：运行期加载独立 cdylib
//! 插件（B 形态）。
//!
//! 装载链：libloading 打开 → 解析 `cos_plugin_abi_version` → 版本握手
//! （不兼容 → fail loud，可读错误）→ 解析 `cos_plugin_manifest`（可选；清单 `api`
//! 字段强制，P12）→ 解析 `cos_plugin_validate`（可选，P12）→
//! `cos_plugin_apply(host, ctx, config, err_buf, err_len)`（apply 前先调 validate）。
//!
//! HostApi 桥（本模块实现宿主侧）：
//! - `get_service`/`service_call`（P9 桥接）：按名查 [`cos_core::BridgeRegistry`] 快照，
//!   返回不透明句柄；`service_call` 以 method + args JSON 调用（身份校验防伪造/悬垂）；
//! - `emit`/`on`（JSON 载荷事件 `PluginEvent`；非 JSON 载荷事件对 dlopen 插件不可见）；
//! - `register_effect`/`free`（fiber 效果，卸载逆序调用 disposer；`free` 可提前撤销）；
//! - `register_tool`（P8 试点能力：C 回调工具，执行时 ToolRun JSON → C → ToolOutcome JSON；
//!   卸载/`free` 自动注销）。
//!
//! P9 生命周期：宿主状态（HostCtx 指向）与插件实例同生命周期——插件可自持 ctx/host
//! 指针并在卸载前调用 host 函数（工具回调内服务直连等）。
//!
//! P10 **清单一等公民**：插件导出 `cos_plugin_manifest`（JSON：`inject`/`provide`）——
//! loader 把清单注入/提供名并入依赖图（拓扑排序，见 [`crate::compose`]），并据此
//! **裁剪 HostApi 能力**：`get_service` 只对清单 `inject` 声明的服务生效，未注入的
//! 服务返回空指针（插件侧 fail loud）。**向后兼容**：无清单符号 = 旧行为（不裁剪、
//! 不参与依赖图）。事件/工具注册等通用能力不受裁剪影响。
//!
//! P11 **资源生命周期**（生态开放前置）：跨边界 `&'static str`（事件名/工具名/描述）
//! 由插件级**驻留区**（`PluginHostState.strings`，去重）持有、随插件状态 drop（卸载）
//! 释放——取代 P8 的 `Box::leak`（每次调用泄漏一份，卸载重载无限增长）；
//! `free` 从空操作变为**诚实回收**（句柄注册表校验：监听/效果提前 dispose、工具注销；
//! 未知/外来/重复 free = 幂等无操作）；工具注册经 fiber 注销效果自动反注册
//! （卸载/回滚不留僵尸工具，重载同名工具可再注册）。

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cos_contract::{
    API_VERSION, ContractVersion, Disposer, ErrorCode, EventCallback, Handle, HostApi, HostCtx,
    PLUGIN_ENTRY_ABI_VERSION, PLUGIN_ENTRY_APPLY, PLUGIN_ENTRY_MANIFEST, PLUGIN_ENTRY_VALIDATE,
    PluginAbiVersion, PluginApply, PluginManifest, PluginManifestFn, PluginValidate, ToolExecute,
};
use cos_core::{Context, EffectHandle, EventPayload, JsonBridge};
use cos_session::ToolError;
use cos_tools::{Tool, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use serde_json::Value;

use crate::error::LoadError;

/// 一个已加载的 dlopen 插件（持有 Library：Drop 即卸载；函数指针随 Arc 存活）。
///
/// **卸载顺序不变量（P12 审计修复）**：插件效果（disposer/工具注销/监听器）的
/// 代码与数据位于库内——**库卸载前必须逆序注销**。显式路径（`LoadedApp::dispose`/
/// `dispose_async`）先 dispose fiber 再 drop 本实例；纯 RAII 路径（装载错误返回、
/// 未调 finish 直接 drop）由 [`Drop for DlopenPlugin`] 兜底：字段 drop（`_library`
/// 卸载）之前先 dispose 状态 fiber。该路径曾因 `_library` 字段先于 `state` drop，
/// 使 `state.ctx` 的 `Fiber::Drop`（逆序执行插件 disposer）发生在库卸载**之后**——
/// 执行已卸载代码 → 访问违例。
pub struct DlopenPlugin {
    /// 工厂名（yml `name`，原样）。
    pub name: String,
    /// 插件清单（`cos_plugin_manifest`；缺省 = 空清单——旧行为不裁剪）。
    manifest: PluginManifest,
    /// 配置预校验入口（`cos_plugin_validate`，可选，P12）；缺失 = 跳过预校验。
    validate: Option<PluginValidate>,
    _library: libloading::Library,
    apply: PluginApply,
    /// 宿主状态（apply 时创建；与实例同生命周期——插件自持的 ctx/host 指针
    /// 在卸载前始终有效，P9）。
    state: Mutex<Option<Arc<PluginHostState>>>,
}

impl Drop for DlopenPlugin {
    fn drop(&mut self) {
        // 纯 RAII 兜底：库随 `_library` 字段卸载前，先逆序注销状态 fiber 上的
        // 插件效果（disposer/工具注销/监听器）——dispose 幂等，显式卸载路径
        // （LoadedApp::dispose / dispose_async）随后调用是无操作。
        if let Some(state) = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            state.ctx.fiber().dispose();
        }
    }
}

impl DlopenPlugin {
    /// 按路径加载：dlopen → 版本握手 → 清单（可选）→ 解析 apply 入口。
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
            // 清单符号可选：缺失 = 旧行为（无依赖声明、不裁剪能力）
            let manifest = match library.get(PLUGIN_ENTRY_MANIFEST.as_bytes()) {
                Ok(manifest_symbol) => {
                    let manifest_fn: PluginManifestFn = *manifest_symbol;
                    let raw = manifest_fn();
                    if raw.is_null() {
                        default_manifest(name)
                    } else {
                        parse_manifest(name, Some(&CStr::from_ptr(raw).to_string_lossy()))?
                    }
                }
                Err(_) => default_manifest(name),
            };
            // P12：清单 `api` 字段强制（不兼容 fail loud；缺省/非法 = 按当前版本对待）
            check_manifest_api(name, &manifest)?;
            // P12：配置预校验入口可选（缺失 = 跳过；apply 前调用，见 `apply`）
            let validate: Option<PluginValidate> =
                match library.get(PLUGIN_ENTRY_VALIDATE.as_bytes()) {
                    Ok(symbol) => Some(*symbol),
                    Err(_) => None,
                };
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
                manifest,
                validate,
                _library: library,
                apply,
                state: Mutex::new(None),
            }))
        }
    }

    /// 插件清单（`inject`/`provide` 声明；无清单符号 = 空清单）。
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
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
        // P10 能力裁剪：有清单 → get_service 只对 inject 声明生效；无清单 = 不裁剪
        let allowed = if self.manifest.inject.is_empty() && self.manifest.provide.is_empty() {
            None
        } else {
            Some(self.manifest.inject.clone())
        };
        let state = Arc::new(PluginHostState {
            ctx: ctx.clone(),
            host_api: Box::new(build_host_api()),
            bridges,
            allowed,
            strings: Mutex::new(Vec::new()),
            handles: Mutex::new(HashMap::new()),
        });
        *self.state.lock().unwrap() = Some(state.clone());
        let config_json =
            CString::new(serde_json::to_string(&config).map_err(|error| {
                LoadError::Other(format!("dlopen 插件配置序列化失败: {error}"))
            })?)
            .map_err(|_| LoadError::Other("配置含 NUL".into()))?;
        // P12：配置预校验（可选入口；apply 之前调用，非零返回 → fail loud）。
        // 状态已建（validate 不依赖 ctx；失败路径状态保留，同 apply 失败语义）。
        if let Some(validate) = self.validate {
            let mut error_buf = vec![0u8; 4096];
            let code = unsafe {
                (validate)(
                    config_json.as_ptr(),
                    error_buf.as_mut_ptr() as *mut c_char,
                    error_buf.len(),
                )
            };
            if code != ErrorCode::Ok as i32 {
                let message = unsafe { CStr::from_ptr(error_buf.as_ptr() as *const c_char) }
                    .to_string_lossy()
                    .into_owned();
                return Err(LoadError::DlopenValidate {
                    name: self.name.clone(),
                    code,
                    message,
                });
            }
        }
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
                // 状态**保留**到实例 drop（apply 失败也走同一卸载链）：
                // 插件失败前可能已注册监听器/工具，其持有驻留区字符串；
                // 此处立即丢弃状态会先于 fiber 注销释放字符串（悬垂）。
                // 实例 drop 顺序：fork context（Fiber::Drop 逆序注销）→ 状态
                // （ctx 字段先 drop，strings 最后 drop）——安全不变量见
                // `PluginHostState.strings` 字段文档。
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

/// 无清单符号的插件：空清单（`id` = 工厂名；无依赖声明——旧行为）。
fn default_manifest(name: &str) -> PluginManifest {
    PluginManifest {
        id: name.to_string(),
        version: String::new(),
        api: None,
        inject: Vec::new(),
        provide: Vec::new(),
    }
}

/// 解析清单 JSON（非法 → fail loud；`None` → 空清单）。
pub fn parse_manifest(name: &str, raw: Option<&str>) -> Result<PluginManifest, LoadError> {
    let Some(raw) = raw else {
        return Ok(default_manifest(name));
    };
    serde_json::from_str(raw).map_err(|error| {
        LoadError::Other(format!("dlopen 插件 '{name}' 的清单不是合法 JSON: {error}"))
    })
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

/// 清单 `api` 字段强制（P12）：解析出版本且不兼容 → fail loud；
/// 缺省/非法字符串 = 按当前版本对待（纯函数，单元测试覆盖）。
fn check_manifest_api(name: &str, manifest: &PluginManifest) -> Result<(), LoadError> {
    match manifest.api_version() {
        Some(declared) => check_version(name, declared),
        None => Ok(()),
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
    /// **能力裁剪**（P10）：`Some(清单 inject)` = get_service 只对这些服务生效；
    /// `None` = 无清单（旧行为，不裁剪）。
    allowed: Option<Vec<String>>,
    /// **字符串驻留区**（P11）：跨边界 `&'static str`（事件名/工具名/描述）的宿主侧
    /// 所有权——去重驻留，随状态 drop（卸载）释放。**字段顺序即安全不变量**：
    /// `ctx`（首字段，drop 时经 `Fiber::Drop` 逆序注销监听器/工具注销效果）必须先于
    /// `strings`（末字段）drop——消费者引用全部消失后才释放字符串；任何字段重排
    /// 都必须保持 `ctx` 在 `strings` 之前。
    strings: Mutex<Vec<Box<str>>>,
    /// **句柄注册表**（P11）：`on` / `register_effect` / `register_tool` 的存活句柄
    /// ——`free` 据此校验并诚实回收（监听/效果提前 dispose、工具注销）；
    /// 未知/外来/重复句柄 → 幂等无操作。
    handles: Mutex<HashMap<Handle, HandleKind>>,
}

/// 存活句柄的回收方式（`free` 分发；统一为 [`EffectHandle`]——dispose 幂等，
/// fiber 里未消费的克隆随后自动无操作）。
enum HandleKind {
    /// `on` 注册的监听器（`ctx.on` 返回的效果句柄）。
    Listener(EffectHandle),
    /// `register_effect` 注册的效果。
    Effect(EffectHandle),
    /// `register_tool` 注册的工具（注销效果；dispose → 从 ToolRegistry 移除）。
    Tool(EffectHandle),
}

impl PluginHostState {
    /// 跨边界 C 字符串 → 插件级驻留的 `&'static str`（去重：相同文本只分配一次）。
    ///
    /// # Safety
    /// 返回的引用指向驻留区 Box 的堆分配，存活到状态 drop；调用方必须保证该引用
    /// 的消费者（监听器闭包/工具对象）在状态 drop 前已注销——卸载路径保证
    /// （fiber 逆序 dispose 先于状态 drop；`DlopenTool` 持状态 Arc 兜底）。
    fn arena_str(&self, ptr: *const c_char) -> &'static str {
        if ptr.is_null() {
            return "";
        }
        let text = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        let mut strings = self.strings.lock().unwrap();
        if let Some(existing) = strings.iter().find(|s| s.as_ref() == text) {
            // 驻留命中：Box 堆分配稳定，借出 'static 视图
            return unsafe { &*(existing.as_ref() as *const str) };
        }
        strings.push(text.into_boxed_str());
        let last = strings.last().unwrap();
        unsafe { &*(last.as_ref() as *const str) }
    }
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
    // P10 能力裁剪：清单声明了 inject → 未注入的服务一律空指针（插件侧 fail loud）；
    // 无清单（旧行为）→ 不过滤
    if let Some(allowed) = &state.allowed
        && !allowed.contains(&name)
    {
        return ptr::null_mut();
    }
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
    if ctx.is_null() || name.is_null() || payload.is_null() {
        return;
    }
    let Some(state) = (unsafe { (ctx as *const PluginHostState).as_ref() }) else {
        return;
    };
    let name = state.arena_str(name);
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
    let name = state.arena_str(name);
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    // 注意：精确捕获分析会捕获 `userdata.0` 字段而非整个包装 → 裸指针操作全部移出闭包
    let userdata = SendUserdata(userdata);
    let Ok(effect) = state.ctx.on(name, move |payload: &EventPayload| {
        let json = match payload.downcast_ref::<PluginEvent>() {
            Some(event) => serde_json::to_string(&event.0).unwrap_or_else(|_| "{}".into()),
            None => "{}".into(),
        };
        let json = CString::new(json).unwrap_or_default();
        invoke_callback(callback, userdata, &json);
    }) else {
        // fiber 已卸载等注册失败 → fail loud（插件侧检查 handle == 0）
        return 0;
    };
    state
        .handles
        .lock()
        .unwrap()
        .insert(handle, HandleKind::Listener(effect));
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
    if state.ctx.fiber().is_disposed() {
        return 0;
    }
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let userdata = SendUserdata(userdata);
    let effect = cos_core::EffectHandle::new(move || invoke_disposer(disposer, userdata));
    state.ctx.fiber().push(effect.clone());
    state
        .handles
        .lock()
        .unwrap()
        .insert(handle, HandleKind::Effect(effect));
    handle
}

/// disposer 调用（裸指针操作，闭包外）。
fn invoke_disposer(disposer: Disposer, userdata: SendUserdata) {
    unsafe {
        disposer(userdata.0);
    }
}

/// `free`（P11 诚实回收）：句柄注册表校验——
/// - 监听/效果句柄 → dispose（提前反注册；fiber 里的克隆随后自动无操作）；
/// - 工具句柄 → 注销效果 dispose（从 ToolRegistry 移除）；
/// - 未知/外来/重复/0 句柄 → 幂等无操作（插件不得重复 free，但重复无害）。
extern "C" fn host_free(ctx: HostCtx, handle: Handle) {
    let Some(state) = state_of(ctx) else {
        return;
    };
    if handle == 0 {
        return;
    }
    let kind = state.handles.lock().unwrap().remove(&handle);
    match kind {
        Some(HandleKind::Listener(effect))
        | Some(HandleKind::Effect(effect))
        | Some(HandleKind::Tool(effect)) => effect.dispose(),
        None => {}
    }
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
    if state.ctx.fiber().is_disposed() {
        return 0;
    }
    // P11：工具名/描述由插件级驻留区持有（随状态释放；卸载链先注销工具——
    // 见 `PluginHostState.strings` 字段文档的安全不变量）
    let name = state.arena_str(name);
    let description = state.arena_str(description);
    let parameters = if parameters_json.is_null() {
        Value::Null
    } else {
        let parameters = unsafe { CStr::from_ptr(parameters_json) }
            .to_string_lossy()
            .to_string();
        serde_json::from_str(&parameters).unwrap_or(Value::Null)
    };
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let tool = DlopenTool {
        name,
        description,
        parameters,
        execute,
        userdata,
    };
    let registry = match state.ctx.get::<ToolRegistry>() {
        Ok(registry) => registry,
        Err(error) => {
            // tools 服务未装配 → fail loud（插件侧检查 handle == 0）
            let _ = error;
            return 0;
        }
    };
    if registry.register(std::sync::Arc::new(tool)).is_err() {
        eprintln!("DEBUG host_register_tool: 同名工具已注册");
        return 0;
    }
    // P11：注销效果挂到插件 fiber——卸载/回滚自动反注册，不留僵尸工具；
    // `free(handle)` 提前 dispose 同一效果即提前注销。
    let name_for_unregister = name;
    let registry_for_unregister = Arc::clone(&registry);
    let unregister = cos_core::EffectHandle::new(move || {
        registry_for_unregister.unregister(name_for_unregister)
    });
    state.ctx.fiber().push(unregister.clone());
    state
        .handles
        .lock()
        .unwrap()
        .insert(handle, HandleKind::Tool(unregister));
    handle
}

/// 工具句柄计数器（on / register_effect / register_tool 共用）。
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

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
            allowed: None,
            strings: Mutex::new(Vec::new()),
            handles: Mutex::new(HashMap::new()),
        })
    }

    /// 带裁剪的状态：只允许 "echo"（模拟清单 inject: ["echo"]）。
    fn state_pruned() -> Arc<PluginHostState> {
        Arc::new(PluginHostState {
            ctx: Context::root(),
            host_api: Box::new(build_host_api()),
            bridges: vec![("echo", Arc::new(EchoBridge) as Arc<dyn JsonBridge>)],
            allowed: Some(vec!["echo".into()]),
            strings: Mutex::new(Vec::new()),
            handles: Mutex::new(HashMap::new()),
        })
    }

    /// 带 tools 服务的状态：ToolRegistry 装配在**插件 fork ctx** 上（反注册效果
    /// 挂在 fork fiber——随状态存活；装配在 root 上会在 root 局部 drop 时
    /// 触发 Fiber::Drop 反注册、把服务从共享注册表移除）。
    fn state_with_tools() -> (Arc<PluginHostState>, Arc<ToolRegistry>) {
        let root = Context::root();
        let state = Arc::new(PluginHostState {
            ctx: root.fork(),
            host_api: Box::new(build_host_api()),
            bridges: Vec::new(),
            allowed: None,
            strings: Mutex::new(Vec::new()),
            handles: Mutex::new(HashMap::new()),
        });
        state.ctx.provide(ToolRegistry::new(&state.ctx)).unwrap();
        let registry = state.ctx.get::<ToolRegistry>().unwrap();
        (state, registry)
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

    /// P10 能力裁剪：清单 inject 声明了 "echo" → echo 可取、其他服务一律空指针。
    #[test]
    fn get_service_is_pruned_to_manifest_inject() {
        let state = state_pruned();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        // 注入的服务可取
        assert!(!host_get_service(ctx, cptr("echo")).is_null());
        // 未注入 → 空指针（即使桥里有）
        assert!(host_get_service(ctx, cptr("nope")).is_null());
    }

    /// P10 向后兼容：无清单（allowed = None）→ 不裁剪。
    #[test]
    fn get_service_legacy_without_manifest_is_unpruned() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        assert!(!host_get_service(ctx, cptr("echo")).is_null());
    }

    /// P10 清单解析：合法 JSON → 结构；非法 JSON → fail loud；None → 空清单。
    #[test]
    fn manifest_parsing_defaults_and_errors() {
        let manifest = parse_manifest(
            "p",
            Some(r#"{"id":"p","inject":["tools"],"provide":["dlopen-todo"]}"#),
        )
        .unwrap();
        assert_eq!(manifest.inject, vec!["tools"]);
        assert_eq!(manifest.provide, vec!["dlopen-todo"]);
        // None → 空清单（无清单符号的旧插件）
        let bare = parse_manifest("p", None).unwrap();
        assert!(bare.inject.is_empty() && bare.provide.is_empty());
        assert_eq!(bare.id, "p");
        // 非法 JSON → fail loud 且错误含插件名
        let error = parse_manifest("bad-plugin", Some("not json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("bad-plugin"), "{error}");
        assert!(error.contains("清单"), "{error}");
    }

    /// P12 清单 `api` 强制：解析出版本且不兼容 → fail loud；缺省/非法 → 按当前版本对待。
    #[test]
    fn manifest_api_field_is_enforced() {
        // 缺省 → 接受
        let bare = parse_manifest("p", None).unwrap();
        assert!(check_manifest_api("p", &bare).is_ok());
        // 与宿主一致 → 接受
        let current_api = format!(
            "{}.{}.{}",
            API_VERSION.major, API_VERSION.minor, API_VERSION.patch
        );
        let current =
            parse_manifest("p", Some(&format!(r#"{{"id":"p","api":"{current_api}"}}"#))).unwrap();
        assert!(check_manifest_api("p", &current).is_ok());
        // 旧版本（minor ≤ 宿主）→ 接受（向后兼容）
        let older = parse_manifest("p", Some(r#"{"id":"p","api":"0.3.0"}"#)).unwrap();
        assert!(check_manifest_api("p", &older).is_ok());
        // 不兼容（major 不同 / minor 超前）→ fail loud 且错误含插件名与版本
        for api in ["1.0.0", "0.5.0"] {
            let manifest =
                parse_manifest("p", Some(&format!(r#"{{"id":"p","api":"{api}"}}"#))).unwrap();
            let error = check_manifest_api("bad-plugin", &manifest)
                .unwrap_err()
                .to_string();
            assert!(error.contains("bad-plugin"), "{error}");
            assert!(error.contains("ABI"), "{error}");
        }
        // 非法版本字符串 → 按当前版本对待（接受）
        let junk = parse_manifest("p", Some(r#"{"id":"p","api":"x.y.z"}"#)).unwrap();
        assert!(check_manifest_api("p", &junk).is_ok());
    }

    // -----------------------------------------------------------------------
    // P11 资源生命周期：字符串驻留（去重、随状态释放）+ free 诚实回收 + 注销链
    // -----------------------------------------------------------------------

    extern "C" fn noop_execute(
        _userdata: *mut c_void,
        _run_json: *const c_char,
        _result_buf: *mut c_char,
        _result_len: usize,
    ) -> i32 {
        0
    }

    extern "C" fn count_callback(userdata: *mut c_void, _event_json: *const c_char) {
        unsafe { *(userdata as *mut usize) += 1 };
    }

    fn emit_ping(ctx: &Context) {
        ctx.emit("ping", std::sync::Arc::new(PluginEvent(Value::Null)));
    }

    /// free 应提前注销监听器（幂等；fiber 卸载路径随后无操作）。
    #[test]
    fn free_disposes_listener_early() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let mut calls = 0usize;
        let handle = host_on(
            ctx,
            cptr("ping"),
            count_callback,
            &mut calls as *mut usize as *mut c_void,
        );
        assert_ne!(handle, 0);
        emit_ping(&state.ctx);
        assert_eq!(calls, 1);
        host_free(ctx, handle);
        emit_ping(&state.ctx);
        assert_eq!(calls, 1, "free 后监听器应已注销");
        // 重复 free 幂等（无 panic、不再回收）
        host_free(ctx, handle);
        emit_ping(&state.ctx);
        assert_eq!(calls, 1);
    }

    /// 外来句柄（另一插件状态）/ 空 ctx / 0 句柄 → 幂等无操作，不影响本插件资源。
    #[test]
    fn free_with_foreign_or_null_handle_is_safe_noop() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let other = state_with_echo();
        let other_ctx = Arc::as_ptr(&other) as HostCtx;
        let mut calls = 0usize;
        let userdata = &mut calls as *mut usize as *mut c_void;
        let handle = host_on(ctx, cptr("ping"), count_callback, userdata);
        let foreign = host_on(other_ctx, cptr("ping"), count_callback, userdata);
        // 用本插件 ctx free 外来句柄 → 无操作
        host_free(ctx, foreign);
        emit_ping(&state.ctx);
        assert_eq!(calls, 1);
        // 空 ctx / 0 句柄 → 无操作
        host_free(std::ptr::null_mut(), handle);
        host_free(ctx, 0);
        emit_ping(&state.ctx);
        assert_eq!(calls, 2);
        // 真句柄 → 注销
        host_free(ctx, handle);
        emit_ping(&state.ctx);
        assert_eq!(calls, 2);
    }

    /// free(工具句柄) 应把工具从注册表移除。
    #[test]
    fn free_unregisters_tool() {
        let (state, registry) = state_with_tools();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let handle = host_register_tool(
            ctx,
            cptr("t1"),
            cptr("描述"),
            cptr("{}"),
            noop_execute,
            std::ptr::null_mut(),
        );
        assert_ne!(handle, 0);
        assert!(registry.get("t1").is_some());
        host_free(ctx, handle);
        assert!(registry.get("t1").is_none(), "free 应注销工具");
        // 重复 free 幂等
        host_free(ctx, handle);
    }

    /// fiber dispose（卸载）应自动注销工具与监听器；重载后同名工具可再注册
    /// （旧实现：泄漏的僵尸工具使同名注册 duplicate 失败）。
    #[test]
    fn fiber_dispose_unregisters_tools_and_reload_re_registers() {
        let root = Context::root();
        root.provide(ToolRegistry::new(&root)).unwrap();
        let registry = root.get::<ToolRegistry>().unwrap();
        let make_state = || {
            Arc::new(PluginHostState {
                ctx: root.fork(),
                host_api: Box::new(build_host_api()),
                bridges: Vec::new(),
                allowed: None,
                strings: Mutex::new(Vec::new()),
                handles: Mutex::new(HashMap::new()),
            })
        };
        // 第一个实例：注册工具 + 监听器 → 卸载（fiber dispose）
        let first = make_state();
        let first_ctx = Arc::as_ptr(&first) as HostCtx;
        let mut calls = 0usize;
        let listener = host_on(
            first_ctx,
            cptr("ping"),
            count_callback,
            &mut calls as *mut usize as *mut c_void,
        );
        assert_ne!(listener, 0);
        assert_ne!(
            host_register_tool(
                first_ctx,
                cptr("t1"),
                cptr("描述"),
                cptr("{}"),
                noop_execute,
                std::ptr::null_mut(),
            ),
            0
        );
        assert!(registry.get("t1").is_some());
        first.ctx.fiber().dispose();
        assert!(
            registry.get("t1").is_none(),
            "卸载应注销 dlopen 工具（无僵尸）"
        );
        emit_ping(&first.ctx);
        assert_eq!(calls, 0, "卸载后监听器应已移除");
        // 第二个实例（重载）：同名工具可再注册
        let second = make_state();
        let second_ctx = Arc::as_ptr(&second) as HostCtx;
        assert_ne!(
            host_register_tool(
                second_ctx,
                cptr("t1"),
                cptr("描述"),
                cptr("{}"),
                noop_execute,
                std::ptr::null_mut(),
            ),
            0,
            "重载后同名工具应可再注册"
        );
        assert!(registry.get("t1").is_some());
        second.ctx.fiber().dispose();
        assert!(registry.get("t1").is_none());
    }

    /// 工具名/描述进入插件级驻留区且去重；emit/on 的事件名同样驻留。
    #[test]
    fn strings_are_interned_in_state_arena() {
        let (state, _registry) = state_with_tools();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let handle = host_register_tool(
            ctx,
            cptr("t1"),
            cptr("描述"),
            cptr("{}"),
            noop_execute,
            std::ptr::null_mut(),
        );
        assert_ne!(handle, 0);
        let _ = host_on(ctx, cptr("ping"), count_callback, std::ptr::null_mut());
        let strings = state.strings.lock().unwrap();
        assert_eq!(
            strings.len(),
            3,
            "驻留区应含工具名/描述/事件名（各一份）: {strings:?}"
        );
        assert!(strings.iter().any(|s| s.as_ref() == "t1"));
        assert!(strings.iter().any(|s| s.as_ref() == "描述"));
        assert!(strings.iter().any(|s| s.as_ref() == "ping"));
    }

    /// 空指针参数 → 安全无操作（不再有未守卫的 CStr::from_ptr）。
    /// 注：fn 指针在 Rust 里保证非空（`useless_ptr_null_checks`），空回调/
    /// 空执行器属于插件侧 UB，宿主不设守卫（与 ABI 契约一致）。
    #[test]
    fn null_pointer_params_are_safe() {
        let (state, registry) = state_with_tools();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        // emit：空名/空载荷 → 无崩溃
        host_emit(ctx, cptr("x"), std::ptr::null());
        host_emit(ctx, std::ptr::null(), cptr("{}"));
        // register_tool：空参数 JSON → 以 Null 参数注册（不崩溃）
        let handle = host_register_tool(
            ctx,
            cptr("t1"),
            cptr("d"),
            std::ptr::null(),
            noop_execute,
            std::ptr::null_mut(),
        );
        assert_ne!(handle, 0);
        assert_eq!(registry.get("t1").unwrap().parameters(), Value::Null);
    }

    /// 注册效果句柄 free → disposer 提前执行（fiber 卸载路径随后无操作）。
    #[test]
    fn free_disposes_effect_early() {
        let state = state_with_echo();
        let ctx = Arc::as_ptr(&state) as HostCtx;
        let mut disposed = 0usize;
        extern "C" fn disposer_cb(userdata: *mut c_void) {
            unsafe { *(userdata as *mut usize) += 1 };
        }
        let handle =
            host_register_effect(ctx, disposer_cb, &mut disposed as *mut usize as *mut c_void);
        assert_ne!(handle, 0);
        host_free(ctx, handle);
        assert_eq!(disposed, 1, "free 应提前执行 disposer");
        // 卸载（fiber dispose）：同效果句柄的 fiber 克隆已无操作
        state.ctx.fiber().dispose();
        assert_eq!(disposed, 1);
    }
}
