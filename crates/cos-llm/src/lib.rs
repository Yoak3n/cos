//! cos-llm —— LLM 接缝：Message / ContentBlock、LlmAdapter trait、stream、
//! 提供商注册表（P3 / LLM 统一管理）。
//!
//! 接缝纪律（PLAN.md §2 / §6）：这里只有 Definition——具体 Provider（mock / 真实适配器）
//! 与消费方（cos-agent-loop、plugins/*）都只依赖本 crate。
//! 所有公开 trait 对象安全、方法参数窄（可 JSON 序列化的数据，§6 前置防返工）。
//!
//! LLM 统一管理：Provider crate 经 [`llm_factory!`] 注册工厂（inventory 静态收集），
//! [`LlmRegistry`] 服务统一装配/按名取用/后备链（[`FallbackAdapter`] 未产出即失败自动切换）。
//!
//! **`openai` feature**（默认关）：OpenAI 兼容 `chat/completions` 适配器
//! （[`build_openai`] / [`OpenAiAdapter`]，原 cos-llm-openai crate，P9 并入）——随
//! feature 引入 reqwest/tokio，只有 Provider 封装插件开启；默认特性零网络依赖。

#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cos_core::{Context, CoreError, CoreResult, JsonBridge, Service};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(feature = "openai")]
pub use openai::{OpenAiAdapter, OpenAiConfig, build_openai};

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 系统消息。
    System,
    /// 用户消息。
    User,
    /// 助手消息。
    Assistant,
    /// 工具结果消息。
    Tool,
}

/// 用户消息（模型可见输入；`source` 等 dsh 字段 P4 随 agent 补齐）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// 消息文本（text 部分）。
    pub content: String,
    /// 附加图片（URL 或 data URL；空 = 纯文本）。旧 JSONL 缺省为空，向后兼容。
    #[serde(default)]
    pub images: Vec<String>,
    /// 排队身份（RPC prompt/steer/follow_up 的命令 id；用于取消队列中某条消息）。
    /// 缺省 None = 不可按 id 取消（旧 JSONL 兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl UserMessage {
    /// 由文本构造用户消息（无图片、无排队 id）。
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            images: Vec::new(),
            id: None,
        }
    }

    /// 是否携带图片（适配器按此映射 content parts）。
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }
}

/// LLM 可接收的输入内容类型（能力标注，如 text / image）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputContent {
    /// 纯文本。
    Text,
    /// 图片（URL / data URL）。
    Image,
}

/// 模型请求的工具调用（参数为模型产出的原始 JSON 字符串，未解析）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 调用 id（与 `tool/result` 配对）。
    #[serde(rename = "callId")]
    pub call_id: String,
    /// 工具名。
    pub name: String,
    /// 原始 JSON 参数字符串。
    pub arguments: String,
}

/// assistant 消息的内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentBlock {
    /// 文本块。
    Text {
        /// 文本。
        text: String,
    },
    /// 推理块（reasoning_content；与正文分开流式，客户端可选择性展示）。
    Thinking {
        /// 思考文本。
        text: String,
    },
    /// 工具调用块。
    ToolUse {
        /// 调用内容。
        call: ToolCall,
    },
}

/// assistant 消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// 内容块序列。
    pub content: Vec<ContentBlock>,
}

impl AssistantMessage {
    /// 由内容块构造 assistant 消息。
    pub fn new(content: Vec<ContentBlock>) -> Self {
        Self { content }
    }

    /// 拼接全部文本块（推理/工具调用块跳过）。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Thinking { .. } | ContentBlock::ToolUse { .. } => None,
            })
            .collect()
    }
}

/// 工具结果消息（模型可见的工具返回值）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 结果文本。
    pub content: String,
    /// 配对调用 id（OpenAI 协议：`tool` 消息必须带 `tool_call_id`；None = 旧日志兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

impl ToolResultMessage {
    /// 由文本构造工具结果消息（无配对调用 id）。
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            call_id: None,
        }
    }
}

/// 模型可见消息：`derive_messages` 的投影结果与请求消息的统一载体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// 系统消息（请求装配时注入，不出现在推导历史里）。
    System {
        /// 系统提示文本。
        content: String,
    },
    /// 用户消息。
    User(UserMessage),
    /// 助手消息。
    Assistant(AssistantMessage),
    /// 工具结果消息。
    Tool(ToolResultMessage),
    /// 第三方插件的事件投影（决策 D4：Custom 原样透传）。
    Custom {
        /// 事件名。
        name: String,
        /// 事件数据（JSON）。
        data: serde_json::Value,
    },
}

/// 流式增量块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ChunkDelta {
    /// 文本增量。
    Text {
        /// 增量文本。
        text: String,
    },
    /// 推理增量（reasoning_content；与正文分开，客户端可选择性展示）。
    Thinking {
        /// 思考增量。
        text: String,
    },
    /// 工具调用增量。
    ToolUse {
        /// 调用内容。
        call: ToolCall,
    },
}

/// 一个流式 chunk（token 级回放保真的最小单位）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamChunk {
    /// 增量内容。
    pub delta: ChunkDelta,
    /// 末块可携带 token 用量（适配器报告则记，随 assistant/message 一起入库）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

impl StreamChunk {
    /// 由文本增量构造 chunk。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            delta: ChunkDelta::Text { text: text.into() },
            usage: None,
        }
    }
}

/// Token 用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// 输入 token 数。
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    /// 输出 token 数。
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
}

/// 一次模型请求（§6：参数窄、可 JSON 序列化）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    /// 系统提示（无则省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// 模型可见消息。
    pub messages: Vec<Message>,
    /// 工具 schema（P5 定型前为 JSON 占位）。
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
}

/// LLM 适配器边界错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum LlmError {
    /// 适配器失败（message 为人读文本）。
    #[error("{0}")]
    Failure(String),
}

/// 流式响应：chunk 序列（`Err` 终止流）。
pub type LlmStream = Pin<Box<dyn futures::Stream<Item = Result<StreamChunk, LlmError>> + Send>>;

/// LLM 适配器接缝（对象安全：同步方法 + boxed stream 返回）。
pub trait LlmAdapter: Send + Sync {
    /// 适配器 id（同 provider 路由名）。
    fn id(&self) -> &str;

    /// 可接收的输入内容类型（能力标注；缺省仅 text）。
    ///
    /// 适配器可按配置/模型声明（如视觉模型 → `[Text, Image]`）；后备链 = 成员并集。
    fn input_content(&self) -> &[InputContent] {
        &[InputContent::Text]
    }

    /// 以流式方式执行一次请求。
    fn stream(&self, request: &LlmRequest) -> LlmStream;
}

/// 提供商工厂条目（inventory 静态收集，同 loader 的 `PluginRegistrar` 模式）。
///
/// `build` 是自由函数指针（const 可构造）：`kind` + 配置 → 已实例化适配器。
/// 各 Provider crate 经 [`llm_factory!`] 注册（inventory 路径；插件化路径见
/// [`LlmRegistry::register_factory_with_defaults`]）。
pub struct FactoryEntry {
    /// 提供商 kind（配置里引用的名字）。
    pub kind: &'static str,
    /// 由配置构建适配器。
    pub build: fn(&serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError>,
}

inventory::collect!(FactoryEntry);

/// 注册一个 LLM 提供商工厂：`llm_factory!("opencode", build_openai)`。
#[macro_export]
macro_rules! llm_factory {
    ($kind:literal, $build:path) => {
        ::inventory::submit! {
            $crate::FactoryEntry { kind: $kind, build: $build }
        }
    };
}

/// 提供商工厂构建函数（配置 → 适配器）。
pub type LlmFactoryFn = fn(&serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError>;

/// 工厂槽位：构建函数 + 默认配置 + 模型目录（`build` 时三级浅合并）。
struct FactorySlot {
    build: LlmFactoryFn,
    /// 插件级默认配置（非对象视为无默认）；`${ENV_VAR}` 展开由注册方负责。
    defaults: serde_json::Value,
    /// 模型级默认（按 `model` 索引；合并序：插件级 < 模型级 < 条目 config）。
    catalog: std::collections::BTreeMap<String, CatalogEntry>,
}

/// 一条目录条目：分组标签 + 模型级默认字段。
#[derive(Clone)]
struct CatalogEntry {
    /// 分组标签（如 go/zen；None = 不分组）。
    group: Option<String>,
    /// 模型级默认字段。
    defaults: serde_json::Value,
}

/// 一条模型目录条目：某模型的内置默认字段（端点、api 风格、预算等）。
///
/// Provider 插件（如 plugin-opencode 的 go/zen 套餐目录）把"每个模型自己的默认值"
/// 随工厂注册——`build(kind, config)` 时按 `config.model` 查到该条目并作为**模型级
/// 默认**参与合并（条目 config 仍可覆盖）。`defaults` 为自由 JSON，常见字段：
/// `base_url` / `api_style` / `streaming` / `max_tokens` / `input_content`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelDefaults {
    /// 模型 id（`config.model` 的匹配键）。
    pub model: String,
    /// 分组标签（如 go/zen 套餐；`group:` 选择与按组查询用；缺省不分组）。
    #[serde(default)]
    pub group: Option<String>,
    /// 该模型的默认字段（浅合并；条目 config 覆盖）。
    #[serde(default)]
    pub defaults: serde_json::Value,
}

/// 把 `config` 浅合并到 `defaults` 之上：defaults 为底，config 的字段覆盖。
fn merge_defaults(defaults: &serde_json::Value, config: &serde_json::Value) -> serde_json::Value {
    let Some(defaults) = defaults.as_object() else {
        return config.clone();
    };
    let Some(config) = config.as_object() else {
        return serde_json::Value::Object(defaults.clone());
    };
    let mut merged = defaults.clone();
    for (key, value) in config {
        merged.insert(key.clone(), value.clone());
    }
    serde_json::Value::Object(merged)
}

/// 把配置值里的 `${ENV_VAR}` 展开为环境变量（缺失 → `Err`；字符串递归）。
///
/// Provider 插件注册 `defaults` 时若含环境变量引用，apply 内先经本函数展开。
pub fn expand_env(config: &mut serde_json::Value) -> Result<(), String> {
    match config {
        serde_json::Value::String(value) => {
            if let Some(rest) = value
                .strip_prefix("${")
                .and_then(|rest| rest.strip_suffix('}'))
            {
                let resolved = std::env::var(rest)
                    .map_err(|_| format!("环境变量 {rest} 未设置（引用处: {value}）"))?;
                *value = resolved;
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for item in items {
                expand_env(item)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                expand_env(item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Provider 插件条目（inventory 静态收集）：**yml 插件名 → 注册的 kind** 的映射。
///
/// Provider 封装插件（plugin-opencode / plugin-custom-provider）经 [`provider_plugin!`]
/// 声明"我的 yml 工厂名对应哪个 kind"——plugin-llm 的 provider 条目即可用
/// `plugin: <插件名>` 引用（无需再写 `kind`），并经目录校验模型可用性。
pub struct ProviderPluginEntry {
    /// yml 插件名（`- name: opencode-provider` 的 name，与 `plugin!` 注册名一致）。
    pub plugin_name: &'static str,
    /// 该插件注册的 Provider kind。
    pub kind: &'static str,
}

inventory::collect!(ProviderPluginEntry);

/// 声明 Provider 插件名 → kind 映射：`provider_plugin!("opencode-provider", OPENCODE_KIND)`。
#[macro_export]
macro_rules! provider_plugin {
    ($plugin:literal, $kind:path) => {
        ::inventory::submit! {
            $crate::ProviderPluginEntry { plugin_name: $plugin, kind: $kind }
        }
    };
}

/// LLM 提供商注册表服务（`ctx.provide` 为 `"llm"`）：统一装配、按名取用、后备链。
///
/// 与 `ToolRegistry`/`AgentRegistry` 同构：宿主装配空注册表，`plugin-llm` 按配置填充，
/// 消费者（记忆插件、agent 创建）按 id / 链 id 解析。工厂来自 inventory 静态收集
/// （MSVC 下需确保 Provider crate 被链接，cos 锚点天然满足）与插件化注册
/// （[`register_factory`] / [`register_factory_with_defaults`]，Provider 封装插件）。
pub struct LlmRegistry {
    factories: Mutex<BTreeMap<&'static str, FactorySlot>>,
    providers: Mutex<BTreeMap<String, Arc<dyn LlmAdapter>>>,
    chains: Mutex<BTreeMap<String, Vec<String>>>,
    /// 插件名 → kind（`provider_plugin!` 静态收集；plugin-llm 的 `plugin:` 引用用）。
    plugins: Mutex<BTreeMap<&'static str, &'static str>>,
}

impl Service for LlmRegistry {
    const NAME: &'static str = "llm";
}

impl LlmRegistry {
    /// 空注册表（工厂表与插件映射由 inventory 收集填充）。
    pub fn new(_root: &Context) -> Self {
        let mut factories = BTreeMap::new();
        for entry in inventory::iter::<FactoryEntry> {
            factories.insert(
                entry.kind,
                FactorySlot {
                    build: entry.build,
                    defaults: serde_json::Value::Null,
                    catalog: BTreeMap::new(),
                },
            );
        }
        let mut plugins = BTreeMap::new();
        for entry in inventory::iter::<ProviderPluginEntry> {
            plugins.insert(entry.plugin_name, entry.kind);
        }
        Self {
            factories: Mutex::new(factories),
            providers: Mutex::new(BTreeMap::new()),
            chains: Mutex::new(BTreeMap::new()),
            plugins: Mutex::new(plugins),
        }
    }

    /// 程序化注册工厂（inventory 之外的补充；同名拒绝）。
    pub fn register_factory(
        &self,
        kind: &'static str,
        build: fn(&serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError>,
    ) -> CoreResult<()> {
        self.register_factory_with_defaults(kind, build, serde_json::Value::Null)
    }

    /// 程序化注册工厂 + **默认配置**（同名拒绝）。
    ///
    /// `build(kind, config)` 时把 `config` 浅合并到 `defaults` 之上——Provider 封装插件
    /// （如 plugin-opencode 的套餐端点）把公共字段下沉为默认值，provider 条目只需填差异
    /// 字段（model/api_key 等）。`defaults` 中的 `${ENV_VAR}` 需注册方在 apply 内展开。
    pub fn register_factory_with_defaults(
        &self,
        kind: &'static str,
        build: fn(&serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError>,
        defaults: serde_json::Value,
    ) -> CoreResult<()> {
        self.register_factory_with_catalog(kind, build, defaults, Vec::new())
    }

    /// 程序化注册工厂 + 默认配置 + **模型目录**（同名拒绝）。
    ///
    /// 三级合并（默认被覆盖的次序）：插件级 `defaults` < 模型级 `catalog[config.model]`
    /// < 条目 `config`。模型目录让 Provider 插件内置"每个模型自己的默认值"——go/zen
    /// 等套餐的模型清单、各自的端点/api 风格/预算——provider 条目通常只写 `model` +
    /// `api_key`。同名模型条目后者覆盖（目录可扩展）。
    pub fn register_factory_with_catalog(
        &self,
        kind: &'static str,
        build: fn(&serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError>,
        defaults: serde_json::Value,
        catalog: Vec<ModelDefaults>,
    ) -> CoreResult<()> {
        let mut factories = self.factories.lock().unwrap();
        if factories.contains_key(kind) {
            return Err(CoreError::Other(format!("LLM 工厂 '{kind}' 已注册")));
        }
        let catalog = catalog
            .into_iter()
            .map(|entry| {
                (
                    entry.model,
                    CatalogEntry {
                        group: entry.group,
                        defaults: entry.defaults,
                    },
                )
            })
            .collect();
        factories.insert(
            kind,
            FactorySlot {
                build,
                defaults,
                catalog,
            },
        );
        Ok(())
    }

    /// 全部已注册工厂 kind。
    pub fn factory_kinds(&self) -> Vec<&'static str> {
        self.factories.lock().unwrap().keys().copied().collect()
    }

    /// 按插件名查 kind（`provider_plugin!` 静态映射；plugin-llm 的 `plugin:` 引用用）。
    pub fn kind_of_plugin(&self, plugin_name: &str) -> Option<&'static str> {
        self.plugins.lock().unwrap().get(plugin_name).copied()
    }

    /// 全部已声明 Provider 插件名（排序稳定；错误提示用）。
    pub fn plugin_names(&self) -> Vec<&'static str> {
        self.plugins.lock().unwrap().keys().copied().collect()
    }

    /// **可用模型查询**（`get_available_models` 的运行时形态）：某 kind 的模型目录中的
    /// 模型 id 列表（排序稳定）。Provider 插件在代码里维护目录（如 plugin-opencode 的
    /// `BUILTIN_MODELS`），配置面经 `config.models` 追加/覆盖——本接口是"代码维护 +
    /// 公开查询"的入口；plugin-llm 据此做目录校验与"省略模型 = 全量展开"。
    pub fn available_models(&self, kind: &str) -> Vec<String> {
        self.factories
            .lock()
            .unwrap()
            .get(kind)
            .map(|slot| slot.catalog.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// 某 kind 目录中的**分组标签**列表（排序去重；`group:` 选择与错误提示用）。
    pub fn available_groups(&self, kind: &str) -> Vec<String> {
        let mut groups: Vec<String> = self
            .factories
            .lock()
            .unwrap()
            .get(kind)
            .map(|slot| {
                slot.catalog
                    .values()
                    .filter_map(|entry| entry.group.clone())
                    .collect()
            })
            .unwrap_or_default();
        groups.sort();
        groups.dedup();
        groups
    }

    /// 某 kind 目录中**指定分组**的模型 id 列表（排序稳定；省略模型 + `group:` 时展开用）。
    pub fn models_in_group(&self, kind: &str, group: &str) -> Vec<String> {
        let mut models: Vec<String> = self
            .factories
            .lock()
            .unwrap()
            .get(kind)
            .map(|slot| {
                slot.catalog
                    .iter()
                    .filter(|(_, entry)| entry.group.as_deref() == Some(group))
                    .map(|(model, _)| model.clone())
                    .collect()
            })
            .unwrap_or_default();
        models.sort();
        models
    }

    /// 按 kind + 配置构建适配器（工厂查找 + 三级默认合并 + 调用）。
    pub fn build(
        &self,
        kind: &str,
        config: &serde_json::Value,
    ) -> Result<Arc<dyn LlmAdapter>, LlmError> {
        let slot = self
            .factories
            .lock()
            .unwrap()
            .get(kind)
            .map(|slot| (slot.build, slot.defaults.clone(), slot.catalog.clone()))
            .ok_or_else(|| LlmError::Failure(format!("未知 LLM 提供商 kind: {kind}")))?;
        // 三级合并：插件级 < 模型级（按 config.model 查目录）< 条目 config
        let mut merged = slot.1;
        if let Some(model_entry) = config
            .get("model")
            .and_then(serde_json::Value::as_str)
            .and_then(|model| slot.2.get(model))
        {
            merged = merge_defaults(&merged, &model_entry.defaults);
        }
        let merged = merge_defaults(&merged, config);
        (slot.0)(&merged)
    }

    /// 注册已实例化适配器；同 id 拒绝（fail loud）。
    pub fn register(&self, id: impl Into<String>, adapter: Arc<dyn LlmAdapter>) -> CoreResult<()> {
        let id = id.into();
        let mut providers = self.providers.lock().unwrap();
        if providers.contains_key(&id) {
            return Err(CoreError::Other(format!("LLM 提供商 '{id}' 已注册")));
        }
        providers.insert(id, adapter);
        Ok(())
    }

    /// 按 id 取已注册提供商。
    pub fn get(&self, id: &str) -> Option<Arc<dyn LlmAdapter>> {
        self.providers.lock().unwrap().get(id).cloned()
    }

    /// 全部已注册提供商 `(id, adapter.id())`，按 id 序。
    pub fn list(&self) -> Vec<(String, String)> {
        self.providers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, adapter)| (id.clone(), adapter.id().to_string()))
            .collect()
    }

    /// 注册后备链（主在前，未产出即失败自动切下一个）；链内 id 必须已注册（fail loud）。
    pub fn register_chain(&self, id: impl Into<String>, providers: Vec<String>) -> CoreResult<()> {
        let id = id.into();
        {
            let registered = self.providers.lock().unwrap();
            for provider in &providers {
                if !registered.contains_key(provider) {
                    let available: Vec<&str> = registered.keys().map(String::as_str).collect();
                    return Err(CoreError::Other(format!(
                        "后备链 '{id}' 引用了未注册提供商 '{provider}'（已注册: {}）—— \
                         从链的 providers 里删掉它，或在 providers 里补上该条目",
                        if available.is_empty() {
                            "无".to_string()
                        } else {
                            available.join(", ")
                        }
                    )));
                }
            }
        }
        let mut chains = self.chains.lock().unwrap();
        if chains.contains_key(&id) {
            return Err(CoreError::Other(format!("LLM 后备链 '{id}' 已注册")));
        }
        chains.insert(id, providers);
        Ok(())
    }

    /// 按链 id 解析 → [`FallbackAdapter`]。
    pub fn resolve(&self, chain_id: &str) -> Result<Arc<dyn LlmAdapter>, LlmError> {
        let ids = self
            .chains
            .lock()
            .unwrap()
            .get(chain_id)
            .cloned()
            .ok_or_else(|| LlmError::Failure(format!("未知 LLM 后备链: {chain_id}")))?;
        let providers = self.providers.lock().unwrap();
        let adapters: Vec<Arc<dyn LlmAdapter>> = ids
            .iter()
            .filter_map(|id| providers.get(id).cloned())
            .collect();
        if adapters.is_empty() {
            return Err(LlmError::Failure(format!(
                "后备链 '{chain_id}' 无可用的提供商"
            )));
        }
        Ok(Arc::new(FallbackAdapter::new(adapters)))
    }

    /// 按 id 解析：先单提供商，再后备链（消费者统一入口）。
    pub fn resolve_id(&self, id: &str) -> Result<Arc<dyn LlmAdapter>, LlmError> {
        if let Some(adapter) = self.get(id) {
            return Ok(adapter);
        }
        self.resolve(id)
    }

    /// 某 id（提供商或链）的可输入内容能力；链 = 成员并集。
    pub fn capabilities(&self, id: &str) -> Option<Vec<InputContent>> {
        if let Some(adapter) = self.get(id) {
            return Some(adapter.input_content().to_vec());
        }
        let ids = self.chains.lock().unwrap().get(id)?.clone();
        let providers = self.providers.lock().unwrap();
        let mut union: Vec<InputContent> = Vec::new();
        for provider in &ids {
            if let Some(adapter) = providers.get(provider) {
                for content in adapter.input_content() {
                    if !union.contains(content) {
                        union.push(*content);
                    }
                }
            }
        }
        Some(union)
    }

    /// 某 id 是否支持指定输入内容类型。
    pub fn supports(&self, id: &str, content: InputContent) -> bool {
        self.capabilities(id)
            .is_some_and(|caps| caps.contains(&content))
    }

    /// 按能力过滤已注册提供商 id（路由查询；不含链）。
    pub fn by_capability(&self, content: InputContent) -> Vec<String> {
        self.providers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, adapter)| adapter.input_content().contains(&content))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// 后备链相位（unfold 状态机）。
enum FallbackPhase {
    /// 尚未进入某 provider 的流。
    Fresh {
        /// 下一个 provider 下标。
        index: usize,
        /// 当前 provider 已产出的 chunk 数。
        sent: usize,
    },
    /// 正在消费某 provider 的流。
    Active {
        /// provider 下标。
        index: usize,
        /// 已产出的 chunk 数。
        sent: usize,
        /// 内层流。
        inner: LlmStream,
    },
    /// 流已终结（错误已交付或正常结束）。
    Exhausted,
}

/// 后备链适配器：主 provider 在产出任何 chunk 前失败（错误 / 空流）→ 自动切换下一个；
/// 已产出后失败 → 原样传播（不切换，避免内容重复）。
///
/// 纯 futures 组合子（无 spawn、无 tokio 依赖），`LlmAdapter` 对象安全接缝内实现。
/// 能力标注 = 成员并集（[`LlmAdapter::input_content`]）。
#[derive(Clone)]
pub struct FallbackAdapter {
    /// 按优先级排列的适配器（主在前）。
    adapters: Vec<Arc<dyn LlmAdapter>>,
    /// 成员能力并集（构造时计算）。
    input_content: Vec<InputContent>,
}

impl FallbackAdapter {
    /// 由按优先级排列的适配器构造（能力 = 成员并集）。
    pub fn new(adapters: Vec<Arc<dyn LlmAdapter>>) -> Self {
        let mut input_content: Vec<InputContent> = Vec::new();
        for adapter in &adapters {
            for content in adapter.input_content() {
                if !input_content.contains(content) {
                    input_content.push(*content);
                }
            }
        }
        Self {
            adapters,
            input_content,
        }
    }

    /// 链成员（主在前）。
    pub fn adapters(&self) -> &[Arc<dyn LlmAdapter>] {
        &self.adapters
    }
}

impl LlmAdapter for FallbackAdapter {
    fn id(&self) -> &str {
        "fallback"
    }

    fn input_content(&self) -> &[InputContent] {
        &self.input_content
    }

    fn stream(&self, request: &LlmRequest) -> LlmStream {
        let adapters = self.adapters.clone();
        let request = request.clone();
        Box::pin(futures::stream::unfold(
            (
                FallbackPhase::Fresh { index: 0, sent: 0 },
                adapters,
                request,
                None::<LlmError>,
            ),
            |(mut phase, adapters, request, mut last_error)| async move {
                loop {
                    let taken = std::mem::replace(&mut phase, FallbackPhase::Exhausted);
                    match taken {
                        FallbackPhase::Exhausted => return None,
                        FallbackPhase::Fresh { index, sent } => {
                            let Some(adapter) = adapters.get(index) else {
                                // 全部尝试完仍无产出 → 交付最后错误（若有）
                                return last_error.map(|error| {
                                    (
                                        Err(error),
                                        (FallbackPhase::Exhausted, adapters, request, None),
                                    )
                                });
                            };
                            let inner = adapter.stream(&request);
                            phase = FallbackPhase::Active { index, sent, inner };
                        }
                        FallbackPhase::Active { index, sent, inner } => {
                            let mut inner = inner;
                            match inner.next().await {
                                Some(Ok(chunk)) => {
                                    phase = FallbackPhase::Active {
                                        index,
                                        sent: sent + 1,
                                        inner,
                                    };
                                    return Some((
                                        Ok(chunk),
                                        (phase, adapters, request, last_error),
                                    ));
                                }
                                Some(Err(error)) => {
                                    if sent == 0 {
                                        // 未产出即失败：记下错误，切换下一个
                                        last_error = Some(error);
                                        phase = FallbackPhase::Fresh {
                                            index: index + 1,
                                            sent: 0,
                                        };
                                    } else {
                                        // 已产出后失败：原样传播，不切换
                                        return Some((
                                            Err(error),
                                            (FallbackPhase::Exhausted, adapters, request, None),
                                        ));
                                    }
                                }
                                None => {
                                    if sent == 0 {
                                        // 空流同样视为"未产出"，可切换
                                        if last_error.is_none() {
                                            last_error = Some(LlmError::Failure(
                                                "空响应流（未产出任何内容）".to_string(),
                                            ));
                                        }
                                        phase = FallbackPhase::Fresh {
                                            index: index + 1,
                                            sent: 0,
                                        };
                                    } else {
                                        // 正常结束
                                        return None;
                                    }
                                }
                            }
                        }
                    }
                }
            },
        ))
    }
}

/// JSON 桥（P9）：B 形态插件经 `get_service("llm")` + `service_call` 调用。
///
/// 方法集：
/// - `kinds`（无参数）→ 已注册提供商工厂 kind 列表（排序稳定）；
/// - `supports` `{id, content?}` → 指定提供商能否接收该输入内容类型（缺省 `text`）。
impl JsonBridge for LlmRegistry {
    fn call(&self, method: &str, args: serde_json::Value) -> CoreResult<serde_json::Value> {
        match method {
            "kinds" => Ok(serde_json::json!(self.factory_kinds())),
            "supports" => {
                let id = args
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| CoreError::Other("llm.supports 需要 id".into()))?;
                let content: InputContent = args
                    .get("content")
                    .cloned()
                    .map(|value| serde_json::from_value(value).unwrap_or(InputContent::Text))
                    .unwrap_or(InputContent::Text);
                Ok(serde_json::json!(self.supports(id, content)))
            }
            other => Err(CoreError::Other(format!("未知 llm 桥方法: {other}"))),
        }
    }
}
