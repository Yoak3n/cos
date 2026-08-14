//! B 形态薄壳试点（P8，P9 服务直连演示）：独立 cdylib 形式的 todo 工具。
//!
//! 导出 `cos_plugin_abi_version` / `cos_plugin_apply`；apply 时经 HostApi 注册
//! `todo_write` 工具 + 卸载效果（释放状态、写 marker 文件验证 disposer 调用链）+
//! 事件（验证 emit 桥）。工具执行 = C 回调：解析 ToolRun JSON → 更新状态 →
//! 把 ToolOutcome JSON 写入宿主缓冲。
//!
//! P9 服务直连演示：apply 时自持 `ctx`/`host` 指针（宿主保证与插件实例同生命周期），
//! 工具回调内经 `get_service("tools")` + `service_call("list")` 查询宿主工具清单，
//! 把数量并入结果文本（端到端验证 JSON 桥）。
//!
//! 纪律：所有 `extern "C"` 入口用 `catch_unwind` 包裹（panic 不跨 FFI）；
//! 不依赖宿主任何 Rust 类型（只经 B-ABI 契约交互）。

#![warn(missing_docs)]

use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

use cos_contract::{API_VERSION, ErrorCode, HostApi, HostCtx};
use cos_tools::{ToolOutcome, ToolRun};
use serde_json::Value;

/// 工具状态（userdata 指向；disposer 释放）。
struct TodoState {
    count: usize,
    /// P9：apply 时自持的宿主 ctx/host（与插件实例同生命周期；工具回调内服务直连用）。
    host: Option<(HostCtx, *const HostApi)>,
}

/// disposer 载荷：工具状态 + marker 路径（卸载验证用）。
struct DisposerData(*mut TodoState, Option<String>);

/// 导出：B-ABI 版本（宿主握手）。
#[unsafe(no_mangle)]
pub extern "C" fn cos_plugin_abi_version() -> u32 {
    API_VERSION.encode()
}

/// 导出：apply（HostApi 桥 + 配置 JSON + 错误缓冲）。
///
/// # Safety
/// `host` 必须是宿主填充的合法 [`HostApi`] 指针（apply 期间有效）；`config_json`
/// 必须是 NUL 结尾 UTF-8（可为空指针，视为空配置）；`error_buf` 由宿主分配、
/// 容量为 `error_len`，插件最多写入 `error_len - 1` 字节 + NUL。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cos_plugin_apply(
    host: *const HostApi,
    ctx: HostCtx,
    config_json: *const c_char,
    error_buf: *mut c_char,
    error_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        apply(host, ctx, config_json, error_buf, error_len)
    }));
    match result {
        Ok(code) => code,
        Err(_) => {
            write_text(error_buf, error_len, "插件内部 panic");
            ErrorCode::ApplyFailed as i32
        }
    }
}

fn apply(
    host: *const HostApi,
    ctx: HostCtx,
    config_json: *const c_char,
    error_buf: *mut c_char,
    error_len: usize,
) -> i32 {
    if host.is_null() {
        write_text(error_buf, error_len, "host 为空");
        return ErrorCode::ApplyFailed as i32;
    }
    let host = unsafe { &*host };
    let config: Value = read_json(config_json).unwrap_or(Value::Null);
    let marker = config
        .get("marker")
        .and_then(Value::as_str)
        .map(str::to_string);

    // 1. 注册工具（userdata = 状态指针；自持 ctx/host 供工具回调内服务直连）
    let state = Box::into_raw(Box::new(TodoState {
        count: 0,
        host: Some((ctx, host)),
    }));
    let name = cstr("todo_write");
    let description = cstr("写入任务清单（B 形态薄壳）");
    let parameters = cstr(
        r#"{"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string"}}}}},"required":["todos"]}"#,
    );
    let handle = unsafe {
        (host.register_tool)(
            ctx,
            name.as_ptr(),
            description.as_ptr(),
            parameters.as_ptr(),
            todo_execute,
            state as *mut c_void,
        )
    };
    if handle == 0 {
        write_text(error_buf, error_len, "工具注册失败");
        unsafe { drop(Box::from_raw(state)) };
        return ErrorCode::ApplyFailed as i32;
    }

    // 2. 注册效果：释放状态 + 写 marker（卸载时宿主逆序调用）
    let disposer_data = Box::into_raw(Box::new(DisposerData(state, marker)));
    unsafe {
        (host.register_effect)(ctx, todo_dispose, disposer_data as *mut c_void);
    }

    // 3. 事件（验证 emit 桥）
    unsafe {
        (host.emit)(ctx, cstr("dlopen/todo-ready").as_ptr(), cstr("{}").as_ptr());
    }
    0
}

/// 工具执行：ToolRun JSON → 状态更新 → ToolOutcome JSON 写回宿主缓冲。
extern "C" fn todo_execute(
    userdata: *mut c_void,
    run_json: *const c_char,
    result_buf: *mut c_char,
    result_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let state = unsafe { &mut *(userdata as *mut TodoState) };
        let run: ToolRun = read_json(run_json).ok_or("run JSON 无效")?;
        let todos = run
            .arguments
            .get("todos")
            .and_then(Value::as_array)
            .ok_or("参数缺少 todos")?;
        state.count += todos.len();
        // P9 服务直连：查询宿主工具清单（tools 桥 list），并入结果文本
        let tools = tools_count(state.host);
        let outcome = match tools {
            Some(count) => {
                ToolOutcome::ok(format!("已写入 {} 条任务（tools={count}）", todos.len()))
            }
            None => ToolOutcome::ok(format!("已写入 {} 条任务", todos.len())),
        };
        let json = serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".into());
        write_text(result_buf, result_len, &json);
        Ok::<i32, String>(0)
    }));
    match result {
        Ok(Ok(code)) => code,
        Ok(Err(message)) => {
            write_text(result_buf, result_len, &message);
            1
        }
        Err(_) => {
            write_text(result_buf, result_len, "execute panic");
            1
        }
    }
}

/// P9 服务直连：经 `get_service("tools")` + `service_call("list")` 查询宿主工具清单数量。
/// 任一环节失败（未注册桥/调用失败/结果非法）→ `None`（工具本身不受影响，诚实降级）。
fn tools_count(host: Option<(HostCtx, *const HostApi)>) -> Option<usize> {
    let (ctx, host_ptr) = host?;
    let host = unsafe { &*host_ptr };
    let service = unsafe { (host.get_service)(ctx, cstr("tools").as_ptr()) };
    if service.is_null() {
        return None;
    }
    let mut buf = vec![0u8; 8192];
    let code = unsafe {
        (host.service_call)(
            ctx,
            service,
            cstr("list").as_ptr(),
            cstr("{}").as_ptr(),
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    if code != ErrorCode::Ok as i32 {
        return None;
    }
    let raw = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
        .to_string_lossy()
        .to_string();
    let tools: Value = serde_json::from_str(&raw).ok()?;
    tools.as_array().map(|tools| tools.len())
}

/// 卸载效果：释放状态 + 写 marker 文件。
extern "C" fn todo_dispose(userdata: *mut c_void) {
    let data = unsafe { *Box::from_raw(userdata as *mut DisposerData) };
    unsafe { drop(Box::from_raw(data.0)) };
    if let Some(marker) = data.1 {
        let _ = std::fs::write(marker, "disposed");
    }
}

/// 读 C JSON 字符串 → 反序列化为 T（非法/空 → None）。
fn read_json<T: serde::de::DeserializeOwned>(ptr: *const c_char) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
    serde_json::from_str(&text).ok()
}

/// 写入宿主缓冲（NUL 结尾；超长截断安全）。
fn write_text(buf: *mut c_char, len: usize, text: &str) {
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

/// 静态 C 字符串（泄漏一次、进程内复用；apply 期调用次数有限）。
fn cstr(text: &'static str) -> &'static std::ffi::CStr {
    let leaked: &'static str = Box::leak(format!("{text}\0").into_boxed_str());
    CStr::from_bytes_with_nul(leaked.as_bytes()).expect("构造的 C 字符串合法")
}
