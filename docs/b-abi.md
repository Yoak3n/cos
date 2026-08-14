# B-ABI 设计（B 形态插件 FFI 契约）

状态：**P7 草案**（PLAN.md P7 冻结项）。P8 以本表实现 `DlopenPluginSource` + 薄壳 cdylib。

## 1. 目标与范围

B 形态插件 = **独立编译的 cdylib**（Linux `.so` / Windows `.dll` / macOS `.dylib`），宿主启动时
用 libloading dlopen 加载。跨边界只允许：C ABI 函数指针、`c_char` 字符串（UTF-8 JSON）、
不透明指针、整数。Rust trait object、`Arc`、泛型一律不出边界。

A 形态（workspace crate + `plugin!` 编译期注册）与 B 形态（cdylib + 运行期 dlopen）共用同一
`Plugin` 语义（inject/provide/Config/apply），只是装载通道不同——`resolve_factory` 是唯一入口，
B 只加一个 source（PLAN §6）。

## 2. 选型：纯 C ABI（非 abi_stable）

| 方案 | 结论 |
|---|---|
| 纯 C ABI（`#[repr(C)]` 函数表 + 裸指针/JSON 字符串） | **采用**。FFI 面最小、无 proc-macro 依赖、Windows 符号可见性可控、`extern "C"` 是稳定契约 |
| abi_stable（`#[sabi]` 宏 + 类型擦除） | 不采用。引入宏生态依赖；其 String/Option 语义与"载荷全 JSON"的设计重叠；后续若需要可降级引入（契约已按纯 C 设计，表结构不变） |

边界载荷纪律：**一切结构化数据 = JSON 字符串**（serde 即 wire 契约，P7 审计已证明跨边界类型
全部可序列化）。字符串生命周期：宿主分配（错误缓冲）、插件只读不持有（事件 JSON）、
调用返回后即失效。

## 3. 版本协商

契约 crate `cos-contract`（crates/cos-contract）持有：

- `API_VERSION: ContractVersion`（当前 `0.1.0`）；`encode()` → u32（`major<<16|minor<<8|patch`）
- 兼容规则（`ContractVersion::compatible_with`）：**major 必须相等**；插件 `minor ≤` 宿主 `minor`
- HostApi 字段顺序即 ABI：**只许表尾追加（minor 递增）；重排/删改/语义变更 = major 递增**

装载流程：宿主 dlopen → 解析 `cos_plugin_abi_version` → 取插件版本 → 不兼容 → fail loud
（可读错误：宿主 x.y.z vs 插件 a.b.c）→ 兼容 → `cos_plugin_apply(host, config_json, err_buf, err_len)`。

## 4. 插件导出入口（cdylib 必须导出）

| 符号 | 签名 | 说明 |
|---|---|---|
| `cos_plugin_abi_version` | `fn() -> u32` | 返回 `API_VERSION.encode()`；宿主先握手 |
| `cos_plugin_apply` | `fn(*const HostApi, *const c_char, *mut c_char, usize) -> i32` | 执行注册；错误写 error_buf（UTF-8），返回 [`ErrorCode`] |
| `cos_plugin_validate`（可选，P9） | `fn(*const c_char, *mut c_char, usize) -> i32` | 配置预校验 |

## 5. HostApi 能力函数表（P7 定稿形态）

Rust 规范形态见 `cos_contract::HostApi`（`#[repr(C)]`，字段顺序 = ABI）：

| 字段 | 类型 | 语义 |
|---|---|---|
| `api_version` | `u32` | 宿主侧 `API_VERSION.encode()`（插件校验） |
| `get_service` | `fn(ctx, name) -> *const c_void` | 按名取服务（不透明句柄）；未注入/未注册 → 空指针 |
| `emit` | `fn(ctx, name, payload_json)` | 广播事件（同步分发；JSON 调用返回后失效） |
| `on` | `fn(ctx, name, callback, userdata) -> Handle` | 注册事件监听；`free` 注销 |
| `register_effect` | `fn(ctx, disposer, userdata) -> Handle` | 注册效果；卸载时**逆序**调用 disposer |
| `free` | `fn(ctx, handle)` | 释放 on/register_effect 的句柄 |

**能力按 inject 裁剪**：宿主在 apply 前读插件清单（B 形态 = 清单 JSON，见 §7），
`get_service` 只对清单 `inject` 声明的服务生效；未注入 → 空指针 + 错误码 `ServiceUnavailable`。
插件不得绕过（宿主侧强制）。

## 6. 谁分配谁释放（所有权纪律）

| 对象 | 分配 | 释放 | 约束 |
|---|---|---|---|
| HostApi 表 | 宿主（栈/堆） | 宿主 | 插件只读；apply 返回后失效 |
| config_json / payload_json | 宿主 | 宿主 | 插件只读；调用返回后失效 |
| error_buf | 宿主 | 宿主 | 插件写入（NUL 结尾 UTF-8），不扩容 |
| 服务句柄（get_service 返回值） | 宿主 | 宿主（随卸载） | 插件不得解引用/释放 |
| Handle（on/register_effect） | 宿主 | `free` 或卸载 | 插件不得重复 free |
| 事件回调 userdata | 插件 | 插件（disposer） | 宿主原样回传 |

## 7. 插件清单（JSON）

B 形态插件随 cdylib 交付清单 JSON（`PluginManifest`，cos_contract 定义）：
`{id, version, api: "major.minor.patch", inject: [...], provide: [...]}`。
宿主按 `inject` 裁剪 HostApi 能力；`api` 缺省 = 按当前版本对待（告警）。

## 8. 错误码（`ErrorCode`，i32）

`Ok=0` / `AbiMismatch=1` / `ConfigInvalid=2` / `ServiceUnavailable=3` / `ApplyFailed=4` /
`EffectRegistrationFailed=5` / `InvalidHandle=6`。详情文本一律走 error_buf。

## 9. 接缝对象安全与"小而窄"审计（P7 结论）

- **通过**：`Plugin`（元数据改实例方法 `id(&self)`，无关联常量）、`LlmAdapter`、`Agent`、
  `Tool`、`ToolGuard`、`Shell` —— 编译期证明见 `crates/cos-contract/tests/object_safety.rs`
- **豁免**：`Service` 保留关联常量 `NAME`——名字是 TypeId 登记键，必须在无实例的泛型路径
  （`get::<T>` 错误信息）可取；服务在 B 边界按名字字符串 + 不透明句柄传递，从不作 trait object。
  若未来需要 `dyn Service`，改法：`fn name(&self)` + 注册表反向查名（错误路径降级为占位名）
- **跨边界类型**：UserMessage / StreamChunk / AssistantMessage / ToolResultMessage /
  LlmRequest / ToolCall / SessionEventData / SessionEvent / ToolRun / ToolOutcome /
  PluginManifest 全部 serde 可序列化（审计测试断言）——P7 顺手补齐了 `ToolRun`/`ToolOutcome` 的 derive
- **"小而窄"**：接缝方法参数/返回值均为标量或上述 JSON 类型；无公开泛型方法（泛型只出现在
  服务注册表内部，如 `provide<T: Service>`，不跨边界）

## 10. P8 试点（已落地）

状态：**完成**（commit 后）。实现：

1. **HostApi 0.2.0**：表尾追加 `register_tool`（工具注册能力：ToolRun JSON → C 回调 →
   ToolOutcome JSON 写宿主缓冲）；`cos_plugin_apply` 增加 `ctx` 参数（插件后续
   调用 host 函数时回传）
2. **`cos-loader::dlopen`**（DlopenPluginSource）：`resolve_factory` 之外的新路径——
   yml `name` 以 `./` 或 `dlopen:` 开头 → libloading 加载 → `cos_plugin_abi_version`
   握手（不兼容 fail loud，可读错误）→ `cos_plugin_apply(host, ctx, config, err_buf, err_len)`
3. **HostApi 桥**（loader 实现）：`get_service` 恒空指针（P8 未桥接）、`emit`/`on`
   （JSON 载荷事件 `PluginEvent`；非 JSON 载荷不可见）、`register_effect`（fiber 效果，
   卸载逆序调 disposer）、`free`（P8 空操作，句柄随 fiber 回收）、`register_tool`
4. **薄壳 `plugins/plugin-todo-dlopen`**（cdylib）：导出两个入口；apply 时注册
   todo_write 工具（C 回调执行：解析 ToolRun → 更新状态 → 回写 ToolOutcome）、
   注册效果（释放状态 + 写 marker 文件验证卸载链）、emit 事件验证桥
5. **装载集成**：`compose::plan/load` 支持 `FactoryRef::Static | Dlopen`；
   `LoadedPlugin` 持有 dlopen 库句柄（**Library 存活到实例 Drop**——注册的工具/效果
   持有库内指针，先 fiber 逆序注销再卸载库，顺序已钉死）

P8 简化（记录在案）：能力不按 inject 裁剪（所有 dlopen 插件获得同一能力集，裁剪留 P9）；
事件名跨边界泄漏（`&'static str` 契约）；`free` 空操作；dlopen 插件的清单（inject/provide）
暂为空（拓扑排序按 entry 自身声明）。

验证（`tests/dlopen_e2e.rs`）：`- name: ./target/debug/plugin_todo_dlopen.dll` 装载 →
demo 模式 mock LLM 调用 todo_write → dlopen 工具经 C 回调执行 → 结果
（"已写入 1 条任务"）回流会话日志 → 不变量全过 → 卸载逆序 → disposer 写 marker。

## 11. 安全边界（P9 前不做）

验签/哈希校验、沙箱、运行时重载——明确不做（PLAN §0 非目标）。P8 只证明机制，不声称安全。
