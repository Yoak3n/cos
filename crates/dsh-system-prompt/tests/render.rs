//! P5：prompt 段装配 + 工具 schema 收集（文本快照测试）。

use dsh_core::Context;
use dsh_system_prompt::PromptSections;
use serde_json::json;

#[test]
fn render_produces_deterministic_snapshot() {
    let root = Context::root();
    let sections = PromptSections::new(&root);
    sections.append("persona", "你是一个助手。");
    sections.append("rules", "先思考再回答。");

    let tools = vec![json!({
        "type": "function",
        "function": {
            "name": "echo",
            "description": "回声",
            "parameters": {
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }
        }
    })];

    let rendered = sections.render(&tools);
    assert_eq!(
        rendered,
        "你是一个助手。\n\
         \n\
         先思考再回答。\n\
         \n\
         可用工具：\n\
         - echo: 回声\n\
         \x20 参数 (JSON Schema):\n\
         {\n  \"properties\": {\n    \"text\": {\n      \"type\": \"string\"\n    }\n  },\n  \"required\": [\n    \"text\"\n  ],\n  \"type\": \"object\"\n}"
    );
}

#[test]
fn render_without_tools_omits_tool_section() {
    let root = Context::root();
    let sections = PromptSections::new(&root);
    sections.append("persona", "你好。");
    assert_eq!(sections.render(&[]), "你好。");
}
