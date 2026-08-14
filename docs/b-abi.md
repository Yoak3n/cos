# B-ABI 设计（B 形态插件 FFI 契约）

状态：**P7 草案**（PLAN.md P7 冻结项）。P8 以本表实现 `DlopenPluginSource` + 薄壳 cdylib；
P9 完成 `get_service`/`service_call` 服务桥接（HostApi 0.3.0，见 §10.5）。

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
| `get_service` | `fn(ctx, name) -> *const c_void` | 按名取服务（不透明句柄）；未注册 → 空指针 |
| `emit` | `fn(ctx, name, payload_json)` | 广播事件（同步分发；JSON 调用返回后失效） |
| `on` | `fn(ctx, name, callback, userdata) -> Handle` | 注册事件监听；`free` 注销 |
| `register_effect` | `fn(ctx, disposer, userdata) -> Handle` | 注册效果；卸载时**逆序**调用 disposer |
| `free` | `fn(ctx, handle)` | 释放 on/register_effect 的句柄 |
| `register_tool`（0.2.0 追加，P8） | `fn(ctx, name, description, parameters_json, execute, userdata) -> Handle` | 注册工具：执行时 ToolRun JSON → C 回调 → ToolOutcome JSON 写宿主缓冲 |
| `service_call`（0.3.0 追加，P9） | `fn(ctx, service, method, args_json, result_buf, result_len) -> i32` | 调用 `get_service` 返回的句柄（身份校验；伪造/悬垂 → `InvalidHandle`）；method + args JSON → 结果 JSON 写宿主缓冲；失败写错误文本并返回非零 |

**能力按 inject 裁剪（P9 前不做）**：当前所有 dlopen 插件获得同一能力集；`get_service`
只对注册进 [`BridgeRegistry`]（服务 `"bridges"`）的 JSON 桥生效，未注册 → 空指针。
按清单 `inject` 裁剪留 P9 后续（清单字段已就位）。

## 6. 谁分配谁释放（所有权纪律）

| 对象 | 分配 | 释放 | 约束 |
|---|---|---|---|
| HostApi 表 | 宿主（堆，随插件状态） | 宿主（随卸载） | 插件只读；**P9 起与插件实例同生命周期**（插件可自持 ctx/host 指针） |
| config_json / payload_json | 宿主 | 宿主 | 插件只读；调用返回后失效 |
| error_buf / result_buf | 宿主 | 宿主 | 插件写入（NUL 结尾 UTF-8），不扩容 |
| 服务句柄（get_service 返回值） | 宿主 | 宿主（随卸载） | 插件不得解引用/释放；仅可回传 `service_call` |
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
`--llm-*` 指向本地回环 chat/completions 服务器（cos-test-support，脚本：tool_use 调
todo_write → 文本）→ dlopen 工具经 C 回调执行 → 结果（"已写入 1 条任务"）回流会话日志
→ 不变量全过 → 卸载逆序 → disposer 写 marker。

## 10.5 P9 服务桥接（已落地）

状态：**完成**。`get_service` 从恒空指针变为真实服务查找，配套新增 `service_call`：

1. **HostApi 0.3.0**：表尾追加 `service_call`（method + args JSON → 结果 JSON 写宿主缓冲；
   服务句柄身份校验：伪造/悬垂 → `InvalidHandle`；桥调用失败 → `CallFailed`（=7，错误文本入缓冲））
2. **`cos-core::JsonBridge` / `BridgeRegistry`**（服务 `"bridges"`）：宿主侧 JSON 桥接口与注册表；
   桥名约定 = `Service::NAME`。`JsonBridge` 不继承 `Service`（其关联常量 `NAME` 使 trait
   非 dyn-compatible，见 §9 豁免条款），对象安全约束为 `Send + Sync + 'static`
3. **内置桥**：`cos-tools` 为 `ToolRegistry` 实现桥（`list` → 工具清单 JSON）；
   `cos-llm` 为 `LlmRegistry` 实现桥（`kinds` / `supports`）；宿主 `assemble` 装配时注册。
   第三方服务实现 `JsonBridge` 后经 `ctx.get::<BridgeRegistry>()?.register(...)` 开放给 B 形态
4. **宿主状态生命周期**：`PluginHostState`（HostCtx 指向）改为与插件实例同生命周期
   （`DlopenPlugin` 持有）；HostApi 表随状态存活——插件可自持 ctx/host 指针，
   在工具回调等 apply 之后的时机调用 host 函数（服务直连、事件等）
5. **桥快照**：dlopen 装载时对 `BridgeRegistry` 做 Arc 快照，`get_service` 返回的指针
   指向快照内 Arc 的分配——注册方卸载不会使指针悬垂
6. **薄壳演示**：`plugin-todo-dlopen` 工具回调内经 `get_service("tools")` +
   `service_call("list")` 查询宿主工具清单，数量并入结果文本（"tools=N"），
   `tests/dlopen_e2e.rs` 断言回流会话日志（端到端验证整条 JSON 桥链路）

P9 未做（记录在案）：能力按清单 `inject` 裁剪（当前同一能力集）；`cos_plugin_validate` 入口；
服务句柄的按名缓存与跨插件复用优化。

## 11. 安全边界（明确不做）

验签/哈希校验、沙箱、运行时重载——明确不做（PLAN §0 非目标）。P8/P9 只证明机制，不声称安全。
