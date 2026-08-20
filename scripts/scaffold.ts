/**
 * Scaffold generator: freeze the cos harness conventions into a reusable
 * template. Copies this repository's plugin tree and config conventions into a
 * new directory, rewrites the `@cos` scope to the caller's scope, and prints
 * next steps. Usage:
 *   pnpm scaffold <targetDir> [--scope <scope>] [--name <name>]
 * Example:
 *   pnpm scaffold ../my-project --scope myco
 * @module cos/scripts/scaffold
 */

import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { join, resolve, basename, relative, dirname } from 'node:path'

const repo = resolve(import.meta.dirname, '..')

interface ScaffoldOptions {
  target: string
  scope: string
  name: string
}

function parseArgs(argv: string[]): ScaffoldOptions {
  if (argv.length < 3) {
    console.error('usage: tsx scripts/scaffold.ts <targetDir> [--scope <scope>] [--name <name>]')
    process.exit(1)
  }
  const target = resolve(process.cwd(), argv[2])
  const scope = argv.includes('--scope') ? String(argv[argv.indexOf('--scope') + 1]) : 'cos'
  const name = argv.includes('--name') ? String(argv[argv.indexOf('--name') + 1]) : basename(target)
  return { target, scope, name }
}

/** Recursively list files under a directory, skipping gitignored/private artifacts. */
function listFiles(root: string): string[] {
  const out: string[] = []
  const walk = (dir: string): void => {
    for (const entry of readdirSync(dir)) {
      if (entry === 'node_modules' || entry === '.sessions' || entry === '.git' || entry === 'dist' || entry === 'lib') continue
      const full = join(dir, entry)
      if (statSync(full).isDirectory()) walk(full)
      else out.push(full)
    }
  }
  walk(root)
  return out
}

function copyTree(sourceRoot: string, targetRoot: string, rewrite: (text: string) => string): string[] {
  const written: string[] = []
  const sources = listFiles(sourceRoot)
  for (const source of sources) {
    const rel = relative(sourceRoot, source)
    const dest = join(targetRoot, rel)
    const text = readFileSync(source, 'utf8')
    // Enforce exactly one trailing newline; apply scope/name rewriting on text files.
    const content = rewrite(text.endsWith('\n') ? text : `${text}\n`)
    mkdirSync(dirname(dest), { recursive: true })
    writeFileSync(dest, content)
    written.push(join(rel, ''))
  }
  return written
}

function main(): void {
  const { target, scope, name } = parseArgs(process.argv)
  console.log(`scaffolding cos template -> ${target} (scope @${scope}, name "${name}")`)

  const templateFiles = ['packages', 'overlays', 'scripts', 'docs', 'main.ts', 'tsconfig.json', '.gitignore', 'cordis.yml', 'cordis.patch.yml', 'pnpm-workspace.yaml']
  mkdirSync(target, { recursive: true })
  const rewrite = (text: string): string => text
    .replace(/@cos\//g, `@${scope}/`)
    .replace(/@cos\b/g, `@${scope}`)
    .replace(/e\.\/(packages\/)/g, `./$1`)

  const written: string[] = []
  for (const item of templateFiles) {
    const src = join(repo, item)
    const dest = join(target, item)
    if (!existsSync(src)) throw new Error(`template item missing: ${src}`)
    if (statSync(src).isDirectory()) {
      mkdirSync(dest, { recursive: true })
      written.push(...copyTree(src, dest, rewrite))
    } else {
      const text = readFileSync(src, 'utf8')
      writeFileSync(dest, rewrite(text))
      written.push(item)
    }
  }
  // Regenerate the root package.json so env-specific deps (external plugins,
  // absolute paths) never leak into a fresh project.
  const packageJson = {
    name,
    private: true,
    type: 'module',
    scripts: {
      dev: 'tsx --expose-internals main.ts',
      typecheck: 'tsc --noEmit',
      start: 'node --expose-internals main.ts',
      scaffold: 'tsx scripts/scaffold.ts',
    },
    dependencies: {
      '@cordisjs/plugin-hmr': '^1.0.15',
      '@cordisjs/plugin-include': '1.0.4',
      '@cordisjs/plugin-loader': '1.0.0-rc.5',
      '@cordisjs/plugin-timer': '1.1.2',
      [`@${scope}/agent-loop`]: 'workspace:*',
      [`@${scope}/agents`]: 'workspace:*',
      [`@${scope}/boot`]: 'workspace:*',
      [`@${scope}/llm`]: 'workspace:*',
      [`@${scope}/llm-deepseek`]: 'workspace:*',
      [`@${scope}/mock-llm`]: 'workspace:*',
      [`@${scope}/persistence`]: 'workspace:*',
      [`@${scope}/persona`]: 'workspace:*',
      [`@${scope}/scope`]: 'workspace:*',
      [`@${scope}/session`]: 'workspace:*',
      [`@${scope}/sidecar`]: 'workspace:*',
      [`@${scope}/system-prompt`]: 'workspace:*',
      [`@${scope}/tools`]: 'workspace:*',
      [`@${scope}/types`]: 'workspace:*',
      '@deepseek-ai/cordis': 'npm:cordis@4.0.0-rc.7',
      cordis: '4.0.0-rc.7',
      yaml: '2.4.0',
    },
    devDependencies: {
      '@types/node': '^26.2.0',
      tsx: '^4.23.12',
      typescript: '^7.0.2',
    },
  }
  writeFileSync(join(target, 'package.json'), `${JSON.stringify(packageJson, null, 2)}\n`)

  // Regenerate a clean user patch layer: the source repo's cordis.patch.yml
  // references a project-specific external plugin, so a fresh project would
  // fail to resolve it. The scaffold ships an empty user-layer placeholder.
  writeFileSync(join(target, 'cordis.patch.yml'), `${[
    '# User patch layer — the LAST layer applied on top of base rows and every',
    '# overlay. Mount your own plugins here by bare package name; a plugin',
    '# written for the DSH ecosystem imports @deepseek-ai/cordis and needs no',
    '# rewrite.',
    '# (empty by default; add rows or `- insert:` blocks here)',
    '',
  ].join('\n')}`)

  console.log(`scaffolded ${written.length} files`)
  console.log('next:')
  console.log(`  cd ${target}`)
  console.log('  pnpm install')
  console.log('  pnpm dev                  # boots the tree (mock provider)')
  console.log('  pnpm start --prompt "hi"  # drive one agent from the CLI')
  console.log('  # or run the sidecar server:', `${target}/packages/sidecar/src/server.ts`)
  console.log(`  set COS_OVERLAYS=overlays/real.yml  # switch to DeepSeek (DEEPSEEK_API_KEY in .env)`)
  console.log('edit cordis.patch.yml to add your own plugins on the user layer.')
}

main()