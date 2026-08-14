//! cos-test-support —— 测试支持库（**仅 dev-dependencies 引用，不进正式二进制**）。
//!
//! 内容：
//! - [`MockAdapter`] / [`MockReply`]：确定性脚本化 mock LLM 适配器（原 cos-llm-mock，
//!   运行时 Provider 已删除，测试桩保留于此）；
//! - [`ScriptedChatServer`]：本地回环 HTTP `chat/completions` 服务器（非流式，
//!   OpenAI 响应形状与 cos-llm 的 openai feature 适配器对齐）——e2e 经 `--llm-*` 指向它，
//!   使真实 CLI 链路（含真实适配器协议）可离线确定性测试。
//!
//! 纪律：本 crate **不注册任何 `llm_factory!`/`plugin!` 条目**——它只服务测试二进制，
//! 不得以任何方式进入运行时工厂表。

#![warn(missing_docs)]

mod mock;
mod server;

pub use mock::{MockAdapter, MockReply};
pub use server::{ChatReply, ScriptedChatServer, spawn_sync};
