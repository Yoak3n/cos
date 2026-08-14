//! M2 验收：cos-llm-opencode —— 本地回环 HTTP 服务器打桩：
//! 流式文本增量 + usage 映射 + [DONE] 收束；服务端失败（5xx / error 块）在无产出时
//! 自动非流式兜底；4xx 不重试原样报错。

use cos_llm::{
    AssistantMessage, ChunkDelta, ContentBlock, InputContent, LlmAdapter, LlmRequest, Message,
    TokenUsage, ToolCall, ToolResultMessage, UserMessage,
};
use cos_llm_opencode::{OpencodeAdapter, OpencodeConfig};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn adapter(port: u16) -> OpencodeAdapter {
    OpencodeAdapter::new(OpencodeConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-key".into(),
        model: "deepseek-v4-flash-free".into(),
        streaming: true,
        max_tokens: Some(2048),
        input_content: vec![InputContent::Text],
    })
}

/// 非流式适配器（单次请求；zen/go 网关默认形态）。
fn single_adapter(port: u16) -> OpencodeAdapter {
    OpencodeAdapter::new(OpencodeConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-key".into(),
        model: "deepseek-v4-flash-free".into(),
        streaming: false,
        max_tokens: Some(2048),
        input_content: vec![InputContent::Text],
    })
}

fn request() -> LlmRequest {
    LlmRequest {
        system: None,
        messages: vec![Message::User(UserMessage::new("你好"))],
        tools: vec![],
    }
}

/// 处理 N 个连接：每次读请求头断言 → 写回对应响应。
async fn serve(listener: TcpListener, responses: Vec<&'static str>) {
    for response in responses {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(request.contains("POST /v1/chat/completions"), "{request}");
        assert!(request.contains("deepseek-v4-flash-free"), "{request}");
        // reqwest 头名小写化，比较时统一小写
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer test-key"),
            "{request}"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn streams_text_and_maps_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"，世界\"}}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n\
             data: [DONE]\n\n",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    let mut usage = None;
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let ChunkDelta::Text { text: delta } = chunk.delta {
            text.push_str(&delta);
        }
        if chunk.usage.is_some() {
            usage = chunk.usage;
        }
    }
    server.await.unwrap();
    assert_eq!(text, "你好，世界");
    assert_eq!(
        usage.unwrap(),
        TokenUsage {
            input_tokens: 10,
            output_tokens: 2
        }
    );
}

#[tokio::test]
async fn reasoning_only_stream_falls_back_to_thought_text() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"思考\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"中\"}}]}\n\n\
             data: [DONE]\n\n",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let ChunkDelta::Text { text: delta } = chunk.delta {
            text.push_str(&delta);
        }
    }
    server.await.unwrap();
    assert_eq!(
        text, "思考中",
        "全程无 content 时回退推理文本（宁可说出思考，不可空回复）"
    );
}

#[tokio::test]
async fn content_drops_buffered_reasoning() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"先想想\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"回答\"}}]}\n\n\
             data: [DONE]\n\n",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let ChunkDelta::Text { text: delta } = chunk.delta {
            text.push_str(&delta);
        }
    }
    server.await.unwrap();
    assert_eq!(text, "回答", "content 一出现即丢弃推理缓冲（回答优先）");
}

#[tokio::test]
async fn bare_json_error_without_data_prefix_is_detected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            // 200 + 裸 JSON error（无 data: 前缀）→ 触发非流式兜底
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"type\":\"error\",\"error\":{\"type\":\"error\",\"message\":\"Internal server error\"}}",
            // 兜底：非流式整段内容
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"c3\",\"object\":\"chat.completion\",\"model\":\"deepseek-v4-flash-free\",\
             \"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"裸错误兜底成功\"}}],\
             \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let ChunkDelta::Text { text: delta } = chunk.delta {
            text.push_str(&delta);
        }
    }
    server.await.unwrap();
    assert_eq!(text, "裸错误兜底成功");
}

#[tokio::test]
async fn http_401_is_fatal_and_not_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec!["HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n{}"],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let item = stream.next().await.unwrap();
    let error = item.unwrap_err();
    assert!(error.to_string().contains("401"), "{error}");
    assert!(stream.next().await.is_none(), "错误后流应收束");
    server.await.unwrap();
}

#[tokio::test]
async fn streaming_5xx_falls_back_to_single_shot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            // 第一次（流式）：服务端 500
            "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\n\r\n{}",
            // 第二次（非流式兜底）：整段内容
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"c1\",\"object\":\"chat.completion\",\"model\":\"deepseek-v4-flash-free\",\
             \"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"你好，世界\"}}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    let mut usage = None;
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let ChunkDelta::Text { text: delta } = chunk.delta {
            text.push_str(&delta);
        }
        if chunk.usage.is_some() {
            usage = chunk.usage;
        }
    }
    server.await.unwrap();
    assert_eq!(text, "你好，世界", "流式失败应自动退化非流式");
    assert_eq!(
        usage.unwrap(),
        TokenUsage {
            input_tokens: 10,
            output_tokens: 4
        }
    );
}

#[tokio::test]
async fn streaming_error_block_falls_back_to_single_shot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            // 第一次（流式）：error 块
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"type\":\"error\",\"error\":{\"type\":\"error\",\"message\":\"Internal server error\"}}\n\n",
            // 第二次（非流式兜底）：整段内容
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"c2\",\"object\":\"chat.completion\",\"model\":\"deepseek-v4-flash-free\",\
             \"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"兜底成功\"}}],\
             \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let ChunkDelta::Text { text: delta } = chunk.delta {
            text.push_str(&delta);
        }
    }
    server.await.unwrap();
    assert_eq!(text, "兜底成功", "error 块应触发非流式兜底");
}

#[tokio::test]
async fn fallback_failure_yields_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\n\r\n{}",
            "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\n\r\n{}",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let item = stream.next().await.unwrap();
    let error = item.unwrap_err();
    assert!(error.to_string().contains("兜底"), "{error}");
    assert!(stream.next().await.is_none());
    server.await.unwrap();
}

/// 带图片的用户消息 → OpenAI 多部分 content（text + image_url parts）。
#[tokio::test]
async fn image_message_maps_to_image_url_parts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 16384];
        let n = socket.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(request.contains("\"type\":\"text\""), "{request}");
        assert!(
            request.contains("\"type\":\"image_url\""),
            "应映射为 image_url part: {request}"
        );
        assert!(
            request.contains("\"image_url\":{\"url\":\"https://example.com/cat.png\"}"),
            "图片 URL 应进 image_url.url: {request}"
        );
        socket
            .write_all(
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
                 {\"id\":\"c\",\"object\":\"chat.completion\",\"model\":\"m\",\
                 \"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}],\
                 \"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}"
                    .as_bytes(),
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
    });

    let mut user = UserMessage::new("这只猫叫什么？");
    user.images.push("https://example.com/cat.png".to_string());
    let request = LlmRequest {
        system: None,
        messages: vec![Message::User(user)],
        tools: vec![],
    };
    let adapter = adapter(port);
    let mut stream = adapter.stream(&request);
    while let Some(item) = stream.next().await {
        assert!(item.is_ok(), "{item:?}");
    }
    server.await.unwrap();

    // 能力标注：纯文本适配器缺省只声明 text
    assert!(adapter.input_content().contains(&InputContent::Text));
    assert!(!adapter.input_content().contains(&InputContent::Image));
}

/// 非流式响应带工具调用（content 空 + tool_calls —— zen/go 推理模型实测形态）
/// → 解析为 ToolUse 块（而不是把思考/空串当回复）。
#[tokio::test]
async fn non_streaming_tool_call_maps_to_tool_use() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"c1\",\"object\":\"chat.completion\",\"model\":\"deepseek-v4-flash-free\",\
             \"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":null,\
             \"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\
             \"function\":{\"name\":\"recall\",\"arguments\":\"{\\\"query\\\":\\\"咖啡\\\"}\"}}]}}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}",
        ],
    ));

    let adapter = single_adapter(port);
    let mut stream = adapter.stream(&request());
    let mut calls = Vec::new();
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item.unwrap().delta {
            ChunkDelta::Text { text: delta } => text.push_str(&delta),
            ChunkDelta::ToolUse { call } => calls.push(call),
        }
    }
    server.await.unwrap();
    assert!(text.is_empty(), "content 空 → 不应有文本（也不应回退思考）");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "call_1");
    assert_eq!(calls[0].name, "recall");
    assert_eq!(calls[0].arguments, r#"{"query":"咖啡"}"#);
}

/// 流式工具调用分片（index 归组 + arguments 拼接）→ 流尾合成 ToolUse。
#[tokio::test]
async fn streaming_tool_call_fragments_are_assembled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\
             \"type\":\"function\",\"function\":{\"name\":\"inventory\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"{\\\"limit\\\":\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"3}\"}}]}}]}\n\n\
             data: [DONE]\n\n",
        ],
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut calls = Vec::new();
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item.unwrap().delta {
            ChunkDelta::Text { text: delta } => text.push_str(&delta),
            ChunkDelta::ToolUse { call } => calls.push(call),
        }
    }
    server.await.unwrap();
    assert!(text.is_empty());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "call_9");
    assert_eq!(calls[0].name, "inventory");
    assert_eq!(calls[0].arguments, r#"{"limit":3}"#);
}

/// assistant 历史带工具调用 → 请求体必须回传 tool_calls（OpenAI 协议，工具结果轮必需）。
#[tokio::test]
async fn assistant_tool_history_is_sent_back_in_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 16384];
        let n = socket.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(request.contains("\"role\":\"assistant\""), "{request}");
        assert!(
            request.contains("\"tool_calls\""),
            "assistant 历史应带 tool_calls: {request}"
        );
        assert!(request.contains("\"id\":\"call_1\""), "{request}");
        assert!(request.contains("\"name\":\"recall\""), "{request}");
        assert!(
            request.contains("\"arguments\":\"{\\\"query\\\":\\\"咖啡\\\"}\""),
            "工具参数字符串原样回传: {request}"
        );
        assert!(
            request.contains("\"tool_call_id\":\"call_1\""),
            "tool 消息必须带配对 tool_call_id（OpenAI 协议）: {request}"
        );
        assert!(
            request.contains("\"role\":\"tool\""),
            "工具结果消息应在: {request}"
        );
        socket
            .write_all(
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
                 {\"id\":\"c\",\"object\":\"chat.completion\",\"model\":\"m\",\
                 \"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}],\
                 \"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}"
                    .as_bytes(),
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
    });

    let request = LlmRequest {
        system: None,
        messages: vec![
            Message::User(UserMessage::new("查一下咖啡")),
            Message::Assistant(AssistantMessage::new(vec![ContentBlock::ToolUse {
                call: ToolCall {
                    call_id: "call_1".into(),
                    name: "recall".into(),
                    arguments: r#"{"query":"咖啡"}"#.into(),
                },
            }])),
            Message::Tool(ToolResultMessage {
                content: "无相关记忆".into(),
                call_id: Some("call_1".into()),
            }),
        ],
        tools: vec![],
    };
    let adapter = single_adapter(port);
    let mut stream = adapter.stream(&request);
    while let Some(item) = stream.next().await {
        assert!(item.is_ok(), "{item:?}");
    }
    server.await.unwrap();
}
