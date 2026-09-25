/**
 * 出厂种子播种与升级对账（B′ 工作区语义）。
 *
 * - 运行时唯一插件区 = `pluginRoot`（用户工作区，如 `cos/plugins`）；
 * - `seedDir`（安装目录 `plugins.seed/` 明文出厂镜像）只在播种/对账时读取；
 * - 每个官方插件三分支：`fresh` 整目录播种 / `update` 文件级静默更新 /
 *   `conflict` 保留用户版 + 官方新版入 `.incoming/<seedVersion>/<plugin>`；
 * - 用户自写插件（manifest 之外）不动；`.seed-state.json` 记录已应用基线，
 *   seedVersion 相同的启动零开销跳过。
 *
 * @module @cos/boot/seed
 */
import {
  cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync,
} from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import {
  classifyPlugin,
  deriveSeedVersion,
  hashContent,
  INCOMING_DIR,
  parseSeedManifest,
  SEED_MANIFEST_FILE,
  SEED_STATE_FILE,
  type SeedManifest,
} from './seed-manifest'

/** 播种/对账时忽略的目录与文件（依赖与编辑器产物不属出厂面）。 */
function ignored(name: string): boolean {
  return name === 'node_modules' || name.startsWith('.') || name.endsWith('.tsbuildinfo')
}

/** 递归收集 `<dir>` 相对路径 → 内容 hash（POSIX 斜杠键；目录缺失返回 undefined）。 */
export function hashTree(dir: string): Record<string, string> | undefined {
  if (!existsSync(dir)) return undefined
  const out: Record<string, string> = {}
  const walk = (rel: string): void => {
    for (const name of readdirSync(join(dir, rel))) {
      if (ignored(name)) continue
      const childRel = rel === '' ? name : `${rel}/${name}`
      const abs = join(dir, childRel)
      if (statSync(abs).isDirectory()) walk(childRel)
      else out[childRel] = hashContent(readFileSync(abs, 'utf8'))
    }
  }
  walk('')
  return out
}

/** 按忽略规则复制目录（播种 / .incoming 投放共用）。 */
function copyTree(src: string, dst: string): void {
  mkdirSync(dst, { recursive: true })
  cpSync(src, dst, {
    recursive: true,
    filter: (from) => !ignored(from.split(/[\\/]/).pop() ?? ''),
  })
}

export interface ApplySeedResult {
  status: 'up-to-date' | 'applied'
  seeded: string[]
  updated: string[]
  conflicts: string[]
  /** conflict 的官方新版投放目录（`.incoming/<seedVersion>`）；无冲突则缺省。 */
  incomingDir?: string
}

const linkType = process.platform === 'win32' ? 'junction' : 'dir'

/**
 * 解析链（B′ 用户工作区自洽的关键）：
 * 1. `<工作区父>/node_modules` → 内置依赖目录（sidecar/node_modules：@cos 映射、
 *    cordis/yaml/vendor、tsx/esbuild）——工作区脱离安装树后 walk-up 不再自然命中；
 * 2. `<工作区>/node_modules/@diver/<slug>` → `<工作区>/<slug>`——@diver 库的
 *    静态导入解析到**用户副本**（与挂载一致：改了 src 重启即生效）。
 * 两者都是 junction（Windows 免管理员），已有实体（如用户真实 node_modules）不动。
 */
export function linkWorkspaceDeps(pluginRoot: string, nodeModulesBase: string): string[] {
  const created: string[] = []
  const baseLink = join(dirname(pluginRoot), 'node_modules')
  if (!existsSync(baseLink) && existsSync(nodeModulesBase)) {
    symlinkSync(resolve(nodeModulesBase), baseLink, linkType)
    created.push(baseLink)
  }
  for (const dir of readdirSync(pluginRoot, { withFileTypes: true })) {
    if (!dir.isDirectory() || dir.name.startsWith('.') || dir.name === 'node_modules') continue
    const pjPath = join(pluginRoot, dir.name, 'package.json')
    if (!existsSync(pjPath)) continue
    let name: unknown
    try {
      name = (JSON.parse(readFileSync(pjPath, 'utf8')) as { name?: unknown }).name
    } catch {
      continue
    }
    if (typeof name !== 'string' || !name.startsWith('@diver/')) continue
    const linkDir = join(pluginRoot, 'node_modules', '@diver')
    const linkPath = join(linkDir, name.slice('@diver/'.length))
    if (existsSync(linkPath)) continue
    mkdirSync(linkDir, { recursive: true })
    symlinkSync(resolve(pluginRoot, dir.name), linkPath, linkType)
    created.push(linkPath)
  }
  return created
}

/**
 * 播种/对账入口：把 `seedDir` 的出厂镜像对账到 `pluginRoot` 用户工作区。
 * 版本标记相同直接返回 `up-to-date`（平时启动的零开销路径）。
 */
export function applySeed(opts: {
  seedDir: string
  pluginRoot: string
  log?: (message: string) => void
}): ApplySeedResult {
  const { seedDir, pluginRoot } = opts
  const log = opts.log ?? ((message) => console.log(message))
  const seed = parseSeedManifest(readFileSync(join(seedDir, SEED_MANIFEST_FILE), 'utf8'))
  let baseline: SeedManifest | undefined
  const statePath = join(pluginRoot, SEED_STATE_FILE)
  if (existsSync(statePath)) {
    try {
      baseline = parseSeedManifest(readFileSync(statePath, 'utf8'))
    } catch (error) {
      log(`[cos][seed] 基线档损坏，按无基线保守对账：${String(error)}`)
    }
  }
  if (baseline?.seedVersion === seed.seedVersion) {
    return { status: 'up-to-date', seeded: [], updated: [], conflicts: [] }
  }

  mkdirSync(pluginRoot, { recursive: true })
  const result: ApplySeedResult = { status: 'applied', seeded: [], updated: [], conflicts: [] }
  const incomingDir = join(pluginRoot, INCOMING_DIR, seed.seedVersion)
  for (const [name, entry] of Object.entries(seed.plugins)) {
    const target = join(pluginRoot, name)
    const plan = classifyPlugin(hashTree(target), baseline?.plugins[name], entry)
    if (plan === 'conflict') {
      copyTree(join(seedDir, name), join(incomingDir, name))
      result.conflicts.push(name)
      log(`[cos][seed] ${name} 已被修改 → 保留你的版本，官方新版放入 ${join(INCOMING_DIR, seed.seedVersion, name)}`)
      continue
    }
    if (plan === 'fresh') {
      copyTree(join(seedDir, name), target)
      result.seeded.push(name)
      continue
    }
    // update：文件级同步（保留 node_modules 与忽略项），删掉官方已移除的旧文件
    const nextFiles = Object.keys(entry.files)
    for (const old of Object.keys(baseline?.plugins[name]?.files ?? {})) {
      if (!entry.files[old]) rmSync(join(target, old), { force: true })
    }
    for (const file of nextFiles) {
      const dst = join(target, file)
      mkdirSync(dirname(dst), { recursive: true })
      cpSync(join(seedDir, name, file), dst)
    }
    result.updated.push(name)
    log(`[cos][seed] ${name} 未改动 → 静默更新`)
  }
  if (result.conflicts.length > 0) result.incomingDir = incomingDir
  writeFileSync(statePath, `${JSON.stringify(seed, null, 2)}\n`)
  linkWorkspaceDeps(pluginRoot, join(dirname(seedDir), 'node_modules'))
  log(`[cos][seed] 对账完成：播种 ${result.seeded.length} / 更新 ${result.updated.length} / 保留 ${result.conflicts.length}（${seed.seedVersion}）`)
  return result
}

/** 便捷封装：无 seedDir 或无 pluginRoot 时跳过（dev 布局零成本）。 */
export function seedIfNeeded(
  seedDir: string | undefined,
  pluginRoot: string | undefined,
  log?: (message: string) => void,
): ApplySeedResult | undefined {
  if (seedDir === undefined || pluginRoot === undefined) return undefined
  if (!existsSync(join(seedDir, SEED_MANIFEST_FILE))) return undefined
  const result = applySeed({ seedDir, pluginRoot, ...(log === undefined ? {} : { log }) })
  if (result.status === 'up-to-date') {
    // 版本没变也补链接（用户删了 node_modules 链接或首次升级到本布局）
    linkWorkspaceDeps(pluginRoot, join(dirname(seedDir), 'node_modules'))
  }
  return result
}

export { deriveSeedVersion }
