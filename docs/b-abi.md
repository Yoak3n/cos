# B-ABI 设计（B 形态插件 FFI 契约）

状态：**P7 草案**（PLAN.md P7 冻结项）。P8 以本表实现 `DlopenPluginSource` + 薄壳 cdylib；
P9 完成 `get_service`/`service_call` 服务桥接（HostApi 0.3.0，见 §10.5）；
P10 清单一等公民（0.4.0，见 §10.6）；P11 资源生命周期（见 §10.7）；
P12 暴露面审计（validate 兑现 / 清单 api 强制 / RAII 卸载顺序修复，见 §12）。

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

- `API_VERSION: ContractVersion`（当前 `0.4.0`）；`encode()` → u32（`major<<16|minor<<8|patch`）
- 兼容规则（`ContractVersion::compatible_with`）：**major 必须相等**；插件 `minor ≤` 宿主 `minor`
- HostApi 字段顺序即 ABI：**只许表尾追加（minor 递增）；重排/删改/语义变更 = major 递增**

装载流程：宿主 dlopen → 解析 `cos_plugin_abi_version` → 取插件版本 → 不兼容 → fail loud
（可读错误：宿主 x.y.z vs 插件 a.b.c）→ 兼容 → 解析 `cos_plugin_manifest`（可选；清单 `api`
字段强制，P12）→ 解析 `cos_plugin_validate`（可选，P12）→
`cos_plugin_apply(host, ctx, config_json, err_buf, err_len)`（apply 前先调 validate）。

## 4. 插件导出入口（cdylib 必须导出）

| 符号 | 签名 | 说明 |
|---|---|---|
| `cos_plugin_abi_version` | `fn() -> u32` | 返回 `API_VERSION.encode()`；宿主先握手 |
| `cos_plugin_manifest`（可选，0.4.0/P10） | `fn() -> *const c_char` | 返回 NUL 结尾清单 JSON（`{id, version, api, inject, provide}`）；宿主据此建依赖图 + 裁剪能力；缺失 = 旧行为 |
| `cos_plugin_apply` | `fn(*const HostApi, HostCtx, *const c_char, *mut c_char, usize) -> i32` | 执行注册；错误写 error_buf（UTF-8），返回 [`ErrorCode`] |
| `cos_plugin_validate`（可选，P12 起兑现） | `fn(*const c_char, *mut c_char, usize) -> i32` | 配置预校验（apply 之前宿主调用；非零 → 装载失败 fail loud，错误文本入 error_buf）；缺失 = 跳过 |

## 5. HostApi 能力函数表（P7 定稿形态）

Rust 规范形态见 `cos_contract::HostApi`（`#[repr(C)]`，字段顺序 = ABI）：

| 字段 | 类型 | 语义 |
|---|---|---|
| `api_version` | `u32` | 宿主侧 `API_VERSION.encode()`（插件校验） |
| `get_service` | `fn(ctx, name) -> *const c_void` | 按名取服务（不透明句柄）；未注册 → 空指针 |
| `emit` | `fn(ctx, name, payload_json)` | 广播事件（同步分发；JSON 调用返回后失效） |
| `on` | `fn(ctx, name, callback, userdata) -> Handle` | 注册事件监听；`free` 注销 |
| `register_effect` | `fn(ctx, disposer, userdata) -> Handle` | 注册效果；卸载时**逆序**调用 disposer |
| `free` | `fn(ctx, handle)` | 释放 on/register_effect/register_tool 的句柄（**0.4.0 起诚实回收**，P11）：监听/效果提前 dispose、工具注销；未知/外来/重复句柄 = 幂等无操作 |
| `register_tool`（0.2.0 追加，P8） | `fn(ctx, name, description, parameters_json, execute, userdata) -> Handle` | 注册工具：执行时 ToolRun JSON → C 回调 → ToolOutcome JSON 写宿主缓冲 |
| `service_call`（0.3.0 追加，P9） | `fn(ctx, service, method, args_json, result_buf, result_len) -> i32` | 调用 `get_service` 返回的句柄（身份校验；伪造/悬垂 → `InvalidHandle`）；method + args JSON → 结果 JSON 写宿主缓冲；失败写错误文本并返回非零 |

**能力按 inject 裁剪（0.4.0 落地，P10）**：插件导出 `cos_plugin_manifest`（JSON
`{id, version, api, inject: [...], provide: [...]}`，NUL 结尾字节串）——宿主：
1. 把清单 `inject`/`provide` 并入 loader **依赖图**（静态插件提供者先于消费者；
   宿主服务如 `tools` 无插件提供者 → 不成边，仅作声明）；
2. 按清单 `inject` **裁剪 HostApi 能力**：`get_service` 只对注入的服务生效，
   未注入 → 空指针（插件侧 fail loud）；`service_call` 身份校验不变。
**向后兼容**：无清单符号 = 旧行为（不裁剪、不参与依赖图）。无清单符号的插件
`get_service` 只对注册进 [`BridgeRegistry`]（服务 `"bridges"`）的 JSON 桥生效，
未注册 → 空指针。

**资源生命周期（0.4.0 落地，P11）**：跨边界 `&'static str`（事件名/工具名/描述）由
插件级**去重驻留区**持有、随插件卸载释放（取代 P8 的每次调用 `Box::leak`）；
`free` 诚实回收（见上表）；工具注册自动挂**注销效果**（卸载/回滚反注册，不留僵尸
工具，重载同名工具可再注册）。详见 §10.7。

## 6. 谁分配谁释放（所有权纪律）

| 对象 | 分配 | 释放 | 约束 |
|---|---|---|---|
| HostApi 表 | 宿主（堆，随插件状态） | 宿主（随卸载） | 插件只读；**P9 起与插件实例同生命周期**（插件可自持 ctx/host 指针） |
| config_json / payload_json | 宿主 | 宿主 | 插件只读；调用返回后失效 |
| error_buf / result_buf | 宿主 | 宿主 | 插件写入（NUL 结尾 UTF-8），不扩容 |
| 服务句柄（get_service 返回值） | 宿主 | 宿主（随卸载） | 插件不得解引用/释放；仅可回传 `service_call` |
| Handle（on/register_effect/register_tool） | 宿主 | `free` 或卸载 | 插件不得重复 free（重复/外来句柄无害——幂等无操作，P11） |
| 事件回调 userdata | 插件 | 插件（disposer） | 宿主原样回传 |

## 7. 插件清单（JSON）

B 形态插件随 cdylib 交付清单 JSON（`PluginManifest`，cos_contract 定义）：
`{id, version, api: "major.minor.patch", inject: [...], provide: [...]}`。
宿主按 `inject` 裁剪 HostApi 能力；`api` 缺省 = 按当前版本对待（告警）。

## 8. 错误码（`ErrorCode`，i32）

`Ok=0` / `AbiMismatch=1` / `ConfigInvalid=2` / `ServiceUnavailable=3` / `ApplyFailed=4` /
`EffectRegistrationFailed=5` / `InvalidHandle=6` / `CallFailed=7`（0.3.0 追加，P9）。
详情文本一律走 error_buf。

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

P8 简化（记录在案）：事件名跨边界泄漏（`&'static str` 契约）；`free` 空操作。
（能力裁剪与清单依赖图已在 P10 落地，见 §5 与 §10.6；字符串驻留与 `free`
诚实回收已在 P11 落地，见 §10.7。）

验证（`tests/dlopen_e2e.rs`）：`- name: ./target/debug/plugin_todo_dlopen.dll` 装载 →
`--llm-*` 指向本地回环 chat/completions 服务器（cos-test-support，脚本：tool_use 调
dlopen_todo → 文本）→ dlopen 工具经 C 回调执行 → 结果（"已写入 1 条任务"）回流会话日志
→ 不变量全过 → 卸载逆序 → disposer 写 marker。

## 10.6 P10 清单一等公民（已落地）

状态：**完成**。插件清单（`cos_plugin_manifest`，0.4.0）声明 `inject`/`provide`：

1. **契约**：`cos-contract` 新增导出符号 `cos_plugin_manifest`（返回 NUL 结尾 JSON
   字节串）与 `PluginManifestFn`；`API_VERSION` 0.3.0 → 0.4.0（向后兼容：旧插件 minor 3 ≤ 4）
2. **loader**：`DlopenPlugin::load` 解析清单（符号缺失 = 空清单，旧行为）；`FactoryRef`
   的 inject/provide 改为拥有型——dlopen 清单名并入 **依赖图**（插件提供的服务成边、
   拓扑排序；宿主服务如 `tools` 无插件提供者 → 不成边仅作声明，静态插件缺依赖仍是
   硬错误）；`--dump-config` 计划 JSON 带 `inject`/`provide` 字段
3. **能力裁剪**：`PluginHostState.allowed` = 清单 `inject`；`get_service` 对未注入服务
   返回空指针（插件侧 fail loud）；无清单符号 = 不裁剪（向后兼容）
4. **薄壳**：`plugin-todo-dlopen` 导出清单（`inject: ["tools","todo-store"]`、
   `provide: ["dlopen-todo"]`）；工具改名 `dlopen_todo`（避免与静态 todo 同名冲突）
5. **验证**：e2e 断言依赖边生效（yml 里 dlopen 在 todo 之前，卸载逆序 dlopen 在前——
   apply 序 todo 先于 dlopen）、dump-config 显示清单、裁剪不阻断注入服务访问
   （tools 桥 tools=2）

P10 未做（记录在案）：服务句柄的按名缓存与跨插件复用优化。

## 10.7 P11 资源生命周期（已落地）

状态：**完成**（生态开放前置——用户优先序③"先处理 leak_cstr/host_free 再谈生态开放"）。
处理 P8 记录在案的两项简化：

1. **字符串驻留区（取代 `leak_cstr` 的 `Box::leak`）**：`PluginHostState.strings`
   （`Mutex<Vec<Box<str>>>`）去重驻留跨边界 `&'static str`（事件名/工具名/描述），
   随插件状态 drop（卸载）释放——每次调用不再泄漏一份，卸载/重载循环内存有界。
   **安全不变量（字段顺序钉死）**：`ctx`（首字段，drop 时 `Fiber::Drop` 逆序注销
   监听器与工具注销效果）必须先于 `strings`（末字段）drop——消费者引用全部消失后
   才释放字符串；apply 失败路径同样保留状态到实例 drop（不再提前置 None，
   否则失败前注册的监听器/工具会持有悬垂字符串）。
2. **`free` 诚实回收**：`PluginHostState.handles` 句柄注册表
   （`HashMap<Handle, HandleKind>`，`Listener/Effect/Tool` 各持 [`EffectHandle`] 克隆）——
   `free` 按句柄分发：监听/效果提前 dispose、工具注销；未知/外来/重复/0 句柄 =
   幂等无操作。fiber 卸载路径随后对同一效果无操作（dispose 幂等）。
3. **工具自动注销（无僵尸工具）**：`host_register_tool` 注册成功即把**注销效果**
   挂到插件 fiber——卸载/回滚自动从 `ToolRegistry` 反注册（`ToolRegistry::unregister`
   新增）；重载场景同名工具可再注册（旧实现：泄漏的僵尸工具使同名注册 duplicate
   失败）。tools 服务缺失/同名重复 → 返回 0（插件侧 fail loud）。
4. **空指针守卫**：`emit` 的载荷、`register_tool` 的参数 JSON 空指针 → 安全处理
   （不再有未守卫的 `CStr::from_ptr`）。fn 指针（callback/disposer/execute）在
   Rust 中保证非空（`useless_ptr_null_checks`），不设守卫。
5. **薄壳**：`plugin-todo-dlopen` 的 `Box::leak` `cstr()` 辅助改为 C 字符串字面量
   （`c"..."` / `cr#"..."#`），插件侧同样零泄漏。
6. **验证**：`cos-loader` 单测 7 项（free 提前注销监听/效果、外来句柄幂等、
   free 注销工具、fiber dispose 注销工具 + 重载再注册、字符串去重驻留、
   空指针安全、效果提前 dispose）；e2e 全链路不变（tools=2 等断言照旧）。

P11 未做（记录在案）：句柄按名缓存与跨插件复用优化（同 P10/P9 未做项）。

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

P9 未做（记录在案）：~~能力按清单 `inject` 裁剪~~（P10 已落地，见 §10.6）；
~~`cos_plugin_validate` 入口~~（P12 已兑现，见 §12）；服务句柄的按名缓存与跨插件复用优化。

## 11. 安全边界（明确不做）

验签/哈希校验、沙箱、运行时重载——明确不做（PLAN §0 非目标）。P8/P9 只证明机制，不声称安全。

## 12. B 形态对外暴露面审计（P12）

状态：**完成**（生态开放前置——用户优先序④"审计 B 形态其余对外暴露面"）。
逐面核对"契约声明 vs 实现"后修复三处空转/缺陷，其余记录在案：

| 暴露面 | 契约 | 审计结论 | 处置 |
|---|---|---|---|
| `cos_plugin_validate`（导出符号） | §4 声明（可选） | **死契约**：宿主从不查找/调用——插件实现了也是静默无操作 | ✅ **兑现**：`DlopenPlugin` 解析可选符号，apply 前调用；非零 → `LoadError::DlopenValidate`（fail loud，错误文本透出）；薄壳导出演示 + e2e 断言失败路径 |
| 清单 `api` 字段 | §7 声明 ABI 版本 | **声明不兑现**：`PluginManifest::api_version()` 从不被装载路径使用 | ✅ **强制**：`check_manifest_api`（load 时）；不兼容 → `AbiMismatch`（fail loud）；缺省/非法字符串 = 按当前版本对待 |
| `ErrorCode::from_i32` | 错误码往返映射 | **缺 `CallFailed`(7)**：`service_call` 返回 7 但映射落到 `None`；§8 文档同样漏 7 | ✅ 补 `7 => CallFailed` + 往返测试 + §8 文档 |
| RAII 卸载顺序 | §10（P8 装载集成）"先 fiber 逆序注销，再卸载库" | **潜伏缺陷（P11 起）**：`DlopenPlugin` 字段序 `_library` 先于 `state`——纯 RAII 路径（装载错误返回/未调 finish 直接 drop）库先卸载、`state.ctx` 的 `Fiber::Drop` 随后才逆序执行插件 disposer → **执行已卸载代码 → 访问违例**（0xC0000005 实测复现）；e2e 从未触发（总走 `finish` 的显式 dispose） | ✅ **修复**：`impl Drop for DlopenPlugin` 在字段 drop（库卸载）前兜底 dispose 状态 fiber（幂等，显式路径不受影响）；回归 e2e `dlopen_raii_unload_without_llm_fails_cleanly`（无 LLM → 干净失败 + marker 证明 disposer 已跑） |
| HostApi 字符串参数空指针 | 契约：NUL 结尾、非空 | P11 已全部守卫（emit 载荷/工具参数 JSON） | 无（P11 已修） |
| fn 指针空值 | — | Rust 保证非空（`useless_ptr_null_checks`） | 无（记录） |
| error_buf / result_buf 越界写 | "插件不扩容" | 恶意/损坏插件可越界写宿主缓冲 → 内存破坏 | 非目标（无沙箱，§11；契约已声明） |
| B 提供服务给 A 形态 | 清单 `provide` 仅声明性 | B 插件无法注册 `JsonBridge`/类型化服务——A 消费方 `ctx.get::<T>()` 取不到；B 只能经工具/事件/桥消费宿主服务 | 已知差距（记录）：未来可加宿主函数 `register_bridge`（B 注册 JSON 桥 → A/B 同池可查） |
| 热重载 / 运行时插件管理 | PLAN §0 非目标 | — | 非目标（记录） |

验证：`cos-contract` 4 测试、`cos-loader` 17 测试（+`manifest_api_field_is_enforced`）、
e2e 4 测试（+`dlopen_validate_rejects_bad_config`、+`dlopen_raii_unload_without_llm_fails_cleanly`）；
P11-era 旧 cdylib（无 validate 符号）向后兼容装载验证通过（回归测试覆盖 RAII 路径）。

P12 未做（记录在案）：`register_bridge` 宿主函数（B 提供服务给 A）；服务句柄按名缓存
与跨插件复用优化（同 P9/P10/P11 未做项）。
