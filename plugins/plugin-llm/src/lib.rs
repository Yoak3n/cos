//! plugin-llm —— LLM 提供商统一管理插件（装配 + 后备链）。
//!
//! 配置形态（cordis.yml）：
//! ```yaml
//! - name: opencode-provider        # Provider 插件：代码维护 go/zen 模型目录 + 插件级 api_key
//!   config: { api_key: "${OPENCODE_API_KEY}" }
//! - name: llm
//!   config:
//!     providers:
//!       - { id: go, plugin: opencode-provider }                    # 省略模型 → 插件目录全量展开
//!       - { id: zen, plugin: opencode-provider,
//!           models: [deepseek-v4-flash-free] }                    # 显式裁剪
//!     chains:
//!       - { id: main, providers: [go, zen] }                      # 组 id 自动展开
//! ```
//! provider 条目用 `plugin: <Provider 插件名>` **引用插件条目**（kind 由 `provider_plugin!`
//! 静态映射解析，无需再写）。**模型由插件代码维护**（`get_available_models`：
//! [`LlmRegistry::available_models`] / plugin-opencode 的 `available_models` 纯函数）：
//! - 省略 `model`/`models` → **插件目录全量展开**（每个模型注册 `<id>.<model>`，条目 id
//!   成为组链）；
//! - `model: <id>` → 单模型（注册 id = 条目 id）；
//! - `models: [<id>, ...]` → 显式批量（同上展开）；
//! - 显式选择的模型必须命中目录（fail loud 列出可用模型，可经插件 `config.models` 扩展）。
//!   `kind: <适配器 kind>` 仍可用（向后兼容；模型未命中目录时回落到插件级默认）；
//!   `build` 三级合并：插件级默认 < 模型级目录 < 条目 config；
//!   配置值支持 `${ENV_VAR}` 展开（api_key 等可放环境变量，密钥不进文件）。
//!
//! 装配纪律：宿主先提供空 `LlmRegistry`（服务 `"llm"`），本插件按配置填充；
//! 消费者（记忆插件、agent 创建）按 id / 链 id 解析。**类型优先级保证顺序**：
//! 本插件声明 **Core 类型**（[`PluginTier::Core`]）——loader 注册前扫描全部插件按
//! 类型排序，任何 Provider 插件（`PluginTier::Provider`，含第三方）自动排到本插件
//! 之前（工厂先注册），yml 条目顺序写反也能正确装载。apply 时 `ctx.get` 仍 fail
//! loud（工厂真缺失时）。

#![warn(missing_docs)]

use cos_core::{Context, CoreError, Plugin, PluginTier, Validate};
use cos_llm::{LlmRegistry, expand_env};
use serde::Deserialize;

/// 一条提供商配置（apply 时构建并注册）。
///
/// `kind` 与 `plugin` **二选一**：
/// - `plugin: <Provider 插件名>`（推荐）：引用 yml 里的 Provider 插件条目（如
///   `opencode-provider`）——kind 由 `provider_plugin!` 静态映射解析；`model`/`models`
///   须命中该插件模型目录（fail loud 列出可用模型，可经插件 `config.models` 扩展）；
/// - `kind: <适配器 kind>`：直接指定（向后兼容；模型未命中目录时回落到插件级默认）。
///
/// 模型声明（五选一）：
/// - 省略 + `group: <组>`：按插件目录的**分组**（套餐）展开该组模型；
/// - 省略：插件目录**全量展开**（推荐——模型列表由 Provider 插件代码维护）；
/// - `model: <id>`：单模型，注册 id = 条目 `id`；
/// - `models: [<id>, ...]`：**批量**（聚合 Provider 推荐）——每个模型独立注册为
///   `<id>.<model>`，条目 `id` 自动成为**组链**（顺序后备链），`chains` 可直接引用组 id；
/// - 兼容：`config.model`（旧写法，条目顶层未声明时读取）。
#[derive(Deserialize)]
pub struct ProviderEntry {
    /// 注册 id（消费者按此引用，如 `main` / `go`）。
    pub id: String,
    /// Provider 插件名（`- name: opencode-provider`；与 `kind` 互斥）。
    #[serde(default)]
    pub plugin: Option<String>,
    /// 提供商 kind（Provider 插件注册的名字，如 "opencode"；与 `plugin` 互斥）。
    #[serde(default)]
    pub kind: Option<String>,
    /// 目录分组（套餐）选择：省略模型时只展开该组的模型（如 `group: go`）。
    #[serde(default)]
    pub group: Option<String>,
    /// 单模型（与 `models` 互斥；注册 id = 条目 id）。
    #[serde(default)]
    pub model: Option<String>,
    /// 批量模型（与 `model` 互斥；展开注册 `<id>.<model>`，条目 id 成为组链）。
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// 提供商配置（原样透传给工厂；streaming/max_tokens 等差异字段）。
    #[serde(default)]
    pub config: serde_json::Value,
}

/// 一条后备链配置（主在前，未产出即失败自动切下一个）。
#[derive(Deserialize)]
pub struct ChainEntry {
    /// 链 id（消费者按此引用）。
    pub id: String,
    /// 按优先级排列的提供商 id 列表。
    pub providers: Vec<String>,
}

/// 插件配置。
#[derive(Deserialize, Default)]
pub struct LlmConfig {
    /// 提供商列表。
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    /// 后备链列表。
    #[serde(default)]
    pub chains: Vec<ChainEntry>,
}

impl Validate for LlmConfig {}

/// LLM 统一管理插件（装配 providers + chains 进 [`LlmRegistry`]）。
#[derive(Default)]
pub struct LlmPlugin;

impl Plugin for LlmPlugin {
    fn id(&self) -> &'static str {
        "plugin-llm"
    }

    type Config = LlmConfig;

    /// **Core 类型**（装配优先级次于 Provider）：任何 Provider 插件（含第三方，
    /// 声明 `PluginTier::Provider` 即可）自动排到本插件**之前**（工厂先注册），
    /// yml 条目顺序不再影响装配（写反了也能正确装载）。
    fn tier(&self) -> PluginTier {
        PluginTier::Core
    }

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<LlmRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("llm"))?;
        // 批量条目的组 id → 展开后的注册 id（chains 引用组时展开）
        let mut groups: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for provider in &config.providers {
            // kind 与 plugin 二选一（plugin = 引用 Provider 插件条目，kind 由静态映射解析）
            let (kind, via_plugin) = match (&provider.kind, &provider.plugin) {
                (Some(_), Some(_)) => {
                    return Err(CoreError::Other(format!(
                        "provider '{}' 的 kind 与 plugin 二选一（推荐 plugin: 引用 Provider 插件）",
                        provider.id
                    )));
                }
                (Some(kind), None) => (kind.clone(), false),
                (None, Some(plugin)) => {
                    let kind = registry.kind_of_plugin(plugin).ok_or_else(|| {
                        CoreError::Other(format!(
                            "未知 Provider 插件 '{plugin}'（provider '{}'）；可用插件: {}。\
                             请在 yml 中于本条目之前声明对应插件（如 - name: opencode-provider）。",
                            provider.id,
                            registry.plugin_names().join(", ")
                        ))
                    })?;
                    (kind.to_string(), true)
                }
                (None, None) => {
                    return Err(CoreError::Other(format!(
                        "provider '{}' 需要 plugin 或 kind（推荐 plugin: 引用 Provider 插件）",
                        provider.id
                    )));
                }
            };
            // 模型列表：顶层 model / models / 兼容 config.model
            // 模型列表：顶层 model / models / 兼容 config.model；**全部省略 = 插件目录
            // 全量展开**（模型列表由 Provider 插件代码维护，配置无需逐个添加）。
            // 省略 + group: <组> → 只展开该分组（套餐）的模型。
            let config_model = provider
                .config
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let all_models = registry.available_models(&kind);
            let models: Vec<String> = match (&provider.model, &provider.models, config_model) {
                (Some(_), Some(_), _) => {
                    return Err(CoreError::Other(format!(
                        "provider '{}' 的 model 与 models 二选一（批量用 models）",
                        provider.id
                    )));
                }
                (Some(model), None, _) => vec![model.clone()],
                (None, Some(models), _) if models.is_empty() => {
                    return Err(CoreError::Other(format!(
                        "provider '{}' 的 models 不能为空",
                        provider.id
                    )));
                }
                (None, Some(models), _) => models.clone(),
                (None, None, Some(model)) => vec![model], // 兼容旧写法
                (None, None, None) if provider.group.is_some() => {
                    let group = provider.group.as_deref().unwrap_or_default();
                    let groups = registry.available_groups(&kind);
                    if groups.is_empty() {
                        // 该 kind 没有任何分组：要么目录确实无分组（如 deepseek 官方
                        // 就两个模型），要么工厂未注册（插件未 apply/顺序错）
                        let available = registry.available_models(&kind);
                        return Err(CoreError::Other(if available.is_empty() {
                            format!(
                                "Provider '{kind}'（provider '{}'）没有模型目录（group: '{group}'）——\
                                 工厂可能未注册：请确认 yml 中对应 Provider 插件在 llm 之前声明。",
                                provider.id
                            )
                        } else {
                            format!(
                                "Provider '{kind}'（provider '{}'）未定义任何分组（group: '{group}'）——\
                                 该 Provider 无需分组：省略 group 即全量展开目录，或用 model/models \
                                 显式选择。可用模型: {}。",
                                provider.id,
                                available.join(", ")
                            )
                        }));
                    }
                    let models = registry.models_in_group(&kind, group);
                    if models.is_empty() {
                        return Err(CoreError::Other(format!(
                            "未知分组 '{group}'（provider '{}'）；可用分组: {}。\
                             分组与模型清单由 Provider 插件代码维护（可经插件 config.models 扩展）。",
                            provider.id,
                            groups.join(", ")
                        )));
                    }
                    models
                }
                (None, None, None) if !all_models.is_empty() => all_models.clone(),
                (None, None, None) => {
                    return Err(CoreError::Other(format!(
                        "provider '{}' 需要 model 或 models（或经 plugin: 引用带模型目录的 Provider 插件）",
                        provider.id
                    )));
                }
            };
            // plugin 引用方式：显式选择的模型必须命中插件模型目录（fail loud 列出可用）。
            // 目录为空 = 工厂未注册（插件未 apply）→ 跳过校验，交给 build 报"未知 kind"。
            let available_models = if via_plugin { all_models } else { Vec::new() };
            if !available_models.is_empty() {
                for model in &models {
                    if !available_models.contains(model) {
                        return Err(CoreError::Other(format!(
                            "模型 '{model}' 不在 Provider 插件目录中（provider '{}'）；可用模型: {}。\
                             可经插件 config.models 扩展目录。",
                            provider.id,
                            available_models.join(", ")
                        )));
                    }
                }
            }
            // ${ENV_VAR} 展开（api_key 等可放环境变量，密钥不进配置文件）
            let mut base_config = provider.config.clone();
            expand_env(&mut base_config).map_err(CoreError::Other)?;
            // 批量展开：单模型注册条目 id；批量注册 <id>.<model> 并把条目 id 注册为组链
            let mut expanded_ids = Vec::new();
            for model in &models {
                let mut provider_config = base_config.clone();
                provider_config["model"] = serde_json::Value::String(model.clone());
                let adapter = registry.build(&kind, &provider_config).map_err(|error| {
                    CoreError::Other(format!(
                        "LLM 提供商 kind '{kind}' 不可用（{error}）；可用 kinds: {}。\
                         Provider 为声明式插件——请在 yml 中于本条目之前声明对应插件（如 - name: opencode-provider）。",
                        registry.factory_kinds().join(", ")
                    ))
                })?;
                let register_id = if models.len() == 1 {
                    provider.id.clone()
                } else {
                    format!("{}.{}", provider.id, model)
                };
                registry.register(register_id.clone(), adapter)?;
                expanded_ids.push(register_id);
            }
            if models.len() > 1 {
                groups.insert(provider.id.clone(), expanded_ids);
            }
        }
        for chain in &config.chains {
            // 组引用展开：chains 里的组 id → 该组展开后的注册 id（保持顺序）
            let mut expanded = Vec::new();
            for id in &chain.providers {
                match groups.get(id) {
                    Some(group_ids) => expanded.extend(group_ids.clone()),
                    None => expanded.push(id.clone()),
                }
            }
            registry.register_chain(chain.id.clone(), expanded)?;
        }
        Ok(())
    }
}

cos_loader::plugin!("llm", LlmPlugin);
