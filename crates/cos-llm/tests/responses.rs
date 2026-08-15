//! OpenAI Responses API 风格适配器（api style: "responses"）验收：本地回环 HTTP 打桩——
//! 请求形状（/responses + instructions/input/tools）、SSE 事件流式、非流式 output
//! 数组、函数调用（function_call）、错误码分类。

#![cfg(feature = "adapters")]

use cos_llm::{ChunkDelta, FinishReason, InputContent, LlmAdapter, LlmErrorCode, LlmRequest};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn adapter(port: u16) -> cos_llm::ResponsesAdapter {
    cos_llm::ResponsesAdapter::new(cos_llm::ResponsesConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-key".into(),
        model: "grok-4.5".into(),
        streaming: true,
        max_tokens: Some(2048),
        input_content: vec![InputContent::Text],
    })
}

fn single_adapter(port: u16) -> cos_llm::ResponsesAdapter {
    cos_llm::ResponsesAdapter::new(cos_llm::ResponsesConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-key".into(),
        model: "grok-4.5".into(),
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

/// 请求形状：/responses 路径、Bearer 头、instructions/input/tools（扁平转换）。
#[tokio::test]
async fn request_uses_responses_path_and_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"r\",\"object\":\"response\",\
             \"output\":[{\"type\":\"message\",\"role\":\"assistant\",\
             \"content\":[{\"type\":\"output_text\",\"text\":\"你好\"}]}],\
             \"usage\":{\"input_tokens\":10,\"output_tokens\":4}}",
        ],
        move |request| {
            assert!(request.contains("POST /v1/responses"), "{request}");
            assert!(
                request
                    .to_lowercase()
                    .contains("authorization: bearer test-key"),
                "{request}"
            );
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["model"], "grok-4.5");
            assert_eq!(body["instructions"], "你是助手");
            assert_eq!(body["max_output_tokens"], 2048);
            assert_eq!(body["stream"], false);
            assert_eq!(body["input"][0]["role"], "user");
            assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
            // 工具转换：嵌套 function → 扁平 {type:"function", name, parameters}
            assert_eq!(body["tools"][0]["type"], "function");
            assert_eq!(body["tools"][0]["name"], "recall");
            assert_eq!(body["tools"][0]["parameters"]["type"], "object");
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

/// SSE 事件流式：output_text.delta → 文本；completed → Finish{Stop}。
#[tokio::test]
async fn streaming_text_events_emit_text_and_finish() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"type\":\"response.created\",\"response\":{}}\n\n\
             data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"你好\"}\n\n\
             data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"text\":\"你好\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":4}}}\n\n",
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

/// 函数调用：output_item.added + function_call_arguments.delta + done → ToolUse。
#[tokio::test]
async fn streaming_function_call_events_become_tool_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
             data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"fc_1\",\"name\":\"recall\"}}\n\n\
             data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"query\\\":\\\"\"}\n\n\
             data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"咖啡\\\"}\"}\n\n\
             data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"fc_1\",\"name\":\"recall\",\"arguments\":\"{\\\"query\\\":\\\"咖啡\\\"}\"}}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":4}}}\n\n",
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
    assert_eq!(calls[0].call_id, "fc_1");
    assert_eq!(calls[0].name, "recall");
    assert_eq!(calls[0].arguments, r#"{"query":"咖啡"}"#);
    assert_eq!(finish, Some(FinishReason::ToolCalls));
}

/// 非流式：output 数组（message + function_call）→ 文本 + ToolUse + Finish{ToolCalls}。
#[tokio::test]
async fn single_shot_parses_output_array() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec![
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n\
             {\"id\":\"r\",\"object\":\"response\",\
             \"output\":[\
               {\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"查一下\"}]},\
               {\"type\":\"function_call\",\"call_id\":\"fc_9\",\"name\":\"recall\",\"arguments\":\"{\\\"query\\\":\\\"咖啡\\\"}\"}\
             ],\
             \"usage\":{\"input_tokens\":10,\"output_tokens\":4}}",
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

/// 错误分类：429 → RateLimit 码 + status 事实。
#[tokio::test]
async fn errors_carry_codes_and_status_facts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve(
        listener,
        vec!["HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n\r\n{}"],
        |_| {},
    ));
    let adapter = single_adapter(port);
    let mut stream = adapter.stream(&request());
    let error = stream.next().await.unwrap().unwrap_err();
    server.await.unwrap();
    assert_eq!(error.code, LlmErrorCode::RateLimit, "{error}");
    assert_eq!(error.facts.unwrap().status, Some(429));
}
