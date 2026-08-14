//! dsh-llm-mock —— 确定性脚本化 mock 适配器（测试与回放，P3）。
//!
//! 语义：脚本 = 预设回复列表；每次 `stream()` 按**调用序号**取下一个预设回复
//! 并流式产出其 chunks（同一脚本 → 同一输出，跨实例确定）。脚本耗尽后流产出 `Err`。
//! （计划 §5 的"按输入哈希选择"为备选方案，A 形态用序号方案即可。）
//!
//! LLM 统一管理：本 crate 经 `llm_factory!("mock", build_mock)` 注册提供商工厂
//! （空脚本 → 任何调用即失败，适合后备链测试与占位）。

#![warn(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dsh_llm::{LlmAdapter, LlmError, LlmRequest, LlmStream, StreamChunk};

/// 提供商工厂构建函数（`llm_factory!` 注册）：配置忽略 → 空脚本 mock（调用即失败）。
pub fn build_mock(_config: &serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError> {
    Ok(Arc::new(MockAdapter::new("mock", vec![])))
}

dsh_llm::llm_factory!("mock", build_mock);

/// 一次预设回复：按顺序流出的 chunk 序列。
#[derive(Debug, Clone)]
pub struct MockReply {
    /// 依次产出的 chunks。
    pub chunks: Vec<StreamChunk>,
}

impl MockReply {
    /// 由 chunks 构造预设回复。
    pub fn new(chunks: Vec<StreamChunk>) -> Self {
        Self { chunks }
    }

    /// 由文本构造单块预设回复（文本自动按字符切分为多块，模拟流式）。
    pub fn text(text: &str) -> Self {
        Self {
            chunks: text
                .chars()
                .map(|c| StreamChunk::text(c.to_string()))
                .collect(),
        }
    }
}

/// 确定性脚本化 mock 适配器。
pub struct MockAdapter {
    id: String,
    script: Vec<MockReply>,
    cursor: AtomicUsize,
}

impl MockAdapter {
    /// 由脚本构造适配器。
    pub fn new(id: impl Into<String>, script: Vec<MockReply>) -> Self {
        Self {
            id: id.into(),
            script,
            cursor: AtomicUsize::new(0),
        }
    }

    /// 脚本是否已耗尽（已取完所有预设回复）。
    pub fn exhausted(&self) -> bool {
        self.cursor.load(Ordering::SeqCst) >= self.script.len()
    }

    /// 重置调用序号（回放场景）。
    pub fn reset(&self) {
        self.cursor.store(0, Ordering::SeqCst);
    }
}

impl LlmAdapter for MockAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn stream(&self, _request: &LlmRequest) -> LlmStream {
        let index = self.cursor.fetch_add(1, Ordering::SeqCst);
        match self.script.get(index) {
            Some(reply) => {
                let chunks: Vec<Result<StreamChunk, LlmError>> =
                    reply.chunks.clone().into_iter().map(Ok).collect();
                Box::pin(futures::stream::iter(chunks))
            }
            None => {
                let message = format!(
                    "mock 脚本耗尽（第 {index} 次调用，脚本共 {} 条）",
                    self.script.len()
                );
                Box::pin(futures::stream::once(async move {
                    Err(LlmError::Failure(message))
                }))
            }
        }
    }
}
