//! plugin-llm —— LLM 提供商统一管理插件（装配 + 后备链）。
//!
//! 配置形态（cordis.yml）：
//! ```yaml
//! - name: llm
//!   config:
//!     providers:
//!       - { id: opencode-go, kind: opencode,
//!           config: { base_url: "https://opencode.ai/zen/go/v1", model: "deepseek-v4-flash",
//!                     api_key: "...", streaming: false } }
//!       - { id: zen-free, kind: opencode,
//!           config: { base_url: "https://opencode.ai/zen/v1", model: "deepseek-v4-flash-free",
//!                     api_key: "...", streaming: false } }
//!     chains:
//!       - { id: main, providers: [opencode-go, zen-free] }
//! ```
//! `kind` 由各 Provider crate 经 `cos_llm::llm_factory!` 注册（opencode/mock/…）；
//! 后备链 `providers` 主在前，主在产出任何 chunk 前失败自动切下一个（`FallbackAdapter`）。
//!
//! 装配纪律：宿主先提供空 `LlmRegistry`（服务 `"llm"`），本插件按配置填充；
//! 消费者（记忆插件、agent 创建）按 id / 链 id 解析。**条目顺序重要**：本插件须在
//! 依赖它的插件之前（loader 的 inject 校验只认插件 provide 表、宿主服务不可见，
//! 无依赖边时按配置顺序稳定排序，apply 时 `ctx.get` fail loud）。

#![warn(missing_docs)]

use cos_core::{Context, CoreError, Plugin, Validate};
use cos_llm::LlmRegistry;
use serde::Deserialize;

/// 一条提供商配置（apply 时构建并注册）。
#[derive(Deserialize)]
pub struct ProviderEntry {
    /// 注册 id（消费者按此引用）。
    pub id: String,
    /// 提供商 kind（`llm_factory!` 注册的名字，如 "opencode"/"mock"）。
    pub kind: String,
    /// 提供商配置（原样透传给工厂）。
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
    const ID: &'static str = "plugin-llm";

    type Config = LlmConfig;

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<LlmRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("llm"))?;
        for provider in &config.providers {
            let adapter = registry
                .build(&provider.kind, &provider.config)
                .map_err(|error| CoreError::Other(error.to_string()))?;
            registry.register(provider.id.clone(), adapter)?;
        }
        for chain in &config.chains {
            registry.register_chain(chain.id.clone(), chain.providers.clone())?;
        }
        Ok(())
    }
}

cos_loader::plugin!("llm", LlmPlugin);
