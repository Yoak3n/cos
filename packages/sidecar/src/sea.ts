/**
 * @cos/sidecar/sea — SEA entry for a self-contained single-executable build.
 * Boots with the in-process plugin registry (no node_modules needed at
 * runtime) and disables source watching. syncBuiltinESMExports satisfies the
 * ESM-SEA runtime contract. Bundle to a single ESM file via
 * scripts/build-sea.mjs, then inject as a Node SEA.
 * @module @cos/sidecar/sea
 */

import { syncBuiltinESMExports } from 'node:module'
import { main } from './sidecar'
import { plugins } from './plugins'

syncBuiltinESMExports()
void main({ watchRoots: [], plugins })
