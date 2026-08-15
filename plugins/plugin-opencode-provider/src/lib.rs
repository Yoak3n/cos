//! plugin-opencode —— OpenAI 兼容 `chat/completions` Provider 的**运行时插件**
//! （yml 工厂名 `opencode-provider`，表明"Provider 插件"的作用）。
//!
//! 背景：API 适配器（对某种 API 的适配）不再是宿主的硬编码依赖，而是与其它插件
//! 一样**声明式装配**——yml 声明 `- name: opencode-provider` 后，本插件把
//! `"opencode"` 工厂注册进 [`LlmRegistry`]（`register_factory_with_catalog` 程序化接口，
//! inventory 之外的声明式路径）；未声明则 `kind: opencode` 不可用（fail loud，
//! 提示声明本插件）。注意区分：**插件名** `opencode-provider`（yml 条目）与
//! **Provider kind** `opencode`（plugin-llm 配置里引用的适配器类型）是两回事。
//!
//! ## 套餐与模型目录
//!
//! 本插件内置**可用模型目录**（[`BUILTIN_MODELS`]），每个模型条目自带默认字段
//! （`base_url` / `api_style` / `streaming` / `max_tokens` 等）——go 与 zen 套餐的
//! 模型清单与端点**不一致**，目录按模型区分；`build` 时三级合并（插件级套餐端点 <
//! 模型级目录 < 条目 config），provider 条目通常只需写 `model` + `api_key`：
//! - `plan: go`  → `https://opencode.ai/zen/go/v1`（OpenCode Go 订阅制网关）；
//! - `plan: zen` → `https://opencode.ai/zen/v1`（Zen 按量网关）；
//! - `config.api_key` 为**插件级 api_key**（支持 `${ENV_VAR}` 展开）——条目可省略
//!   `api_key`（条目仍可覆盖）；模型目录条目内也可带 api_key（模型级）；
//! - `config.models` 可**追加/覆盖**目录条目（同名模型后者生效）——新模型或私有网关
//!   无需改代码；每个模型可独立指定端点与 api 风格（`api_style` 字段已预留，
//!   当前适配器实现 `openai` 风格，其余风格待适配器扩展）。
//!
//! 依赖纪律（PLAN.md §2 的例外）：普通插件不得依赖具体 Provider crate；本插件的职责
//! 就是把 opencode Provider 接进运行时，因此开启 `cos-llm` 的 **`openai` feature**
//! （OpenAI 兼容适配器族，[`cos_llm::build_with_style`]）是其本职。
//!
//! 装载顺序：本插件声明 **Provider 类型**（[`PluginTier::Provider`]，装配优先级最高）
//! ——loader 注册前扫描全部插件按类型排序，本插件自动排到 Core/Other 之前
//! （yml 条目写在哪都行，工厂先于 plugin-llm 注册）。

#![warn(missing_docs)]

use cos_core::{Context, CoreError, Plugin, PluginTier, Validate};
use cos_llm::{LlmRegistry, ModelDefaults, expand_env};
use serde::Deserialize;

/// OpenCode Go 订阅制网关端点（`plan: go`）。
pub const GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
/// Zen 按量网关端点（`plan: zen`）。
pub const ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";

/// opencode kind（宿主 `--llm-*` 解析用；本插件在 apply 时注册进 LlmRegistry）。
/// kind 归插件所有（适配器 crate 是通用 OpenAI 兼容实现，不绑定 kind）。
pub const OPENCODE_KIND: &str = "opencode";

/// 内置模型目录条目（全 const 字符串；apply 时转 [`ModelDefaults`]）。
///
/// 每个模型独立声明**所属分组**（套餐）、端点与 **api style**（`openai` =
/// `/chat/completions`、`anthropic` = `/messages`、`responses` = `/responses`；
/// 经 [`cos_llm::build_with_style`] 分发——opencode-go 内部三种端点并存，见
/// [`BUILTIN_MODELS`]）。`config.models` 可追加/覆盖（同名后者生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinModel {
    /// 模型 id。
    pub model: &'static str,
    /// 所属分组（套餐）：`"go"` 或 `"zen"`——provider 条目 `group:` 按组选择/展开。
    pub group: &'static str,
    /// 该模型的端点（风格决定路径后缀，base_url 同一）。
    pub base_url: &'static str,
    /// api style（openai / anthropic / responses）。
    pub api_style: &'static str,
    /// 建议 streaming（推理网关建议 false）。
    pub streaming: bool,
    /// 建议输出预算。
    pub max_tokens: u32,
}

/// 目录条目构造（const）：同一套餐统一端点/流式/预算，只差模型与风格。
const fn entry(
    model: &'static str,
    group: &'static str,
    base_url: &'static str,
    api_style: &'static str,
) -> BuiltinModel {
    BuiltinModel {
        model,
        group,
        base_url,
        api_style,
        streaming: false,
        max_tokens: 4096,
    }
}

/// 内置模型目录（**go/zen 分组**：go 与 zen 的模型清单/端点不一致，按组维护）。
///
/// go 套餐模型按官方 API 文档标注各自 **api style**：GLM/Kimi/DeepSeek/MiMo/Hy3 →
/// `openai`（`/chat/completions`）、MiniMax/Qwen → `anthropic`（`/messages`）、
/// Grok/GPT → `responses`（`/responses`）——同一 `https://opencode.ai/zen/go/v1`
/// base_url，路径由风格决定。可用模型查询：`available_models()`（全量）/
/// `models_in_group(group)`（按组）；provider 条目省略模型时可按 `group:` 展开。
pub const BUILTIN_MODELS: &[BuiltinModel] = &[
    // ---- go 套餐（订阅制，opencode.ai/zen/go/v1）----
    entry("grok-4.5", "go", GO_BASE_URL, "responses"),
    entry("gpt-5.6-luna", "go", GO_BASE_URL, "responses"),
    entry("glm-5.3", "go", GO_BASE_URL, "openai"),
    entry("glm-5.2", "go", GO_BASE_URL, "openai"),
    entry("glm-5.1", "go", GO_BASE_URL, "openai"),
    entry("kimi-k3", "go", GO_BASE_URL, "openai"),
    entry("kimi-k2.7-code", "go", GO_BASE_URL, "openai"),
    entry("kimi-k2.6", "go", GO_BASE_URL, "openai"),
    entry("deepseek-v4-pro", "go", GO_BASE_URL, "openai"),
    entry("deepseek-v4-flash", "go", GO_BASE_URL, "openai"),
    entry("mimo-v2.5", "go", GO_BASE_URL, "openai"),
    entry("mimo-v2.5-pro", "go", GO_BASE_URL, "openai"),
    entry("minimax-m3", "go", GO_BASE_URL, "anthropic"),
    entry("minimax-m2.7", "go", GO_BASE_URL, "anthropic"),
    entry("minimax-m2.5", "go", GO_BASE_URL, "anthropic"),
    entry("qwen3.8-max", "go", GO_BASE_URL, "anthropic"),
    entry("qwen3.7-max", "go", GO_BASE_URL, "anthropic"),
    entry("qwen3.7-plus", "go", GO_BASE_URL, "anthropic"),
    entry("qwen3.6-plus", "go", GO_BASE_URL, "anthropic"),
    entry("hy3", "go", GO_BASE_URL, "openai"),
    // ---- zen 套餐（按量，opencode.ai/zen/v1）----
    entry("deepseek-v4-flash-free", "zen", ZEN_BASE_URL, "openai"),
];

impl BuiltinModel {
    /// 转为 [`ModelDefaults`]（模型级默认字段，含分组标签）。
    pub fn to_model_defaults(&self) -> ModelDefaults {
        ModelDefaults {
            model: self.model.to_string(),
            group: Some(self.group.to_string()),
            defaults: serde_json::json!({
                "base_url": self.base_url,
                "api_style": self.api_style,
                "streaming": self.streaming,
                "max_tokens": self.max_tokens,
            }),
        }
    }
}

/// 合并目录：内置目录为底，`extra` 追加/覆盖（同名模型后者生效）。
pub fn catalog_with(builtin: &[BuiltinModel], extra: Vec<ModelDefaults>) -> Vec<ModelDefaults> {
    builtin
        .iter()
        .map(BuiltinModel::to_model_defaults)
        .chain(extra)
        .collect()
}

/// **可用模型查询**（`get_available_models`）：内置目录 + 扩展覆盖后的模型 id 列表
/// （排序去重）。模型列表由本插件代码维护（[`BUILTIN_MODELS`]），配置面 `config.models`
/// 可追加/覆盖；llm 配置省略模型时即按此列表全量展开。
pub fn available_models(extra: &[ModelDefaults]) -> Vec<String> {
    let mut models: Vec<String> = catalog_with(BUILTIN_MODELS, extra.to_vec())
        .into_iter()
        .map(|entry| entry.model)
        .collect();
    models.sort();
    models.dedup();
    models
}

/// 指定分组（套餐）的可用模型 id 列表（排序去重；`group: go` / `group: zen` 选择用）。
pub fn models_in_group(extra: &[ModelDefaults], group: &str) -> Vec<String> {
    let mut models: Vec<String> = catalog_with(BUILTIN_MODELS, extra.to_vec())
        .into_iter()
        .filter(|entry| entry.group.as_deref() == Some(group))
        .map(|entry| entry.model)
        .collect();
    models.sort();
    models.dedup();
    models
}

/// 目录中出现的分组标签列表（排序去重；`group:` 校验与错误提示用）。
pub fn available_groups(extra: &[ModelDefaults]) -> Vec<String> {
    let mut groups: Vec<String> = catalog_with(BUILTIN_MODELS, extra.to_vec())
        .into_iter()
        .filter_map(|entry| entry.group)
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

/// 插件配置：套餐选择 + 端点覆盖 + api_key + 模型目录扩展。
///
/// yml 无 `config` 时 loader 传 JSON `null`——手写 `Deserialize` 把 `null` 视为默认。
#[derive(Default)]
pub struct OpencodePluginConfig {
    /// 套餐：`go`（缺省）/ `zen`；决定未命中模型目录时的兜底端点。
    pub plan: Option<String>,
    /// 端点覆盖（任意 OpenAI 兼容端点；优先级低于模型目录条目、高于套餐兜底）。
    pub base_url: Option<String>,
    /// **api_key（插件级默认）**：provider 条目可省略（条目仍可覆盖）；
    /// 支持 `${ENV_VAR}` 展开（如 `${OPENCODE_API_KEY}`，缺失 fail loud），密钥不进文件。
    pub api_key: Option<String>,
    /// 模型目录追加/覆盖（同名模型后者生效；字段见 [`ModelDefaults`]）。
    pub models: Vec<ModelDefaults>,
    /// **模型清单拉取端点**（opt-in）：apply 时 GET 该端点（如
    /// `https://opencode.ai/zen/go/v1/models`）拉取 Provider 可用模型，并入目录
    /// （位置：内置目录 < 拉取 < `config.models` 显式覆盖）。网络失败 → fail loud。
    pub models_endpoint: Option<String>,
    /// 拉取模型的默认 api style（缺省 `"openai"`；可后续经 `config.models` 逐模型覆盖）。
    pub models_api_style: Option<String>,
}

impl<'de> serde::Deserialize<'de> for OpencodePluginConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        struct Inner {
            #[serde(default)]
            plan: Option<String>,
            #[serde(default)]
            base_url: Option<String>,
            #[serde(default)]
            api_key: Option<String>,
            #[serde(default)]
            models: Vec<ModelDefaults>,
            #[serde(default)]
            models_endpoint: Option<String>,
            #[serde(default)]
            models_api_style: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(deserializer)?.unwrap_or_default();
        Ok(Self {
            plan: inner.plan,
            base_url: inner.base_url,
            api_key: inner.api_key,
            models: inner.models,
            models_endpoint: inner.models_endpoint,
            models_api_style: inner.models_api_style,
        })
    }
}

impl Validate for OpencodePluginConfig {
    fn validate(&self) -> Result<(), CoreError> {
        match self.plan.as_deref() {
            None | Some("go") | Some("zen") => Ok(()),
            Some(other) => Err(CoreError::Other(format!(
                "未知 opencode 套餐 '{other}'（可选: go / zen）"
            ))),
        }
    }
}

/// 解析兜底端点：显式 `base_url` 优先，否则按套餐取默认端点（缺省 `go`）。
/// 注意：模型目录命中时其 `base_url` 优先于此兜底（三级合并次序）。
pub fn resolve_base_url(plan: Option<&str>, base_url: Option<&str>) -> String {
    match base_url {
        Some(url) => url.to_string(),
        None => match plan {
            Some("zen") => ZEN_BASE_URL.to_string(),
            _ => GO_BASE_URL.to_string(),
        },
    }
}

/// opencode Provider 插件主体。
#[derive(Default)]
pub struct OpencodePlugin;

impl Plugin for OpencodePlugin {
    fn id(&self) -> &'static str {
        "plugin-opencode-provider"
    }

    type Config = OpencodePluginConfig;

    /// **Provider 类型**（装配优先级最高）：loader 注册前扫描按类型排序——本插件
    /// 自动排到所有 Core/Other 插件之前（含 yml 里写在 `llm` 之后的场景）。
    fn tier(&self) -> PluginTier {
        PluginTier::Provider
    }

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<LlmRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("llm"))?;
        // 插件级默认：套餐兜底端点（模型目录命中时被模型级默认覆盖）+ api_key（插件配置）
        let resolved = resolve_base_url(config.plan.as_deref(), config.base_url.as_deref());
        let mut defaults = serde_json::json!({ "base_url": resolved });
        if let Some(api_key) = &config.api_key {
            defaults["api_key"] = serde_json::Value::String(api_key.clone());
        }
        // defaults 与模型目录都支持 ${ENV_VAR} 展开（api_key 等可放环境变量，密钥不进文件）
        expand_env(&mut defaults).map_err(CoreError::Other)?;
        // 目录次序：内置 < models_endpoint 拉取 < config.models 显式覆盖
        let mut catalog = catalog_with(BUILTIN_MODELS, Vec::new());
        if let Some(endpoint) = &config.models_endpoint {
            let fetched = cos_llm::fetch_models(
                endpoint,
                config.models_api_style.as_deref().unwrap_or("openai"),
            )
            .map_err(CoreError::Other)?;
            catalog.extend(fetched);
        }
        catalog.extend(config.models.clone());
        for entry in &mut catalog {
            expand_env(&mut entry.defaults).map_err(CoreError::Other)?;
        }
        registry
            .register_factory_with_catalog(
                OPENCODE_KIND,
                cos_llm::build_with_style,
                defaults,
                catalog,
            )
            .map_err(|error| {
                CoreError::Other(format!(
                    "opencode 工厂注册失败（kind '{OPENCODE_KIND}' 已存在？）: {error}"
                ))
            })
    }
}

cos_loader::plugin!("opencode-provider", OpencodePlugin);

// yml 插件名 → kind 映射：plugin-llm 条目可用 `plugin: opencode-provider` 引用（免写 kind）
cos_llm::provider_plugin!("opencode-provider", OPENCODE_KIND);
