//! plugin-custom-provider —— **自定义 Provider 插件**（yml 工厂名 `custom-provider`）。
//!
//! 为"不想写代码"的自定义端点提供纯配置空间：注册 `kind: custom` 工厂（复用
//! OpenAI 兼容 `chat/completions` 适配器，即 [`cos_llm::build_openai`]，随 `cos-llm`
//! 的 `openai` feature 提供），
//! 用户经 plugin-llm 的 provider 条目任意指定 `base_url`/`api_key`/`model` 等：
//! ```yaml
//! - name: custom-provider
//! - name: llm
//!   config:
//!     providers:
//!       - { id: my-api, kind: custom,
//!           config: { base_url: "https://my-gateway/v1", model: "my-model",
//!                     api_key: "${MY_API_KEY}", streaming: false } }
//! ```
//!
//! `config.defaults` 可选：公共字段（如 base_url）下沉为默认值，provider 条目只填差异
//! 字段（条目 config 覆盖默认）。默认值支持 `${ENV_VAR}` 展开（apply 内完成）。
//! `config.api_key` 可选：**插件级 api_key**（支持 `${ENV_VAR}` 展开）——条目可省略
//! `api_key`（条目仍可覆盖）；模型目录条目内也可带 api_key（模型级）。
//! `config.models` 可选：模型目录——每个模型独立声明端点/api 风格/预算等默认字段
//! （`build` 三级合并：插件级 defaults < 模型级 models[model] < 条目 config），
//! 同一网关下不同模型走不同端点/风格时无需重复写条目字段：
//! ```yaml
//! - name: custom-provider
//!   config:
//!     models:
//!       - { model: my-fast,  defaults: { base_url: "https://gw-a/v1", api_style: "openai" } }
//!       - { model: my-vision, defaults: { base_url: "https://gw-b/v1", api_style: "openai",
//!                                          input_content: [text, image] } }
//! ```
//!
//! 与"代码级自定义"的关系：新适配器 = 新 crate + 新封装插件（见 plugin-opencode 范本）
//! 的空间仍然保留；本插件覆盖"纯配置接任意 OpenAI 兼容端点"的场景。
//!
//! 依赖纪律（PLAN.md §2 的例外）：Provider 封装插件的职责就是把 Provider 接进运行时，
//! 因此开启 `cos-llm` 的 **`openai` feature**（OpenAI 兼容适配器）是其本职。
//!
//! 装载顺序：本插件声明 **Provider 类型**（[`PluginTier::Provider`]，装配优先级最高）
//! ——loader 注册前扫描全部插件按类型排序，本插件自动排到 Core/Other 之前
//! （yml 条目写在哪都行，工厂先于 plugin-llm 注册）。

#![warn(missing_docs)]

use cos_core::{Context, CoreError, Plugin, PluginTier, Validate};
use cos_llm::{LlmRegistry, ModelDefaults, expand_env};
use serde::Deserialize;

/// 自定义 Provider 的 kind（plugin-llm 配置里 `kind: custom` 引用）。
pub const CUSTOM_KIND: &str = "custom";

/// 插件配置：可选默认值 + api_key + 模型目录（provider 条目 config 覆盖）。
///
/// yml 无 `config` 时 loader 传 JSON `null`——手写 `Deserialize` 把 `null` 视为默认。
#[derive(Default)]
pub struct CustomProviderConfig {
    /// 公共字段默认值（浅合并到每个 `kind: custom` 条目的 config 之上）。
    pub defaults: Option<serde_json::Value>,
    /// **api_key（插件级默认）**：provider 条目可省略（条目仍可覆盖）；
    /// 支持 `${ENV_VAR}` 展开（如 `${MY_API_KEY}`，缺失 fail loud），密钥不进文件。
    pub api_key: Option<String>,
    /// 模型目录（按 `model` 查默认字段；合并序：插件级 < 模型级 < 条目）。
    pub models: Vec<ModelDefaults>,
}

impl<'de> serde::Deserialize<'de> for CustomProviderConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Inner {
            #[serde(default)]
            defaults: Option<serde_json::Value>,
            #[serde(default)]
            api_key: Option<String>,
            #[serde(default)]
            models: Vec<ModelDefaults>,
        }
        let inner = Option::<Inner>::deserialize(deserializer)?.unwrap_or(Inner {
            defaults: None,
            api_key: None,
            models: Vec::new(),
        });
        Ok(Self {
            defaults: inner.defaults,
            api_key: inner.api_key,
            models: inner.models,
        })
    }
}

impl Validate for CustomProviderConfig {}

/// 自定义 Provider 插件主体。
#[derive(Default)]
pub struct CustomProviderPlugin;

impl Plugin for CustomProviderPlugin {
    fn id(&self) -> &'static str {
        "plugin-custom-provider"
    }

    type Config = CustomProviderConfig;

    /// **Provider 类型**（装配优先级最高）：loader 注册前扫描按类型排序——本插件
    /// 自动排到所有 Core/Other 插件之前（含 yml 里写在 `llm` 之后的场景）。
    fn tier(&self) -> PluginTier {
        PluginTier::Provider
    }

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<LlmRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("llm"))?;
        let mut defaults = config.defaults.clone().unwrap_or(serde_json::Value::Null);
        // 插件级 api_key 并入默认（条目可省略；条目仍可覆盖）
        if let Some(api_key) = &config.api_key {
            if !defaults.is_object() {
                defaults = serde_json::json!({});
            }
            defaults["api_key"] = serde_json::Value::String(api_key.clone());
        }
        // 默认值与模型目录都支持 ${ENV_VAR} 展开（如 api_key 放环境变量）
        expand_env(&mut defaults).map_err(CoreError::Other)?;
        let mut catalog = config.models.clone();
        for entry in &mut catalog {
            expand_env(&mut entry.defaults).map_err(CoreError::Other)?;
        }
        registry
            .register_factory_with_catalog(
                CUSTOM_KIND,
                cos_llm::build_with_style,
                defaults,
                catalog,
            )
            .map_err(|error| {
                CoreError::Other(format!(
                    "custom 工厂注册失败（kind '{CUSTOM_KIND}' 已存在？）: {error}"
                ))
            })
    }
}

cos_loader::plugin!("custom-provider", CustomProviderPlugin);

// yml 插件名 → kind 映射：plugin-llm 条目可用 `plugin: custom-provider` 引用（免写 kind）
cos_llm::provider_plugin!("custom-provider", CUSTOM_KIND);
