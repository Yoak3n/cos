/**
 * 出厂种子 manifest 判据层（B′ 播种对账的纯逻辑）。
 *
 * schema（构建侧 scripts/lib/seed-manifest.mjs 同算法生成）：
 * ```json
 * {
 *   "schemaVersion": 1,
 *   "seedVersion": "<内容派生 16 位 hex>",
 *   "generatedAt": "…",
 *   "plugins": { "memory": { "files": { "src/index.ts": "<sha256hex>", … } } }
 * }
 * ```
 * - hash = 归一化换行（\r\n → \n）后的 UTF-8 内容 sha256；CRLF 编辑不误判「改过」。
 * - seedVersion 由逐插件逐文件 hash 列表派生：内容相同 → 版本相同，
 *   未变的重建不触发对账。
 *
 * @module @cos/boot/seed-manifest
 */
import { createHash } from 'node:crypto'

export const SEED_MANIFEST_FILE = 'seed-manifest.json'
export const SEED_STATE_FILE = '.seed-state.json'
export const INCOMING_DIR = '.incoming'

export interface SeedPluginEntry { files: Record<string, string> }
export interface SeedManifest {
  schemaVersion: number
  seedVersion: string
  generatedAt?: string
  plugins: Record<string, SeedPluginEntry>
}

/** 归一化换行后取 sha256（构建/运行时同算法）。 */
export function hashContent(content: string): string {
  return createHash('sha256').update(content.replace(/\r\n?/g, '\n'), 'utf8').digest('hex')
}

/** 由逐插件逐文件 hash 列表推导稳定 seedVersion。 */
export function deriveSeedVersion(plugins: Record<string, SeedPluginEntry>): string {
  const canonical = JSON.stringify(
    Object.keys(plugins).sort().map((name) => [
      name,
      Object.keys(plugins[name].files).sort().map((f) => [f, plugins[name].files[f]]),
    ]),
  )
  return createHash('sha256').update(canonical, 'utf8').digest('hex').slice(0, 16)
}

const HASH_RE = /^[0-9a-f]{64}$/

/** 解析 + 校验 manifest（坏档直接抛错，交调用方 fail-loud）。 */
export function parseSeedManifest(json: string): SeedManifest {
  let raw: unknown
  try {
    raw = JSON.parse(json)
  } catch (error) {
    throw new Error(`seed manifest: invalid JSON: ${String(error)}`)
  }
  const m = raw as Partial<SeedManifest>
  if (m.schemaVersion !== 1) throw new Error(`seed manifest: unsupported schemaVersion ${String(m.schemaVersion)}`)
  if (typeof m.seedVersion !== 'string' || m.seedVersion.length === 0) throw new Error('seed manifest: missing seedVersion')
  if (m.plugins === null || typeof m.plugins !== 'object') throw new Error('seed manifest: missing plugins')
  for (const [name, entry] of Object.entries(m.plugins)) {
    const files = (entry as Partial<SeedPluginEntry> | undefined)?.files
    if (files === null || typeof files !== 'object') throw new Error(`seed manifest: plugin ${name} missing files`)
    for (const [file, hash] of Object.entries(files)) {
      if (typeof hash !== 'string' || !HASH_RE.test(hash)) throw new Error(`seed manifest: bad hash for ${name}/${file}`)
    }
  }
  return m as SeedManifest
}

export type PluginPlan = 'fresh' | 'update' | 'conflict'

function sameFiles(a: Record<string, string>, b: Record<string, string>): boolean {
  const ak = Object.keys(a)
  const bk = Object.keys(b)
  if (ak.length !== bk.length) return false
  return ak.every((k) => a[k] === b[k])
}

/**
 * 分类单个官方插件在用户区的状态（纯函数）：
 * - `fresh`    用户区无该插件 → 整目录播种；
 * - `update`   与上次播种基线逐文件一致（或已等于新版）→ 文件级静默更新；
 * - `conflict` 与基线不一致（改过/删过/无基线）→ 保留用户版，官方新版入 `.incoming`。
 */
export function classifyPlugin(
  userFiles: Record<string, string> | undefined,
  baseline: SeedPluginEntry | undefined,
  seed: SeedPluginEntry,
): PluginPlan {
  if (userFiles === undefined) return 'fresh'
  if (sameFiles(userFiles, seed.files)) return 'update' // 已是新版（如手动合并过）
  if (baseline === undefined) return 'conflict' // 无基线 → 保守按用户所有
  return sameFiles(userFiles, baseline.files) ? 'update' : 'conflict'
}
