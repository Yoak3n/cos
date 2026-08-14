use serde_json::Value;

/// RPC 命令（`type` 字段的强类型化；`parse` 之外的字符串一律视为未知命令，
/// 杜绝手写字符串拼错导致静默分发错误）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 发送用户消息（异步接受；处理中须带 streamingBehavior）。
    Prompt,
    /// 排队 steering 消息。
    Steer,
    /// 排队后续消息。
    FollowUp,
    /// 取消队列中指定 id 的待处理消息（cos 扩展）。
    CancelMessage,
    /// 中止当前操作（保留排队）。
    Abort,
    /// 会话状态。
    GetState,
    /// 模型可见消息历史。
    GetMessages,
    /// 最后一条助手文本。
    GetLastAssistantText,
    /// 会话统计。
    GetSessionStats,
    /// 命令清单。
    GetCommands,
    /// 退出（cos 扩展）。
    Exit,
}

impl Command {
    /// 从请求的 `type` 字段解析；未知 → None。
    pub fn parse(request: &Value) -> Option<Command> {
        match request.get("type").and_then(Value::as_str) {
            Some("prompt") => Some(Command::Prompt),
            Some("steer") => Some(Command::Steer),
            Some("follow_up") => Some(Command::FollowUp),
            Some("cancel_message") => Some(Command::CancelMessage),
            Some("abort") => Some(Command::Abort),
            Some("get_state") => Some(Command::GetState),
            Some("get_messages") => Some(Command::GetMessages),
            Some("get_last_assistant_text") => Some(Command::GetLastAssistantText),
            Some("get_session_stats") => Some(Command::GetSessionStats),
            Some("get_commands") => Some(Command::GetCommands),
            Some("exit") => Some(Command::Exit),
            _ => None,
        }
    }

    /// wire 名称（响应 `command` 字段回显）。
    pub fn name(self) -> &'static str {
        match self {
            Command::Prompt => "prompt",
            Command::Steer => "steer",
            Command::FollowUp => "follow_up",
            Command::CancelMessage => "cancel_message",
            Command::Abort => "abort",
            Command::GetState => "get_state",
            Command::GetMessages => "get_messages",
            Command::GetLastAssistantText => "get_last_assistant_text",
            Command::GetSessionStats => "get_session_stats",
            Command::GetCommands => "get_commands",
            Command::Exit => "exit",
        }
    }
}
