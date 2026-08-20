/**
 * @cos/scope — per-agent scoped-registration primitive, ported from
 * dsh-scope (packages/core/scope): mint a context tagged with an opaque key,
 * then route events through a carrier whose filter admits untagged (global)
 * listeners and tagged listeners whose key matches the carrier key or an
 * ancestor of it — events flow up the scope chain, never down.
 * @module @cos/scope
 */

import { Context } from 'cordis'
import type { Context as ContextType } from 'cordis'

export type ScopeKey = object

const kScope = Symbol('cos.scope')

const scopeParents = new WeakMap<ScopeKey, ScopeKey>()

const carrierKeys = new WeakMap<object, ScopeKey | undefined>()

declare const ScopedBrand: unique symbol

/** A routing-only event receiver; the subject travels in the payload. */
export type Scoped<T extends object> = object & { readonly [ScopedBrand]: T }

/** Bind `parent` as `key`'s enclosing scope with a cycle check. */
export function bindScopeParent(key: ScopeKey, parent: ScopeKey): void {
  for (let cursor: ScopeKey | undefined = parent; cursor !== undefined; cursor = scopeParents.get(cursor)) {
    if (cursor === key) throw new Error('cos/scope: scope parent link would form a cycle')
  }
  scopeParents.set(key, parent)
}

/** The chain from a key to its root ancestor, nearest-first. */
export function scopeChainOf(key: ScopeKey | undefined): ScopeKey[] {
  const chain: ScopeKey[] = []
  for (let cursor = key; cursor !== undefined; cursor = scopeParents.get(cursor)) chain.push(cursor)
  return chain
}

export interface Scope {
  /** Context through which scope-owned registrations are made. */
  ctx: ContextType
  /** Dispose every scope-owned registration; racing calls await the same completion. */
  dispose(): Promise<void>
}

/** Shared no-op plugin used as the backing scope fiber. */
function scope(): void {}

/**
 * Mint a scope under `ctx`: the scoped context inherits the dependency API and
 * carries the `kScope` tag used for event routing.
 */
export function createScope(ctx: ContextType, key: ScopeKey): Scope {
  const fiber = ctx.plugin(scope)
  const scoped: ContextType = fiber.ctx.extend({ [kScope]: key })
  let disposing: Promise<void> | undefined
  return {
    ctx: scoped,
    dispose: () => (disposing ??= Promise.resolve(fiber.dispose())),
  }
}

/** Read the nearest scope tag inherited by a context. */
export function scopeOf(ctx: ContextType): ScopeKey | undefined {
  return (ctx as ContextType & { [kScope]?: ScopeKey })[kScope]
}

/**
 * Build an opaque receiver that preserves the base filter, admits untagged
 * listeners globally, and admits tagged listeners matching `key` or any of
 * its ancestors.
 */
export function scopeTarget<T extends object>(base: T, key: ScopeKey | undefined): Scoped<T> {
  const baseFilter = (base as { [Context.filter]?: (ctx: ContextType) => boolean })[Context.filter]
  const carrier = {
    [Context.filter](ctx: ContextType): boolean {
      if (baseFilter !== undefined && !baseFilter.call(base, ctx)) return false
      const tag = scopeOf(ctx)
      if (tag === undefined) return true
      for (let cursor = key; cursor !== undefined; cursor = scopeParents.get(cursor)) {
        if (cursor === tag) return true
      }
      return false
    },
  }
  carrierKeys.set(carrier, key)
  return carrier as unknown as Scoped<T>
}

/** Read a carrier's routing key. */
export function carrierKeyOf(value: unknown): ScopeKey | undefined {
  if (typeof value !== 'object' || value === null) return undefined
  return carrierKeys.get(value)
}