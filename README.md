# cos

A plugin-based agent harness built on [Cordis](https://github.com/cordiverse/cordis). Every capability is a Cordis plugin living in its own workspace package under `packages/`, composed together by `cordis.yml` at boot time.

## 项目结构

```
cordis.yml             # 基础组装文件：每行一个插件（Loader 解析 @cos/* 包名）
cordis.patch.yml       # 用户补丁层，boot 时自动加载（最后应用）
overlays/              # 覆盖层：在基础 cordis.yml 之上做增删改
packages/              # 所有 @cos/* 工作区包
  boot/                #   启动逻辑：装配树、加载补丁/覆盖层、fail-loud
  llm/                 #   适配器注册表 + 流式组装
  llm-deepseek/        #   DeepSeek 真实适配器
  mock-llm/            #   本地 mock 模型（默认）
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

### 1. 配置密钥文件（使用真实模型时需要）

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

不带 `--prompt` 启动时只做一次就绪自检（boot 整棵树后干净退出），可用于判断装配是否成功：

```sh
pnpm start
```

## 模型选择：mock vs 真实 DeepSeek

默认 `cordis.yml` 挂载的是 **mock 模型**（本地回显，无需密钥、可离线调试）。想用真实 DeepSeek，用 `overlays/real.yml` 覆盖层替换即可。

通过环境变量（项目级默认）：

```sh
# PowerShell
$env:COS_OVERLAYS = "overlays/real.yml"

# cmd / bash
set COS_OVERLAYS=overlays/real.yml
export COS_OVERLAYS=overlays/real.yml
```

或通过命令行参数（优先级更高，覆盖环境变量）：

```sh
pnpm start --overlays overlays/real.yml --prompt "你好"
```

覆盖层是可叠加的：`COS_OVERLAYS` 环境变量先应用，随后是 `--overlays` 显式参数。真实模式下 DeepSeek 的 API key 通过 `@cos/credentials` 从 `secrets.yml` 读取（key `deepseek.apiKey`）；密钥缺失时启动会 fail-loud 并给出诊断。

## 其它启动选项

```sh
pnpm start --prompt "…" --provider <provider> --model <model>   # 显式指定路由
pnpm start --config path/to/cordis.yml                          # 指定基础组装文件
pnpm start --bundles <bundle>...                                # 命名 bundle 层
pnpm start --patch path/to/cordis.patch.yml                     # 指定用户补丁层
```

## 常用命令

```sh
pnpm run typecheck   # 类型检查
pnpm run scaffold    # 生成新插件骨架
```

## 故障排查

- **`deepseek.apiKey is required and unresolved`**：`secrets.yml` 缺失或未配置 `deepseek.apiKey`（或 `credentials.config.file` 指向的文件里没有该 key）。
- **真实模型没有输出 / 只有推理内容**：请先更新 `packages/llm` 与 `packages/credentials`（较旧版本存在文本组装与凭据读取的 bug，已修复）。
- **想关掉 system-prompt 调试打印**：应用 `overlays/quiet.yml`，或把 `cordis.yml` 中 agent-loop 行的 `debugSystemPrompt` 设为 `false`。

## 说明

- 这是独立于 DeepSeek Harness 的再造/教学实现，代码与文档约定遵循仓库根目录的 `AGENTS.md`。
- 仓库尚无提交历史（git 全为未跟踪文件），后续可按需 `git init` 后提交。
