# RPC 模式（协议对齐 pi）

`cos --rpc` 提供无头运行：stdin/stdout 上的 JSONL 协议，供 IDE、自定义 UI 或脚本嵌入。
协议形态与 [pi 的 RPC 模式](https://github.com/gaianet/pi/blob/main/packages/coding-agent/docs/rpc.md) 对齐
（命令/响应/事件信封、流式事件、`id` 关联），实现为 cos 当前能力的子集。

## 启动

```bash
cos --config cordis.yml --rpc [--session <id>] [--no-save]
```

## 协议概览

- **命令**：stdin 每行一个 JSON 对象
- **响应**：`{"id"?, "type": "response", "command": "<命令>", "success": bool, "data"?, "error"?}`
- **事件**：处理期间 stdout 实时流式输出（JSONL）

帧格式：严格 JSONL，LF（`\n`）为唯一分隔；输入可带尾部 `\r`（自动剥离）。
`id` 可选：提供则响应原样回显。

## 命令

| 命令 | 说明 |
|---|---|
| `prompt` | 发送用户消息（异步接受；响应先到，事件随后流式）。正在处理时须带 `streamingBehavior`: `"steer"`（当前 turn 工具执行完后、下一次模型调用前送达）或 `"followUp"`（agent 空闲后送达）；不指定则报错。响应 `data.messageId` = 该消息的排队 id（命令 `id` 即消息 id；缺省自动生成） |
| `steer` | 排队 steering 消息（工具执行完后、下一次模型调用前送达） |
| `follow_up` | 排队后续消息（agent 处理完后送达） |
| `abort` | 中止当前操作（保留已排队消息） |
| `cancel_message` | **cos 扩展**：取消队列中指定 id 的待处理消息（已开始处理的无法取消） |
| `get_state` | `{isStreaming, sessionId, sessionName, messageCount, pendingMessageCount}` |
| `get_messages` | 模型可见消息历史（pi 风格 role/content） |
| `get_last_assistant_text` | 最后一条助手文本（无则 `text: null`） |
| `get_session_stats` | `{sessionId, userMessages, assistantMessages, toolCalls, toolResults, totalMessages, tokens}` |
| `get_commands` | 命令清单（当前为空列表） |
| `exit` | **cos 扩展**：响应后优雅退出（pi 客户端直接杀进程） |

未实现命令（`set_model`、`compact`、`bash`、会话树等）返回 `success: false` + `error`，协议兼容。

### prompt 示例

```json
{"id": "req-1", "type": "prompt", "message": "你好"}
{"id": "req-1", "type": "response", "command": "prompt", "success": true, "data": {"messageId": "req-1"}}
```

带图片（pi `ImageContent` 格式 → data URL）：

```json
{"type": "prompt", "message": "这是什么？", "images": [{"type": "image", "data": "<base64>", "mimeType": "image/png"}]}
```

### 取消排队消息

`prompt`/`steer`/`follow_up` 的响应带 `data.messageId`（命令 `id` 即消息 id；缺省自动生成
`m-<n>`）。处理中（agent 忙）排队的消息可精准取消；已开始处理的消息不在队列中，取消失败。
**排队去重**：id 已在队列中时重复排队会被拒绝（`success: false` + "message id 已存在于队列"），
避免 `cancel_message` 产生歧义；已消费的 id 可以复用（队列中无同名消息即可）：

```json
{"id": "req-2", "type": "prompt", "message": "任务A", "streamingBehavior": "followUp"}
{"id": "req-3", "type": "cancel_message", "messageId": "req-2"}
{"id": "req-3", "type": "response", "command": "cancel_message", "success": true, "data": {"cancelled": true}}
```

## 事件

事件由事件转发器从会话日志（唯一事实源）增量投影，实时流式输出：

| 事件 | 说明 |
|---|---|
| `agent_start` | turn 开始前发出（cos 近似：一轮 = 一次 run） |
| `turn_start` / `turn_end` | 带 `turn` 号；`turn_end` 带 `reason`（completed/error/aborted/blocked/…） |
| `message_start` / `message_update` / `message_end` | assistant 消息生命周期；`message_end.message` 为准 |
| `tool_execution_start` / `tool_execution_end` | 工具执行（`toolCallId` 关联；`tool_execution_end` 带结果与 `isError`） |
| `agent_end` / `agent_settled` | 一轮收束（`willRetry: false`） |

`message_update.assistantMessageEvent` 区分推理与正文：`thinking_start` / `thinking_delta` /
`thinking_end`（`reasoning_content`）、`text_start` / `text_delta` / `text_end`、
`toolcall_start` / `toolcall_end`（含完整 `toolCall`，适配器在流尾一次性合成）。

**内部拼接**：每条 `message_update` 的 `assistantMessageEvent` 除增量（`delta`）外携带
`partial` 与 `message`——**已拼接好的累积消息快照**（`content` 按块累积：thinking / text /
toolCall），客户端直接展示快照即可，无需自行拼增量。快照含元数据：
`role` / `content` / `api` / `provider` / `model` / `usage`（input/output/totalTokens，
cost 未核算恒零）/ `stopReason`（流式中 `pending`；`message_end` 为 `stop` 或 `toolUse`）/
`timestamp`。

```json
{"type": "message_update", "assistantMessageEvent": {
  "type": "text_delta", "contentIndex": 1, "delta": "你好",
  "partial": {"role": "assistant", "content": [
    {"type": "thinking", "thinking": "……", "thinkingSignature": "reasoning_content"},
    {"type": "text", "text": "你好"}
  ], "api": "openai-completions", "provider": "opencode-go",
  "model": "deepseek-v4-flash", "usage": {"input": 0, "output": 0, "cacheRead": 0,
  "cacheWrite": 0, "totalTokens": 0, "cost": {"input": 0, "output": 0, "cacheRead": 0,
  "cacheWrite": 0, "total": 0}}, "stopReason": "pending", "timestamp": 1786706685357},
  "message": {…同 partial…}
}}

## 错误处理

```json
{"type": "response", "command": "set_model", "success": false, "error": "未知命令: set_model"}
{"type": "response", "command": "parse", "success": false, "error": "Failed to parse command: ..."}
```

## 最小客户端（Python）

```python
import subprocess, json

proc = subprocess.Popen(
    ["cos", "--config", "cordis.yml", "--rpc", "--no-save"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

def send(cmd):
    proc.stdin.write(json.dumps(cmd) + "\n"); proc.stdin.flush()

send({"id": "req-1", "type": "prompt", "message": "你好"})
print(json.loads(proc.stdout.readline()))  # response

for line in proc.stdout:
    event = json.loads(line)
    if event.get("type") == "message_update":
        delta = event.get("assistantMessageEvent", {})
        if delta.get("type") == "text_delta":
            print(delta["delta"], end="", flush=True)
    if event.get("type") == "agent_settled":
        break
```
