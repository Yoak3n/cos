//! 确定性脚本化 mock LLM 适配器（迁移自 cos-llm-mock；测试与回放专用）。

use std::sync::atomic::{AtomicUsize, Ordering};

use cos_llm::{LlmAdapter, LlmError, LlmRequest, LlmStream, StreamChunk};

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

#[cfg(test)]
mod tests {
    use super::*;
    use cos_llm::ChunkDelta;
    use futures::StreamExt;

    /// 脚本按调用序号推进；耗尽后流出错。
    #[tokio::test]
    async fn script_replies_in_order_then_errors() {
        let adapter = MockAdapter::new(
            "t",
            vec![
                MockReply::new(vec![StreamChunk::text("你"), StreamChunk::text("好")]),
                MockReply::text("回"),
            ],
        );
        let request = LlmRequest::default();
        let mut stream = adapter.stream(&request);
        let mut first = String::new();
        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            if let ChunkDelta::Text { text } = chunk.delta {
                first.push_str(&text);
            }
        }
        assert_eq!(first, "你好");

        let mut stream = adapter.stream(&request);
        let mut second = String::new();
        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            if let ChunkDelta::Text { text } = chunk.delta {
                second.push_str(&text);
            }
        }
        assert_eq!(second, "回");

        // 脚本耗尽 → 流 Err
        let mut stream = adapter.stream(&request);
        let item = stream.next().await.unwrap();
        assert!(item.is_err());
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn exhausted_and_reset() {
        let adapter = MockAdapter::new("t", vec![MockReply::text("x")]);
        assert!(!adapter.exhausted());
        let _ = adapter.stream(&LlmRequest::default());
        assert!(adapter.exhausted());
        adapter.reset();
        assert!(!adapter.exhausted());
    }
}
