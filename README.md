# cos

A plugin-based agent harness built on [Cordis](https://github.com/cordiverse/cordis). Every capability is a Cordis plugin living in its own workspace package under `packages/`, composed together by `cordis.yml` at boot time.

仓库：`https://github.com/Yoak3n/cos`（也可作为 [diver](https://github.com/Yoak3n/diver) 的 `harness/` submodule 使用）。

## 项目结构

```text
cordis.yml             # 基础组装：每行一个插件（Loader 解析 @cos/* 包名）
cordis.patch.yml       # 用户补丁层，boot 自动加载（最后应用）
overlays/              # 覆盖层：在基础 cordis.yml 之上增删改
bundles/               # 组合层目录（非工作区包，用 --bundles 显式启用）
packages/              # 所有 @cos/* 工作区插件包
  boot/                #   启动装配、补丁/覆盖层、fail-loud
  llm/  llm-deepseek/  #   适配器注册表 + DeepSeek 真实适配器（默认）
  mock-llm/            #   本地 mock（可选，离线调试）
  credentials/         #   凭据（读取 secrets.yml）
  session/ persistence/ agent-loop/ tools/ types/
  system-prompt/ persona/ scope/ skills/ subagents/
  profile/             #   DSH 风格 profile（bundles + 用户补丁）
  plugin-api/          #   第三方插件类型面（含 Context 增强）
  sidecar/             #   JSON-RPC sidecar（client / server / sea）
  dsh/                 #   DSH 兼容层（包名 @deepseek-ai/dsh-*，目录收拢）
    agent/ llm/ scope/ session/ system-prompt/ tools/
main.ts                # one-shot CLI + 常驻 JSON-RPC 入口
secrets.example.yml    # 密钥模板（secrets.yml 已 gitignore）
```

## 环境要求

- Node.js `^22`
- [pnpm](https://pnpm.io/)

```sh
pnpm install
```

## 快速开始

### 1. 配置密钥

```sh
cp secrets.example.yml secrets.yml
# 编辑 secrets.yml：
#   deepseek:
#     apiKey: sk-...
```

### 2. 运行

```sh
pnpm start --prompt "你好"
# 等价：pnpm dev -- --prompt "你好"
```

不带 `--prompt` 时进程**常驻**，作为 **JSON-RPC 服务**在 `stdin/stdout` 上工作（与 `@cos/sidecar` 共用同一套新行分隔 JSON-RPC 2.0），供上层应用驱动 agent，直到 `stdin` EOF 才退出：

```sh
pnpm start
```

### 3. JSON-RPC 集成

- 启动后先输出一行 `sidecar-ready`（含 providers）。
- 请求方法（每行 `{"jsonrpc":"2.0","id":N,"method":"…","params":{…}}`）：
  - `ping` → `{ ok, providers }`
  - `system.listProviders` / `system.model`
  - `agent.create`（可指定 `sessionId` / `agentOptions` / `meta` / `resume`）
  - `agent.followup`
  - `agent.whenIdle` / `agent.status`
  - `session.events`（事件流，支持 `since`）

```ts
import { SidecarClient } from '@cos/sidecar/client'

const sidecar = new SidecarClient({ cwd: '/path/to/cos' })
await sidecar.ready
const { agent } = await sidecar.request('agent.create', {
  agentOptions: { provider: 'mock', model: 'mock-1' },
})
await sidecar.request('agent.followup', { sessionId: agent, text: '你好', source: 'cli' })
await sidecar.request('agent.whenIdle', { sessionId: agent })
const { events } = await sidecar.request('session.events', { sessionId: agent })
console.log(events.at(-1))
sidecar.dispose()
```

同一套协议也打进 SEA 单文件（见下文）。

## 模型：真实 DeepSeek（默认）vs mock

默认 `cordis.yml` 挂载 **DeepSeek**（provider `deepseek-official`，模型 `deepseek-v4-flash`）。配好 `secrets.yml` 即可直接用。

离线/无密钥调试用 mock：

```sh
# 环境变量
$env:COS_OVERLAYS = "overlays/mock.yml"    # PowerShell
export COS_OVERLAYS=overlays/mock.yml      # bash

# 或命令行（优先级更高）
pnpm start --overlays overlays/mock.yml --prompt "你好"
```

覆盖层可叠加。密钥缺失时启动 fail-loud。

## 其它启动选项

```sh
pnpm start --prompt "…" --provider <p> --model <m>
pnpm start --config path/to/cordis.yml
pnpm start --bundles <bundle>...
pnpm start --patch path/to/cordis.patch.yml
pnpm start --plugin-root <dir>    # 开放插件根（@scope/name → <dir>/<name>）
pnpm start --profile <name>       # DSH 风格 profile
```

## 第三方插件

- 类型面：`@cos/plugin-api`（re-export 公共类型，并加载核心服务以合并 Cordis `Context`）。
- DSH 写法可用：`import { defineTool } from '@deepseek-ai/dsh-tools'`（或 `@cos/plugin-api` / `@cos/tools`）。
- 示例（可选）：把 `hello-external` 装到仓库旁 `../cos-plugins/hello-external`，再取消 `cordis.patch.yml` 里注释掉的 `insert`。
- 能力约定见 [docs/plugins.md](docs/plugins.md)。

## 存储布局

- 会话日志：`$COS_HOME/sessions/`（无 `COS_HOME` 时为 `./sessions/`）
- 运行时设置：`$COS_HOME/cos-settings.json`（provider 配置等）
- 密钥：`secrets.yml`（或 `credentials.config.file` 指向的文件）

## 编译成单文件可执行（Node SEA）

```sh
pnpm run build:sea          # 产出 dist/cos-sidecar.exe
dist/cos-sidecar.exe        # boot 后走 JSON-RPC（stdin/stdout）
```

- 插件经 `packages/sidecar/src/plugins.ts` 注册表静态打入。
- 运行时从**当前工作目录**读 `cordis.yml` / `secrets.yml`。
- 构建细节见 `scripts/build-sea.mjs`。

## 常用命令

```sh
pnpm run typecheck   # 类型检查
pnpm run build:sea   # 打包单文件 sidecar
pnpm run scaffold    # 新插件骨架
```

## 故障排查

- **`deepseek.apiKey is required and unresolved`**：`secrets.yml` 缺失或未配置 `deepseek.apiKey`。
- **想关掉 system-prompt 调试打印**：用 `overlays/quiet.yml`，或把 agent-loop 的 `debugSystemPrompt` 设为 `false`。

## 说明

- 独立于 DeepSeek Harness 的再造/教学实现；概念对齐 DSH，实现不 fork 上游。
- DSH 兼容包集中在 `packages/dsh/`，npm 包名仍为 `@deepseek-ai/dsh-*`，社区插件 import 不变。
- 被 [diver](https://github.com/Yoak3n/diver) 以 submodule 方式挂在 `harness/`；产品层插件（`@diver/*`）不进本仓库。
- 请勿提交真实密钥（`secrets.yml` / `sessions/` / `.cos-home/` 均已 gitignore）。
