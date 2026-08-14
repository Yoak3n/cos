//! cos-llm —— LLM 接缝：Message / ContentBlock、LlmAdapter trait、stream、
//! 提供商注册表（P3 / LLM 统一管理）。
//!
//! 接缝纪律（PLAN.md §2 / §6）：这里只有 Definition——具体 Provider（mock / 真实适配器）
//! 与消费方（cos-agent-loop、plugins/*）都只依赖本 crate。
//! 所有公开 trait 对象安全、方法参数窄（可 JSON 序列化的数据，§6 前置防返工）。
//!
//! LLM 统一管理：Provider crate 经 [`llm_factory!`] 注册工厂（inventory 静态收集），
//! [`LlmRegistry`] 服务统一装配/按名取用/后备链（[`FallbackAdapter`] 未产出即失败自动切换）。

#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cos_core::{Context, CoreError, CoreResult, Service};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

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
    /// 消息文本。
    pub content: String,
}

impl UserMessage {
    /// 由文本构造用户消息。
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
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

    /// 拼接全部文本块（工具调用块跳过）。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::ToolUse { .. } => None,
            })
            .collect()
    }
}

/// 工具结果消息（模型可见的工具返回值）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 结果文本。
    pub content: String,
}

impl ToolResultMessage {
    /// 由文本构造工具结果消息。
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
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

    /// 以流式方式执行一次请求。
    fn stream(&self, request: &LlmRequest) -> LlmStream;
}

/// 提供商工厂条目（inventory 静态收集，同 loader 的 `PluginRegistrar` 模式）。
///
/// `build` 是自由函数指针（const 可构造）：`kind` + 配置 → 已实例化适配器。
/// 各 Provider crate（cos-llm-opencode / cos-llm-mock …）经 [`llm_factory!`] 注册。
pub struct FactoryEntry {
    /// 提供商 kind（配置里引用的名字）。
    pub kind: &'static str,
    /// 由配置构建适配器。
    pub build: fn(&serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError>,
}

inventory::collect!(FactoryEntry);

/// 注册一个 LLM 提供商工厂：`llm_factory!("opencode", build_opencode)`。
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

/// LLM 提供商注册表服务（`ctx.provide` 为 `"llm"`）：统一装配、按名取用、后备链。
///
/// 与 `ToolRegistry`/`AgentRegistry` 同构：宿主装配空注册表，`plugin-llm` 按配置填充，
/// 消费者（记忆插件、agent 创建）按 id / 链 id 解析。工厂来自 inventory 静态收集
/// （MSVC 下需确保 Provider crate 被链接，cos 锚点天然满足）。
pub struct LlmRegistry {
    factories: Mutex<BTreeMap<&'static str, LlmFactoryFn>>,
    providers: Mutex<BTreeMap<String, Arc<dyn LlmAdapter>>>,
    chains: Mutex<BTreeMap<String, Vec<String>>>,
}

impl Service for LlmRegistry {
    const NAME: &'static str = "llm";
}

impl LlmRegistry {
    /// 空注册表（工厂表由 inventory 收集填充）。
    pub fn new(_root: &Context) -> Self {
        let mut factories = BTreeMap::new();
        for entry in inventory::iter::<FactoryEntry> {
            factories.insert(entry.kind, entry.build);
        }
        Self {
            factories: Mutex::new(factories),
            providers: Mutex::new(BTreeMap::new()),
            chains: Mutex::new(BTreeMap::new()),
        }
    }

    /// 程序化注册工厂（inventory 之外的补充；同名拒绝）。
    pub fn register_factory(
        &self,
        kind: &'static str,
        build: fn(&serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError>,
    ) -> CoreResult<()> {
        let mut factories = self.factories.lock().unwrap();
        if factories.contains_key(kind) {
            return Err(CoreError::Other(format!("LLM 工厂 '{kind}' 已注册")));
        }
        factories.insert(kind, build);
        Ok(())
    }

    /// 全部已注册工厂 kind。
    pub fn factory_kinds(&self) -> Vec<&'static str> {
        self.factories.lock().unwrap().keys().copied().collect()
    }

    /// 按 kind + 配置构建适配器（工厂查找 + 调用）。
    pub fn build(
        &self,
        kind: &str,
        config: &serde_json::Value,
    ) -> Result<Arc<dyn LlmAdapter>, LlmError> {
        let build = self
            .factories
            .lock()
            .unwrap()
            .get(kind)
            .copied()
            .ok_or_else(|| LlmError::Failure(format!("未知 LLM 提供商 kind: {kind}")))?;
        build(config)
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
                    return Err(CoreError::Other(format!(
                        "后备链 '{id}' 引用了未注册提供商 '{provider}'"
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
        Ok(Arc::new(FallbackAdapter { adapters }))
    }

    /// 按 id 解析：先单提供商，再后备链（消费者统一入口）。
    pub fn resolve_id(&self, id: &str) -> Result<Arc<dyn LlmAdapter>, LlmError> {
        if let Some(adapter) = self.get(id) {
            return Ok(adapter);
        }
        self.resolve(id)
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
#[derive(Clone)]
pub struct FallbackAdapter {
    /// 按优先级排列的适配器（主在前）。
    pub adapters: Vec<Arc<dyn LlmAdapter>>,
}

impl LlmAdapter for FallbackAdapter {
    fn id(&self) -> &str {
        "fallback"
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
