/**
 * StandardWriter 落盘 id 单测：持久化 id 是随机 UUID，与内存 `event.seq`
 * 完全解耦——旧实现拿 seq 的 hex 当 id，resume 后 seq 只按已落盘消息条数
 * 重新起算，跨重启会把旧 id 再发一遍（历史里互相挤掉）。
 * 运行：pnpm --dir harness test。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import type { SessionEvent } from '@cos/types'

import { StandardWriter } from './codec.ts'

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

function userEvent(seq: number, text = 'hi'): SessionEvent {
  return {
    type: 'user/message',
    seq,
    time: seq + 1,
    data: {
      id: `u${seq}`,
      role: 'user',
      content: [{ type: 'text', text }],
      source: { kind: 'human' },
    },
  } as unknown as SessionEvent
}

function parse(line: string): { id: string; parentId: string | null } {
  return JSON.parse(line) as { id: string; parentId: string | null }
}

test('落盘 id 是随机 UUID，不取内存 seq', () => {
  const writer = new StandardWriter(null)
  const [first] = writer.encodeBatch([userEvent(0)])
  assert.ok(first !== undefined)
  const parsed = parse(first)
  assert.match(parsed.id, UUID_RE)
  assert.notEqual(parsed.id, '00000000', '不得回退成 seq hex')
  assert.equal(parsed.parentId, null)
})

test('同批与跨批 id 均不重复', () => {
  const writer = new StandardWriter(null)
  const ids = [...writer.encodeBatch([userEvent(0), userEvent(1), userEvent(2)])]
    .concat(writer.encodeBatch([userEvent(3)]))
    .map((line) => parse(line).id)
  assert.equal(new Set(ids).size, ids.length)
})

test('resume：parentId 指向段尾 id（原样透传，不再解析数值续号）', () => {
  const writer = new StandardWriter('00000030')
  const [first] = writer.encodeBatch([userEvent(0)])
  assert.ok(first !== undefined)
  const parsed = parse(first)
  assert.match(parsed.id, UUID_RE)
  assert.equal(parsed.parentId, '00000030')
})

test('resume 两次（同段尾）：分配的 id 互不相同，不依赖段尾数值', () => {
  const a = parse(new StandardWriter('00000030').encodeBatch([userEvent(0)])[0]!)
  const b = parse(new StandardWriter('00000030').encodeBatch([userEvent(0)])[0]!)
  assert.match(a.id, UUID_RE)
  assert.match(b.id, UUID_RE)
  assert.notEqual(a.id, b.id)
})
