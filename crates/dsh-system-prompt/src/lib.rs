//! dsh-system-prompt —— prompt 段装配 + 工具 schema 收集（P5）。
//!
//! 语义参考：`packages/core/system-prompt/src/`（P5 简化：有序段列表 + 工具清单，
//! 变量/条件段等高级机制留待后续阶段；渲染文本确定性、可快照）。

#![warn(missing_docs)]

use std::sync::Mutex;

use dsh_core::{Context, Service};

/// 一段 prompt。
#[derive(Debug, Clone, PartialEq)]
pub struct PromptSection {
    /// 段名（如 "persona"、"tools"）。
    pub name: String,
    /// 段文本。
    pub text: String,
}

/// prompt 装配服务（`ctx.provide` 为 `"system-prompt"`）。
pub struct PromptSections {
    sections: Mutex<Vec<PromptSection>>,
}

impl Service for PromptSections {
    const NAME: &'static str = "system-prompt";
}

impl PromptSections {
    /// 空装配器。
    pub fn new(_root: &Context) -> Self {
        Self {
            sections: Mutex::new(Vec::new()),
        }
    }

    /// 追加一段（装配顺序 = 追加顺序）。
    pub fn append(&self, name: impl Into<String>, text: impl Into<String>) {
        self.sections.lock().unwrap().push(PromptSection {
            name: name.into(),
            text: text.into(),
        });
    }

    /// 当前段快照。
    pub fn sections(&self) -> Vec<PromptSection> {
        self.sections.lock().unwrap().clone()
    }

    /// 渲染完整 system prompt：各段按序以空行分隔，末尾附工具清单段。
    ///
    /// 确定性输出（可快照测试）；工具清单按注册表名字序。
    pub fn render(&self, tools: &[serde_json::Value]) -> String {
        let mut parts: Vec<String> = self
            .sections()
            .into_iter()
            .map(|section| section.text)
            .collect();
        if !tools.is_empty() {
            let mut lines: Vec<String> = Vec::new();
            for schema in tools {
                let function = &schema["function"];
                let name = function["name"].as_str().unwrap_or("?");
                let description = function["description"].as_str().unwrap_or("");
                let parameters = serde_json::to_string_pretty(&function["parameters"])
                    .unwrap_or_else(|_| "{}".into());
                lines.push(format!(
                    "- {name}: {description}\n  参数 (JSON Schema):\n{parameters}"
                ));
            }
            parts.push(format!("可用工具：\n{}", lines.join("\n")));
        }
        parts.join("\n\n")
    }
}
