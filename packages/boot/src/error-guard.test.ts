/**
 * error-guard 单测（决策 6 分级兜底）。
 * 运行：pnpm --dir harness test（node --import tsx --test packages/boot/src/）
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  classifyErrorOrigin,
  createBreaker,
  createGuardHandler,
  DEFAULT_CORE_PLUGINS,
} from './error-guard'

const CORE = new Set(DEFAULT_CORE_PLUGINS)

function errWithStack(stack: string): Error {
  const e = new Error('boom')
  e.stack = stack
  return e
}

test('classify：非核心插件帧 → plugin', () => {
  const e = errWithStack('Error: boom\n    at apply (E:\\app\\resources\\sidecar\\plugins\\memory\\src\\index.ts:10:5)')
  assert.deepEqual(classifyErrorOrigin(e, CORE), { kind: 'plugin', name: '@diver/memory' })
})

test('classify：dev 布局 cos-plugins/<name> 同样命中', () => {
  const e = errWithStack('Error: boom\n    at apply (E:\\repo\\cos-plugins\\web-tools\\src\\search.ts:3:1)')
  assert.deepEqual(classifyErrorOrigin(e, CORE), { kind: 'plugin', name: '@diver/web-tools' })
})

test('classify：核心插件帧 → core panic 面', () => {
  const e = errWithStack('Error: boom\n    at serve (E:\\app\\plugins\\backend\\src\\server.ts:20:3)')
  assert.deepEqual(classifyErrorOrigin(e, CORE), { kind: 'core', where: 'core-plugin', name: '@diver/backend' })
})

test('classify：harness 帧 → engine', () => {
  const e = errWithStack('Error: boom\n    at boot (E:\\app\\harness\\packages\\boot\\src\\index.ts:1:1)')
  assert.deepEqual(classifyErrorOrigin(e, CORE), { kind: 'core', where: 'engine' })
})

test('classify：vendor/node: 帧跳过，落到更深的插件帧', () => {
  const e = errWithStack([
    'Error: boom',
    '    at parse (E:\\app\\sidecar\\node_modules\\yaml\\.e5.js:7:9)',
    '    at node:internal/process/task_queues:90:21',
    '    at tick (E:\\app\\plugins\\self-prompt\\src\\skill.ts:44:7)',
  ].join('\n'))
  assert.deepEqual(classifyErrorOrigin(e, CORE), { kind: 'plugin', name: '@diver/self-prompt' })
})

test('classify：无栈 / 未知帧 → core（安全侧 panic）', () => {
  assert.deepEqual(classifyErrorOrigin('plain string', CORE), { kind: 'core', where: 'unknown' })
  const e = errWithStack('Error: boom\n    at somewhere (/var/lib/whatever/x.js:1:1)')
  assert.deepEqual(classifyErrorOrigin(e, CORE), { kind: 'core', where: 'unknown' })
})

test('classify：cause 链可归属', () => {
  const inner = errWithStack('Error: inner\n    at apply (E:\\app\\plugins\\mcp\\src\\index.ts:8:3)')
  const outer = new Error('wrapper', { cause: inner })
  assert.deepEqual(classifyErrorOrigin(outer, CORE), { kind: 'plugin', name: '@diver/mcp' })
})

test('breaker：窗口内到限熔断，窗口滑动后不误熔', () => {
  let t = 0
  const b = createBreaker(3, 1000, () => t)
  assert.equal(b.record('a').tripped, false)
  t = 100
  assert.equal(b.record('a').tripped, false)
  t = 200
  assert.equal(b.record('a').tripped, true)
  assert.equal(b.has('a'), true)
  // 已熔断的插件在新窗口继续计数不影响判定
  t = 5000
  const again = b.record('a')
  assert.equal(again.count, 1)
  assert.equal(again.tripped, false)
})

test('handler：非核心插件错误隔离并熔断，exit 不被调用', () => {
  const logs: string[] = []
  let exited = 0
  let disposed = 0
  const handler = createGuardHandler({
    corePlugins: DEFAULT_CORE_PLUGINS,
    breakerLimit: 2,
    breakerWindowMs: 60_000,
    log: (_level, message) => logs.push(message),
    exit: (code) => { exited = code },
    ctx: {
      loader: {
        entries: () => [{ options: { name: '@diver/memory' }, fiber: { dispose: () => { disposed += 1 } } }],
      },
    },
  })
  const mk = (): Error => errWithStack('Error: boom\n    at apply (E:\\app\\plugins\\memory\\src\\index.ts:10:5)')
  handler(mk(), 'unhandledRejection')
  handler(mk(), 'unhandledRejection')
  assert.equal(exited, 0)
  assert.equal(disposed, 1)
  assert.ok(logs.some((l) => l.includes('熔断 @diver/memory')))
  // 熔断后的错误被吞掉
  const before = logs.length
  handler(mk(), 'unhandledRejection')
  assert.equal(logs.length, before)
})

test('handler：核心插件错误立即 panic', () => {
  let exited = 0
  const logs: string[] = []
  const handler = createGuardHandler({
    corePlugins: DEFAULT_CORE_PLUGINS,
    log: (_level, message) => logs.push(message),
    exit: (code) => { exited = code },
  })
  handler(errWithStack('Error: boom\n    at serve (E:\\app\\plugins\\backend\\src\\server.ts:1:1)'), 'uncaughtException')
  assert.equal(exited, 1)
  assert.ok(logs.some((l) => l.includes('FATAL')))
})
