import { existsSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const root = dirname(fileURLToPath(import.meta.url))
const sharedBuild = resolve(root, '../../Agent2Providers/wasm/build.mjs')
if (!existsSync(sharedBuild)) throw new Error('缺少相邻 Agent2Providers 仓库的 WASM 构建工具与 SDK')
if (!process.argv[2]) throw new Error('请指定 Web 提供商目录，例如 tabbit-intl')
process.env.AGENT2API_WASM_WORKSPACE = root
await import(pathToFileURL(sharedBuild).href)
