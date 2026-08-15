//! Anthropic Messages 风格适配器（api style: "anthropic"）验收：本地回环 HTTP 打桩——
//! 请求形状（/messages + system/messages/tools/max_tokens）、SSE 事件流式、
//! 非流式 content 块、工具调用（tool_use）、错误码分类。

#![cfg(feature = "adapters")]

use cos_llm::{ChunkDelta, FinishReason, InputContent, LlmAdapter, LlmErrorCode, LlmRequest};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn adapter(port: u16) -> cos_llm::AnthropicAdapter {
    cos_llm::AnthropicAdapter::new(cos_llm::AnthropicConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-key".into(),
        model: "minimax-m3".into(),
        streaming: true,
        max_tokens: Some(2048),
        input_content: vec![InputContent::Text],
    })
}

fn single_adapter(port: u16) -> cos_llm::AnthropicAdapter {
    cos_llm::AnthropicAdapter::new(cos_llm::AnthropicConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-key".into(),
        model: "minimax-m3".into(),
        streaming: false,
        max_tokens: Some(2048),
        input_content: vec![InputContent::Text],
    })
}

fn request() -> LlmRequest {
    LlmRequest {
        system: Some("你是助手".into()),
        messages: vec![cos_llm::Message::User(cos_llm::UserMessage::new("你好"))],
        tools: vec![serde_json::json!({
            "type": "function",
            "function": { "name": "recall", "parameters": { "type": "object" } }
        })],
    }
}

/// 处理 N 个连接：读请求头断言形状 → 写回固定响应。
async fn serve(
    listener: TcpListener,
    responses: Vec<&'static str>,
    assert: impl Fn(&str) + Send + 'static,
) {
    for response in responses {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 65536];
        let n = socket.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        assert(&request);
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    }
}

/// 请求形状：/messages 路径、x-api-key 头、system/messages/max_tokens/tools。
#[tokio::test]
async fn request_uses_messages_path_and_anthropic_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\
             \"content\":[{\"type\":\"text\",\"text\":\"你好\"}],\
             \"usage\":{\"input_tokens\":10,\"output_tokens\":4},\"stop_reason\":\"end_turn\"}",
        ],
        move |request| {
            assert!(request.contains("POST /v1/messages"), "{request}");
            assert!(
                request.to_lowercase().contains("x-api-key: test-key"),
                "{request}"
            );
            assert!(request.contains("anthropic-version"), "{request}");
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["model"], "minimax-m3");
            assert_eq!(body["system"], "你是助手");
            assert_eq!(body["max_tokens"], 2048);
            assert_eq!(body["stream"], false);
            // 工具转换：OpenAI 形状 → input_schema
            assert_eq!(body["tools"][0]["name"], "recall");
            assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        },
    ));

    let adapter = single_adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let ChunkDelta::Text { text: delta } = item.unwrap().delta {
            text.push_str(&delta);
        }
    }
    server.await.unwrap();
    assert_eq!(text, "你好");
}

/// SSE 事件流式：content_block_delta(text_delta) → 文本；message_stop → Finish{Stop}。
#[tokio::test]
async fn streaming_text_events_emit_text_and_finish() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n\
             event: content_block_stop\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":4}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n",
        ],
        |_| {},
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    let mut finish = None;
    while let Some(item) = stream.next().await {
        match item.unwrap().delta {
            ChunkDelta::Text { text: delta } => text.push_str(&delta),
            ChunkDelta::Finish { reason } => finish = Some(reason),
            _ => {}
        }
    }
    server.await.unwrap();
    assert_eq!(text, "你好");
    assert_eq!(finish, Some(FinishReason::Stop));
}

/// 工具调用：content_block_start(tool_use) + input_json_delta 累积 + stop → ToolUse。
#[tokio::test]
async fn streaming_tool_use_blocks_become_tool_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"recall\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\\\"\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"咖啡\\\"}\"}}\n\n\
             event: content_block_stop\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n",
        ],
        |_| {},
    ));

    let adapter = adapter(port);
    let mut stream = adapter.stream(&request());
    let mut calls = Vec::new();
    let mut finish = None;
    while let Some(item) = stream.next().await {
        match item.unwrap().delta {
            ChunkDelta::ToolUse { call } => calls.push(call),
            ChunkDelta::Finish { reason } => finish = Some(reason),
            _ => {}
        }
    }
    server.await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "toolu_1");
    assert_eq!(calls[0].name, "recall");
    assert_eq!(calls[0].arguments, r#"{"query":"咖啡"}"#);
    assert_eq!(finish, Some(FinishReason::ToolCalls));
}

/// 非流式：content 块数组（text + tool_use）→ 文本 + ToolUse + Finish{ToolCalls}。
#[tokio::test]
async fn single_shot_parses_content_blocks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\
             \"content\":[{\"type\":\"text\",\"text\":\"查一下\"},{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"recall\",\"input\":{\"query\":\"咖啡\"}}],\
             \"usage\":{\"input_tokens\":10,\"output_tokens\":4},\"stop_reason\":\"tool_use\"}",
        ],
        |_| {},
    ));

    let adapter = single_adapter(port);
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut finish = None;
    while let Some(item) = stream.next().await {
        match item.unwrap().delta {
            ChunkDelta::Text { text: delta } => text.push_str(&delta),
            ChunkDelta::ToolUse { call } => calls.push(call),
            ChunkDelta::Finish { reason } => finish = Some(reason),
            _ => {}
        }
    }
    server.await.unwrap();
    assert_eq!(text, "查一下");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, r#"{"query":"咖啡"}"#);
    assert_eq!(finish, Some(FinishReason::ToolCalls));
}

/// 错误分类：401 → Auth 码 + status 事实；SSE error 事件 → 按 error.type 分类。
#[tokio::test]
async fn errors_carry_codes_and_events_classify() {
    // HTTP 401
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec!["HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n{}"],
        |_| {},
    ));
    let single = single_adapter(port);
    let mut stream = single.stream(&request());
    let error = stream.next().await.unwrap().unwrap_err();
    server.await.unwrap();
    assert_eq!(error.code, LlmErrorCode::Auth, "{error}");
    assert_eq!(error.facts.unwrap().status, Some(401));

    // SSE error 事件（invalid_request_error——不可重试，原样交付分类）
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             event: error\n\
             data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad request\"}}\n\n",
        ],
        |_| {},
    ));
    let streaming = adapter(port);
    let mut stream = streaming.stream(&request());
    let error = stream.next().await.unwrap().unwrap_err();
    server.await.unwrap();
    assert_eq!(error.code, LlmErrorCode::InvalidRequest, "{error}");
}
