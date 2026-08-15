//! 运行配置。
//!

use std::sync::{Arc, atomic::AtomicBool};

use cos_llm::Message;
use cos_session::SessionEvent;

/// 库嵌入模式：`RunConfig { session_id: "my-app".into(), ..Default::default() }`
/// （`config_path: None` = 零插件装配）。
#[derive(Debug, Clone, Default)]
pub struct RunConfig {
    /// cordis.yml 路径；`None` = 零插件装配（库嵌入模式，不装载任何插件）。
    pub config_path: Option<String>,
    /// 只输出装载计划（不启动）。
    pub dump_config: bool,
    /// 会话 id。
    pub session_id: String,
    /// 一次性模式的用户消息（None = 交互/RPC 模式）。
    pub prompt: Option<String>,
    /// 会话 JSONL 输出路径（None = 不落盘）。
    pub session_path: Option<String>,
    /// 外部取消信号（main 的 Ctrl-C 监视任务写入）。
    pub cancel: Option<Arc<AtomicBool>>,
    /// 真实 LLM 配置（`--llm-*`，注册为 "default"）；None 时须由 yml plugin-llm 提供，否则启动失败。
    pub llm: Option<LlmConfig>,
    /// 主 agent 的 LLM 提供商/后备链 id（LLM 统一管理；None = `--llm-*` 的 "default" 或 yml 自动解析）。
    pub agent_llm: Option<String>,
    /// 主 agent 驱动 id（`agent_factory!` 注册表；None = "loop"；未知 id → 启动失败）。
    pub agent_driver: Option<String>,
    /// `--patch` 附加层（P13：按顺序应用，后覆盖先；相对 cwd 解析）。
    pub patch_files: Vec<String>,
}

/// 真实 LLM 配置（opencode-go 等 OpenAI 兼容端点）。
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// base URL（不带 `/chat/completions` 后缀）。
    pub base_url: String,
    /// API key。
    pub api_key: String,
    /// 模型 id。
    pub model: String,
    /// 是否流式（false = 非流式单次；opencode zen/go 流式只出推理文本，建议 false）。
    pub streaming: bool,
}

/// 运行报告（测试与 CLI 打印消费）。
pub struct RunReport {
    /// `--dump-config` 的计划 JSON（仅 dump 模式）。
    pub dump: Option<String>,
    /// 优雅卸载顺序（apply 逆序）。
    pub unload_order: Vec<String>,
    /// 完整会话事件（快照/重放用）。
    pub events: Vec<SessionEvent>,
    /// 模型可见消息（derive_messages）。
    pub messages: Vec<Message>,
    /// 不变量违规（空 = 全过）。
    pub violations: Vec<String>,
    /// 卸载后插件服务已反注册（审计）。
    pub services_after_unload: bool,
}
