/**
 * seed 判据层 + 对账 IO 单测（B′ 播种对账）。
 * 运行：pnpm --dir harness test
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync, existsSync, readdirSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  classifyPlugin,
  deriveSeedVersion,
  hashContent,
  parseSeedManifest,
  SEED_STATE_FILE,
} from './seed-manifest'
import { applySeed, hashTree, linkWorkspaceDeps } from './seed'

const silent = (): void => {}

function manifest(plugins: Record<string, Record<string, string>>, version = 'v1') {
  const entries = Object.fromEntries(
    Object.entries(plugins).map(([name, files]) => [name, { files: Object.fromEntries(
      Object.entries(files).map(([f, c]) => [f, hashContent(c)]),
    ) }]),
  )
  return {
    schemaVersion: 1,
    seedVersion: version === 'auto' ? deriveSeedVersion(entries) : version,
    plugins: entries,
  }
}

test('hashContent：换行归一化，CRLF 不误判', () => {
  assert.equal(hashContent('a\r\nb\nc'), hashContent('a\nb\nc'))
  assert.notEqual(hashContent('a'), hashContent('b'))
})

test('deriveSeedVersion：内容同 → 版本同；键序无关', () => {
  const a = manifest({ p1: { 'a.ts': '1', 'b.ts': '2' }, p2: { 'c.ts': '3' } }, 'auto')
  const b = manifest({ p2: { 'c.ts': '3' }, p1: { 'b.ts': '2', 'a.ts': '1' } }, 'auto')
  assert.equal(a.seedVersion, b.seedVersion)
  const c = manifest({ p1: { 'a.ts': 'changed' } }, 'auto')
  assert.notEqual(a.seedVersion, c.seedVersion)
})

test('parseSeedManifest：坏档抛错', () => {
  assert.throws(() => parseSeedManifest('not json'))
  assert.throws(() => parseSeedManifest('{"schemaVersion":2,"seedVersion":"x","plugins":{}}'))
  assert.throws(() => parseSeedManifest('{"schemaVersion":1,"seedVersion":"x","plugins":{"p":{"files":{"a.ts":"zz"}}}}'))
})

test('classifyPlugin：fresh / update / conflict 三分支', () => {
  const seed = { files: { 'a.ts': hashContent('new') } }
  const base = { files: { 'a.ts': hashContent('old') } }
  assert.equal(classifyPlugin(undefined, base, seed), 'fresh')
  assert.equal(classifyPlugin({ 'a.ts': hashContent('old') }, base, seed), 'update')
  assert.equal(classifyPlugin({ 'a.ts': hashContent('new') }, base, seed), 'update') // 手动合并过
  assert.equal(classifyPlugin({ 'a.ts': hashContent('edited') }, base, seed), 'conflict')
  assert.equal(classifyPlugin({ 'a.ts': hashContent('old') }, undefined, seed), 'conflict') // 无基线保守
  assert.equal(classifyPlugin({ 'a.ts': hashContent('old'), 'b.ts': hashContent('x') }, base, seed), 'conflict') // 多了文件
})

// ── 对账 IO（临时目录注入）─────────────────────────────────────────────

function makeSeed(dir: string, files: Record<string, Record<string, string>>): ReturnType<typeof manifest> {
  for (const [name, map] of Object.entries(files)) {
    for (const [file, content] of Object.entries(map)) {
      const dst = join(dir, name, file)
      mkdirSync(join(dst, '..'), { recursive: true })
      writeFileSync(dst, content)
    }
  }
  const m = manifest(files, 'auto')
  writeFileSync(join(dir, 'seed-manifest.json'), JSON.stringify(m, null, 2))
  return m
}

test('applySeed：fresh 播种 + 状态落档 + 二次启动零开销', () => {
  const root = mkdtempSync(join(tmpdir(), 'seed-'))
  const seedDir = join(root, 'seed')
  const work = join(root, 'work')
  const m = makeSeed(seedDir, { memory: { 'package.json': '{}', 'src/index.ts': 'export const x = 1' } })
  const r1 = applySeed({ seedDir, pluginRoot: work, log: silent })
  assert.equal(r1.status, 'applied')
  assert.deepEqual(r1.seeded, ['memory'])
  assert.ok(existsSync(join(work, 'memory', 'src', 'index.ts')))
  assert.ok(existsSync(join(work, SEED_STATE_FILE)))
  const r2 = applySeed({ seedDir, pluginRoot: work, log: silent })
  assert.equal(r2.status, 'up-to-date')
  void m
})

test('applySeed：update 文件级同步——新增/删除/覆盖，node_modules 保留', () => {
  const root = mkdtempSync(join(tmpdir(), 'seed-'))
  const seedDir = join(root, 'seed')
  const work = join(root, 'work')
  makeSeed(seedDir, { memory: { 'a.ts': 'a1', 'b.ts': 'b1' } })
  applySeed({ seedDir, pluginRoot: work, log: silent })
  mkdirSync(join(work, 'memory', 'node_modules'), { recursive: true })
  writeFileSync(join(work, 'memory', 'node_modules', 'keep.js'), 'keep')
  // 升级：a 改、b 删、c 增
  makeSeed(seedDir, { memory: { 'a.ts': 'a2', 'c.ts': 'c1' } })
  const r = applySeed({ seedDir, pluginRoot: work, log: silent })
  assert.deepEqual(r.updated, ['memory'])
  assert.equal(readFileSync(join(work, 'memory', 'a.ts'), 'utf8'), 'a2')
  assert.ok(!existsSync(join(work, 'memory', 'b.ts')))
  assert.ok(existsSync(join(work, 'memory', 'c.ts')))
  assert.ok(existsSync(join(work, 'memory', 'node_modules', 'keep.js')), '用户依赖不被更新清除')
})

test('applySeed：conflict 保留用户版，官方新版入 .incoming/<ver>/<plugin>', () => {
  const root = mkdtempSync(join(tmpdir(), 'seed-'))
  const seedDir = join(root, 'seed')
  const work = join(root, 'work')
  makeSeed(seedDir, { memory: { 'a.ts': 'a1' } })
  applySeed({ seedDir, pluginRoot: work, log: silent })
  writeFileSync(join(work, 'memory', 'a.ts'), 'MY EDIT')
  const m2 = makeSeed(seedDir, { memory: { 'a.ts': 'a2' } })
  const r = applySeed({ seedDir, pluginRoot: work, log: silent })
  assert.deepEqual(r.conflicts, ['memory'])
  assert.equal(readFileSync(join(work, 'memory', 'a.ts'), 'utf8'), 'MY EDIT')
  const incoming = join(work, '.incoming', m2.seedVersion, 'memory')
  assert.equal(readFileSync(join(incoming, 'a.ts'), 'utf8'), 'a2')
})

test('applySeed：用户自写插件不动；无基线的既有目录按 conflict 保守处理', () => {
  const root = mkdtempSync(join(tmpdir(), 'seed-'))
  const seedDir = join(root, 'seed')
  const work = join(root, 'work')
  mkdirSync(join(work, 'my-plugin'), { recursive: true })
  writeFileSync(join(work, 'my-plugin', 'index.ts'), 'mine')
  mkdirSync(join(work, 'memory'), { recursive: true })
  writeFileSync(join(work, 'memory', 'a.ts'), 'preexisting')
  const m = makeSeed(seedDir, { memory: { 'a.ts': 'a1' } })
  const r = applySeed({ seedDir, pluginRoot: work, log: silent })
  assert.equal(readFileSync(join(work, 'my-plugin', 'index.ts'), 'utf8'), 'mine')
  assert.ok(!existsSync(join(work, 'my-plugin', 'a.ts')))
  assert.deepEqual(r.conflicts, ['memory'])
  assert.equal(readFileSync(join(work, 'memory', 'a.ts'), 'utf8'), 'preexisting')
  assert.ok(existsSync(join(work, '.incoming', m.seedVersion, 'memory', 'a.ts')))
})

test('linkWorkspaceDeps：工作区解析链 junction（用户副本可被 @diver 库解析）', () => {
  const root = mkdtempSync(join(tmpdir(), 'seed-'))
  const seedDir = join(root, 'sidecar', 'plugins.seed')
  const work = join(root, 'cos', 'plugins')
  const nmBase = join(root, 'sidecar', 'node_modules')
  makeSeed(seedDir, {
    memory: { 'a.ts': 'a1' },
    'native-bridge': {
      'package.json': '{"name":"@diver/native-bridge","type":"module","main":"index.ts"}',
      'index.ts': 'nb',
    },
  })
  mkdirSync(nmBase, { recursive: true })
  writeFileSync(join(nmBase, 'fake-dep.js'), '// vendor')
  applySeed({ seedDir, pluginRoot: work, log: silent })
  // 1) 父级 node_modules → 内置依赖目录
  assert.equal(realpathSync(join(root, 'cos', 'node_modules')), realpathSync(nmBase))
  // 2) @diver 库 junction → 用户副本（挂载与库导入同一份源码）
  assert.equal(realpathSync(join(work, 'node_modules', '@diver', 'native-bridge')), realpathSync(join(work, 'native-bridge')))
  // 3) 端到端解析：用户插件文件能解析 @diver/* 与内置依赖（模拟真实 Node 解析）
  const req = createRequire(join(work, 'memory', 'src', 'index.ts'))
  assert.equal(req.resolve('@diver/native-bridge'), join(work, 'native-bridge', 'index.ts'))
  assert.equal(req.resolve('fake-dep'), join(nmBase, 'fake-dep.js'))
  // 4) 幂等：重复播种不炸、不重复创建
  const again = linkWorkspaceDeps(work, nmBase)
  assert.deepEqual(again, [])
  // 5) 悬空基座链接重建（卸载后重装到不同路径：旧 junction 指向已消失的安装目录）
  const nmBase2 = join(root, 'sidecar-b', 'node_modules')
  mkdirSync(nmBase2, { recursive: true })
  writeFileSync(join(nmBase2, 'fake-dep-b.js'), '// vendor B')
  rmSync(nmBase, { recursive: true, force: true }) // 旧安装目录消失 → cos/node_modules 悬空
  const rebuilt = linkWorkspaceDeps(work, nmBase2)
  assert.ok(rebuilt.includes(join(root, 'cos', 'node_modules')), '悬空基座链接应被重建')
  // 注：同一进程内 junction 换目标后有路径级 realpath 缓存（tsx/Node），
  // 解析验证一律走新子进程——与生产语义一致（每次启动都是新进程、先建链后解析）
  const resolveFresh = (parent: string, spec: string): string =>
    execFileSync(process.execPath, [
      '-e',
      `process.stdout.write(require('node:module').createRequire(${JSON.stringify(parent)}).resolve(${JSON.stringify(spec)}))`,
    ], { encoding: 'utf8' })
  assert.equal(resolveFresh(join(work, 'memory', 'src', 'index.ts'), 'fake-dep-b'), join(nmBase2, 'fake-dep-b.js'))
  // 6) @diver 链接目标被删后重建（同路径重播种）自动愈合，解析恢复
  rmSync(join(work, 'native-bridge'), { recursive: true, force: true })
  mkdirSync(join(work, 'native-bridge'), { recursive: true })
  writeFileSync(join(work, 'native-bridge', 'index.ts'), 'nb2')
  writeFileSync(join(work, 'native-bridge', 'package.json'), '{"name":"@diver/native-bridge","type":"module","main":"index.ts"}')
  linkWorkspaceDeps(work, nmBase2)
  assert.equal(realpathSync(join(work, 'node_modules', '@diver', 'native-bridge')), realpathSync(join(work, 'native-bridge')))
  assert.equal(resolveFresh(join(work, 'memory', 'src', 'index.ts'), '@diver/native-bridge'), join(work, 'native-bridge', 'index.ts'))
})

test('hashTree：忽略 node_modules / 点目录 / tsbuildinfo', () => {
  const root = mkdtempSync(join(tmpdir(), 'seed-'))
  mkdirSync(join(root, 'node_modules', 'x'), { recursive: true })
  mkdirSync(join(root, '.hidden'), { recursive: true })
  writeFileSync(join(root, 'node_modules', 'x', 'i.js'), 'x')
  writeFileSync(join(root, '.hidden', 'h.ts'), 'x')
  writeFileSync(join(root, 'a.ts'), 'x')
  writeFileSync(join(root, 'b.tsbuildinfo'), 'x')
  assert.deepEqual(Object.keys(hashTree(root) ?? {}), ['a.ts'])
  assert.equal(hashTree(join(root, 'nope')), undefined)
  assert.ok(readdirSync(root).length > 0)
})
