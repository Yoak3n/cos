//! plugin-deepseek —— DeepSeek **官方 API** Provider 的运行时插件（yml 工厂名
//! `deepseek-provider`，表明"Provider 插件"的作用）。
//!
//! 背景：DeepSeek 官方 API（[`DEEPSEEK_BASE_URL`]）是 OpenAI 兼容的 `chat/completions`
//! 接口——本插件开启 `cos-llm` 的 `openai` feature 复用其 OpenAI 兼容适配器
//! （[`cos_llm::build_openai`]：流式优先、未产出即失败自动非流式兜底、
//! `reasoning_content` 推理内容独立成 Thinking 块），按 plugin-opencode 的范本把
//! `"deepseek"` 工厂注册进 [`LlmRegistry`]（`register_factory_with_catalog` 声明式路径）。
//! 未声明本插件则 `kind: deepseek` 不可用（fail loud，提示声明本插件）。注意区分：
//! **插件名** `deepseek-provider`（yml 条目）与 **Provider kind** `deepseek`
//! （plugin-llm 配置里引用的适配器类型）是两回事。
//!
//! ## 内置模型目录
//!
//! 官方模型清单（[`BUILTIN_MODELS`]），**无分组**（官方就两个模型，无需套餐/家族分组）：
//! - `deepseek-v4-flash`（通用对话）；
//! - `deepseek-v4-pro`（旗舰；思考吃预算，`max_tokens` 建议给足）；
//!
//! 每个模型独立声明端点/流式/预算（官方 API 全模型同一端点）；`config.models` 可
//! **追加/覆盖**目录条目（同名后者生效）。`config.api_key` 为**插件级 api_key**
//! （支持 `${ENV_VAR}` 展开）——provider 条目可省略 `api_key`（条目仍可覆盖）。
//!
//! 依赖纪律（PLAN.md §2 的例外）：Provider 封装插件的职责就是把 Provider 接进运行时，
//! 因此开启 `cos-llm` 的 **`openai` feature**（OpenAI 兼容适配器）是其本职。
//!
//! 装载顺序：本插件声明 **Provider 类型**（[`PluginTier::Provider`]，装配优先级最高）
//! ——loader 注册前扫描全部插件按类型排序，本插件自动排到 Core/Other 之前
//! （yml 条目写在哪都行，工厂先于 plugin-llm 注册）。
//!
//! 说明：适配器 id 沿复用实现为 `"openai"`（kind 才是 `"deepseek"`；custom-provider
//! 同理）——仅影响 `list()` 等展示，路由/装配按 kind 与注册 id 进行。
#![warn(missing_docs)]

use cos_core::{Context, CoreError, Plugin, PluginTier, Validate};
use cos_llm::{LlmRegistry, ModelDefaults, expand_env};
use serde::Deserialize;

/// DeepSeek 官方 API 端点（适配器自动拼 `/chat/completions`；`/v1` 前缀官方同样接受）。
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";

/// deepseek kind（plugin-llm 配置里 `kind: deepseek` 引用；与插件名 `deepseek-provider` 区分）。
pub const DEEPSEEK_KIND: &str = "deepseek";

/// 内置模型目录条目（全 const 字符串；apply 时转 [`ModelDefaults`]）。
///
/// 每个模型独立声明**所属分组**（模型家族）与流式/预算建议。`config.models` 可追加/覆盖
/// （同名后者生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinModel {
    /// 模型 id。
    pub model: &'static str,
    /// 该模型的端点（官方 API 全模型同一端点）。
    pub base_url: &'static str,
    /// api 风格（OpenAI 兼容；当前适配器实现 "openai"）。
    pub api_style: &'static str,
    /// 建议 streaming（官方 API SSE 稳定；个别网络环境可经条目 config 关闭）。
    pub streaming: bool,
    /// 建议输出预算（pro 思考会吃掉预算，给足以免正文被裁空）。
    pub max_tokens: u32,
}

/// 内置模型目录（官方 API 模型清单；两模型共用官方端点，**无分组**——官方就
/// `deepseek-v4-flash` / `deepseek-v4-pro` 两个模型，无需套餐/家族分组）。
///
/// 可用模型查询：`available_models()`（全量）；provider 条目省略模型即按此全量展开，
/// 或用 `model:`/`models:` 显式选择。`group:` 在本 Provider 上不可用（报错会列出模型）。
pub const BUILTIN_MODELS: &[BuiltinModel] = &[
    BuiltinModel {
        model: "deepseek-v4-flash",
        base_url: DEEPSEEK_BASE_URL,
        api_style: "openai",
        streaming: true,
        max_tokens: 1_000_000,
    },
    BuiltinModel {
        model: "deepseek-v4-pro",
        base_url: DEEPSEEK_BASE_URL,
        api_style: "openai",
        streaming: true,
        max_tokens: 1_000_000,
    },
];

impl BuiltinModel {
    /// 转为 [`ModelDefaults`]（模型级默认字段；官方目录不分组）。
    pub fn to_model_defaults(&self) -> ModelDefaults {
        ModelDefaults {
            model: self.model.to_string(),
            group: None,
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

/// 插件配置：端点覆盖 + api_key + 模型目录扩展。
///
/// yml 无 `config` 时 loader 传 JSON `null`——手写 `Deserialize` 把 `null` 视为默认。
#[derive(Default)]
pub struct DeepseekPluginConfig {
    /// 端点覆盖（任意 OpenAI 兼容端点；官方 API 缺省 [`DEEPSEEK_BASE_URL`]）。
    pub base_url: Option<String>,
    /// **api_key（插件级默认）**：provider 条目可省略（条目仍可覆盖）；
    /// 支持 `${ENV_VAR}` 展开（如 `${DEEPSEEK_API_KEY}`，缺失 fail loud），密钥不进文件。
    pub api_key: Option<String>,
    /// 模型目录追加/覆盖（同名模型后者生效；字段见 [`ModelDefaults`]）。
    pub models: Vec<ModelDefaults>,
}

impl<'de> serde::Deserialize<'de> for DeepseekPluginConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        struct Inner {
            #[serde(default)]
            base_url: Option<String>,
            #[serde(default)]
            api_key: Option<String>,
            #[serde(default)]
            models: Vec<ModelDefaults>,
        }
        let inner = Option::<Inner>::deserialize(deserializer)?.unwrap_or_default();
        Ok(Self {
            base_url: inner.base_url,
            api_key: inner.api_key,
            models: inner.models,
        })
    }
}

impl Validate for DeepseekPluginConfig {}

/// 解析端点：显式 `base_url` 优先，否则官方 API 端点。
/// 注意：模型目录命中时其 `base_url` 优先于此兜底（三级合并次序）。
pub fn resolve_base_url(base_url: Option<&str>) -> String {
    match base_url {
        Some(url) => url.to_string(),
        None => DEEPSEEK_BASE_URL.to_string(),
    }
}

/// DeepSeek 官方 API Provider 插件主体。
#[derive(Default)]
pub struct DeepseekPlugin;

impl Plugin for DeepseekPlugin {
    fn id(&self) -> &'static str {
        "plugin-deepseek"
    }

    type Config = DeepseekPluginConfig;

    /// **Provider 类型**（装配优先级最高）：loader 注册前扫描按类型排序——本插件
    /// 自动排到所有 Core/Other 插件之前（含 yml 里写在 `llm` 之后的场景）。
    fn tier(&self) -> PluginTier {
        PluginTier::Provider
    }

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<LlmRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("llm"))?;
        // 插件级默认：官方端点（条目 config 可覆盖）+ api_key（插件配置）
        let mut defaults =
            serde_json::json!({ "base_url": resolve_base_url(config.base_url.as_deref()) });
        if let Some(api_key) = &config.api_key {
            defaults["api_key"] = serde_json::Value::String(api_key.clone());
        }
        // defaults 与模型目录都支持 ${ENV_VAR} 展开（api_key 等可放环境变量，密钥不进文件）
        expand_env(&mut defaults).map_err(CoreError::Other)?;
        let mut catalog = catalog_with(BUILTIN_MODELS, config.models.clone());
        for entry in &mut catalog {
            expand_env(&mut entry.defaults).map_err(CoreError::Other)?;
        }
        registry
            .register_factory_with_catalog(DEEPSEEK_KIND, cos_llm::build_openai, defaults, catalog)
            .map_err(|error| {
                CoreError::Other(format!(
                    "deepseek 工厂注册失败（kind '{DEEPSEEK_KIND}' 已存在？）: {error}"
                ))
            })
    }
}

cos_loader::plugin!("deepseek-provider", DeepseekPlugin);

// yml 插件名 → kind 映射：plugin-llm 条目可用 `plugin: deepseek-provider` 引用（免写 kind）
cos_llm::provider_plugin!("deepseek-provider", DEEPSEEK_KIND);
