/**
 * Build the sidecar as a Node Single Executable Application. Steps:
 *   1. esbuild bundles packages/sidecar/src/sea.ts (all @cos/* plugins via the
 *      in-process registry) into one ESM file dist/sea.mjs.
 *   2. node --experimental-sea-config produces the SEA blob dist/sea-prep.blob.
 *   3. postject injects the blob into a copy of the running node.exe,
 *      producing dist/cos-sidecar.exe.
 * Run with `pnpm build:sea`. The binary reads cordis.yml / secrets.yml from
 * its working directory at runtime, so no node_modules is required.
 */
import { execFileSync } from 'node:child_process'
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'esbuild'
import { inject } from 'postject'

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const dist = join(root, 'dist')
mkdirSync(dist, { recursive: true })

const OUT = join(dist, 'sea.cjs')
const BLOB = join(dist, 'sea-prep.blob')
const SEA_CONFIG = join(dist, 'sea-config.json')
const EXE = join(dist, 'cos-sidecar.exe')
const FUSE = 'NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2'

// 1) Bundle the whole sidecar graph (plugins bundled via the registry) to a
// single CJS file — the Node SEA parity requires a CJS main. plugin-loader
// reads `import.meta.url` for createRequire, which is undefined in CJS, so
// shim it to the current file URL.
await build({
  entryPoints: [join(root, 'packages/sidecar/src/sea.ts')],
  bundle: true,
  platform: 'node',
  format: 'cjs',
  define: { 'import.meta.url': '"file:///C:/"' },
  outfile: OUT,
  logLevel: 'info',
})

// 2) Generate the SEA blob from the bundled ESM main.
writeFileSync(SEA_CONFIG, JSON.stringify({
  main: OUT,
  output: BLOB,
  disableExperimentalSEAWarning: true,
}, null, 2))
execFileSync(process.execPath, ['--experimental-sea-config', SEA_CONFIG], { cwd: root })

// 3) Inject the blob into a copy of the current node binary (postject API).
copyFileSync(process.execPath, EXE)
await inject(EXE, 'NODE_SEA_BLOB', readFileSync(BLOB), { sentinelFuse: FUSE })

console.log(`\nbuilt ${EXE}`)
