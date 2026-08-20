# cos

A plugin-based agent harness built on [Cordis](https://github.com/cordiverse/cordis). Every capability is a Cordis plugin living in its own workspace package under `packages/`, composed together by `cordis.yml` at boot time.

## 项目结构

```
cordis.yml             # 基础组装文件：每行一个插件（Loader 解析 @cos/* 包名）
cordis.patch.yml       # 用户补丁层，boot 时自动加载（最后应用）
overlays/              # 覆盖层：在基础 cordis.yml 之上做增删改
bundles/               # 组合层目录（非工作区包，用 --bundles 显式启用）
packages/              # 所有 @cos/* 工作区插件包
  boot/                #   启动逻辑：装配树、加载补丁/覆盖层、fail-loud
  llm/                 #   适配器注册表 + 流式组装
  llm-deepseek/        #   DeepSeek 真实适配器（默认）
  mock-llm/            #   本地 mock 模型（可选，测试用）
  credentials/         #   凭据无缝（读取 secrets.yml）
  session/ persistence/ agent-loop/ tools/ ...
main.ts                # 启动器 + 命令行驱动
secrets.example.yml    # 密钥文件模板（gitignored）
```

## 环境要求

- Node.js `^22`
- [pnpm](https://pnpm.io/)

首次运行先安装依赖：

```sh
pnpm install
```

## 快速开始

### 1. 配置密钥文件（默认真实模型需要密钥）

```sh
cp secrets.example.yml secrets.yml
# 编辑 secrets.yml，填入真实 DeepSeek API key：
#   deepseek:
#     apiKey: sk-...
```

`secrets.yml` 已被 gitignore，不会提交。

### 2. 运行

```sh
pnpm start --prompt "你好"
# 等价：pnpm dev -- --prompt "你好"
```

不带 `--prompt` 时，进程**常驻**并作为 **JSON-RPC 服务**在 `stdin/stdout` 上服务（与 `@cos/sidecar` 共用同一套新行分隔 JSON-RPC 协议，实现只有一份），供上一层应用驱动 agent，直到 `stdin` 关闭（EOF）才退出：

```sh
pnpm start    # 启动 JSON-RPC sidecar，等上一层应用通过 stdin/stdout 通信
```

### 3. 通过 JSON-RPC 与上一层应用通信

这是与上层应用集成的标准通道。协议基于 **新行分隔的 JSON-RPC 2.0**：每一行一个请求/响应，一行一个 `sidecar-ready` 通知（启动后先发）。上层应用可以直接用 `@cos/sidecar/client`（负责 spawn 进程并收发消息），也可以自己按协议用任意语言实现客户端：

- 启动后服务端先输出一行 `sidecar-ready`（含 providers）。
- 请求方法（每行 `{"jsonrpc":"2.0","id":N,"method":"…","params":{…}}`）：
  - `ping` → `{ ok, providers }`
  - `system.listProviders` / `system.model`
  - `agent.create`（可指定 `sessionId` / `agentOptions` / `meta` / `resume`）
  - `agent.followup`（向 agent 发一条用户消息）
  - `agent.whenIdle` / `agent.status`
  - `session.events`（读取会话事件流，含间隔 `since`）
- 示例（TypeScript，用内置客户端）：

```ts
import { SidecarClient } from '@cos/sidecar/client'

const sidecar = new SidecarClient({ cwd: 'E:/Project/VueProject/cos' })
await sidecar.ready
const { agent } = await sidecar.request('agent.create', {
  agentOptions: { provider: 'mock', model: 'mock-1' },
})
sidecar.request('agent.followup', { sessionId: agent, text: '你好', source: 'cli' })
await sidecar.request('agent.whenIdle', { sessionId: agent })
const { events } = await sidecar.request('session.events', { sessionId: agent })
console.log(events.at(-1))
sidecar.dispose()
```

> 同一套 JSON-RPC 服务也会被 `pnpm run build:sea` 打包进单文件可执行（见下文），两种入口协议完全一致。

## 模型选择：真实 DeepSeek（默认）vs mock

默认 `cordis.yml` 挂载的是 **真实 DeepSeek 适配器**（provider `deepseek-official`，模型 `deepseek-v4-flash`）。只要配置好 `secrets.yml` 里的 API key 即可直接用——默认就是真实模型，无需任何额外参数。

想用本地 **mock 模型**（本地回显、无需密钥、可离线调试）来测试，用 `overlays/mock.yml` 覆盖层切换即可：

通过环境变量（项目级默认）：

```sh
# PowerShell
$env:COS_OVERLAYS = "overlays/mock.yml"

# cmd / bash
set COS_OVERLAYS=overlays/mock.yml
export COS_OVERLAYS=overlays/mock.yml
```

或通过命令行参数（优先级更高，覆盖环境变量）：

```sh
pnpm start --overlays overlays/mock.yml --prompt "你好"
```

覆盖层是可叠加的：`COS_OVERLAYS` 环境变量先应用，随后是 `--overlays` 显式参数。真实模式下 DeepSeek 的 API key 通过 `@cos/credentials` 从 `secrets.yml` 读取（key `deepseek.apiKey`）；密钥缺失时启动会 fail-loud 并给出诊断。

## 其它启动选项

```sh
pnpm start --prompt "…" --provider <provider> --model <model>   # 显式指定路由
pnpm start --config path/to/cordis.yml                          # 指定基础组装文件
pnpm start --bundles <bundle>...                                # 命名 bundle 层
pnpm start --patch path/to/cordis.patch.yml                     # 指定用户补丁层
```

## 编译成单文件可执行（Node SEA）

可以把整个 sidecar（含全部 `@cos/*` 插件）打包成一个**真正单文件、无需 node_modules** 的可执行程序，适合作为对外交付的 sidecar：

```sh
pnpm run build:sea          # 产出 dist/cos-sidecar.exe
dist/cos-sidecar.exe        # 直接运行：boot 后走 JSON-RPC（stdin/stdout）
```

- 所有插件通过 `packages/sidecar/src/plugins.ts` 的注册表静态打包进二进制，loader 运行时从注册表取，不再 `import('@cos/x')` 找 node_modules。
- 二进制运行时从**当前工作目录**读取 `cordis.yml` / `secrets.yml`（真实 DeepSeek 仍需配套密钥文件）。
- 构建产物在 `dist/`（已 gitignore）。构建管线见 `scripts/build-sea.mjs`（esbuild 打包 → `--experimental-sea-config` 生成 blob → postject 注入 node.exe）。

## 常用命令

```sh
pnpm run typecheck   # 类型检查（TS7 原生编译器）
pnpm run build:sea   # 打包 sidecar 为单文件可执行
pnpm run scaffold    # 生成新插件骨架
```

## 故障排查

- **`deepseek.apiKey is required and unresolved`**：`secrets.yml` 缺失或未配置 `deepseek.apiKey`（或 `credentials.config.file` 指向的文件里没有该 key）。
- **真实模型没有输出 / 只有推理内容**：请先更新 `packages/llm` 与 `packages/credentials`（较旧版本存在文本组装与凭据读取的 bug，已修复）。
- **想关掉 system-prompt 调试打印**：应用 `overlays/quiet.yml`，或把 `cordis.yml` 中 agent-loop 行的 `debugSystemPrompt` 设为 `false`。

## 说明

- 这是独立于 DeepSeek Harness 的再造/教学实现，代码与文档约定遵循仓库根目录的 `AGENTS.md`。
- 仓库已 `git init` 并有首次提交；`.gitignore` 已忽略 `secrets.yml`、`.sessions/`、`node_modules/` 等，请不要把真实密钥提交进仓库。
