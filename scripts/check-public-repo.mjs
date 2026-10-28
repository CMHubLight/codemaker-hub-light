#!/usr/bin/env node
/**
 * 公开仓库门禁（零依赖，只用 Node 内置模块 + git）。
 *
 * 四项检查：
 *   1. 敏感信息：内网域名、公司名、个人账号、本机路径、逆向过程用语；
 *   2. 路径：逆向证据与旧版文档不得进入版本库；
 *   3. 链接：Markdown 相对链接必须指向版本库内的真实文件；
 *   4. 文档与代码一致：IPC 命令 / 事件 / 设置键 / 数据表 / 代理路由逐项对账。
 *
 * 用法：node scripts/check-public-repo.mjs
 * 退出码：0 通过；1 有失败项。
 */
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const SELF = 'scripts/check-public-repo.mjs'

const failures = []
const fail = (scope, message) => failures.push(`[${scope}] ${message}`)

function git(args) {
  return execFileSync('git', args, { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 << 20 })
}

function trackedFiles() {
  return git(['ls-files', '-z']).split('\0').filter(Boolean)
}

function read(relPath) {
  return readFileSync(join(ROOT, relPath), 'utf8')
}

/** 读取 Markdown 中 ```text name=xxx 形式的清单块。 */
function namedBlocks(markdown) {
  const blocks = new Map()
  const fence = /```[a-zA-Z]*[ \t]+name=([a-z0-9-]+)[ \t]*\r?\n([\s\S]*?)```/g
  let match
  while ((match = fence.exec(markdown)) !== null) {
    const items = match[2]
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter((line) => line && !line.startsWith('#'))
    blocks.set(match[1], items)
  }
  return blocks
}

function compareSet(scope, docLabel, docItems, codeLabel, codeItems) {
  if (!docItems) {
    fail(scope, `${docLabel} 缺少 name= 清单块`)
    return
  }
  const doc = new Set(docItems)
  const code = new Set(codeItems)
  const missing = [...code].filter((item) => !doc.has(item)).sort()
  const extra = [...doc].filter((item) => !code.has(item)).sort()
  if (missing.length) fail(scope, `${docLabel} 缺少代码中的条目：${missing.join(', ')}`)
  if (extra.length) fail(scope, `${docLabel} 多出 ${codeLabel} 中不存在的条目：${extra.join(', ')}`)
}

// ---------------------------------------------------------------- 1. 敏感信息

// 关键字拆开拼接，避免脚本自身被扫到（脚本在扫描时也被排除）。
const words = {
  vendorDomain: ['nie', 'netease', 'com'].join('\\.'),
  vendor: ['net', 'ease'].join(''),
  cnVendor: '\u7f51\u6613',
  businessUnit: '\u4e8b\u4e1a\u7fa4',
  personalAccount: ['cheng', 'ziqian'].join(''),
  personalName: '\u6210\u5b50\u8c26',
  capture: '\u6293\u5305',
  internalCode: 'SIG' + '001'
}

const FORBIDDEN = [
  ['内网域名', new RegExp(words.vendorDomain, 'i')],
  ['公司域名', new RegExp(words.vendor, 'i')],
  ['公司名', new RegExp(words.cnVendor)],
  ['部门名', new RegExp(words.businessUnit)],
  ['个人账号', new RegExp(words.personalAccount, 'i')],
  ['个人姓名', new RegExp(words.personalName)],
  ['本机绝对路径', /C:\\Users\\/i],
  ['逆向过程用语', new RegExp(words.capture)],
  ['内部错误码', new RegExp(words.internalCode)]
]

/** 允许出现关键字的文件：本脚本与 .gitignore 需要用它来阻挡这些字符串。 */
const SCAN_EXEMPT = new Set([SELF, '.gitignore'])

// ---------------------------------------------------------------- 主流程

const files = trackedFiles()
const tracked = new Set(files)

// 1. 敏感信息
for (const relPath of files) {
  if (SCAN_EXEMPT.has(relPath)) continue
  if (relPath.endsWith('.ico') || relPath.endsWith('.png') || relPath.endsWith('.lock')) continue
  let text
  try {
    text = read(relPath)
  } catch {
    continue // 二进制文件按 UTF-8 读不出来，跳过
  }
  const lines = text.split(/\r?\n/)
  for (const [label, pattern] of FORBIDDEN) {
    lines.forEach((line, index) => {
      if (pattern.test(line)) fail('敏感信息', `${relPath}:${index + 1} 命中${label}`)
    })
  }
}

// 2. 禁止入库的路径
const FORBIDDEN_PATHS = [
  [/^research\//, '逆向证据目录'],
  [/^docs\/appendix\//, '旧版文档（附录）'],
  [/^docs\/review\//, '内部评审记录'],
  [/^docs\/design\//, '旧版设计文档'],
  [/^docs\/\d{2}-/, '原版产品功能文档']
]
for (const relPath of files) {
  for (const [pattern, label] of FORBIDDEN_PATHS) {
    if (pattern.test(relPath)) fail('路径', `${relPath} 属于${label}，不应入库`)
  }
}

// 3. 链接检查
const markdownFiles = files.filter((relPath) => relPath.endsWith('.md'))
for (const relPath of markdownFiles) {
  const text = read(relPath)
  const linkPattern = /\[[^\]]*]\(([^)\s]+)(?:\s+"[^"]*")?\)/g
  let match
  while ((match = linkPattern.exec(text)) !== null) {
    const target = match[1]
    if (/^(?:https?:|mailto:|tel:|#)/i.test(target)) continue
    const clean = decodeURI(target.split('#')[0].split('?')[0])
    if (!clean) continue
    const resolved = clean.startsWith('/') ? clean.slice(1) : join(dirname(relPath), clean)
    const normalized = resolved.replaceAll('\\', '/')
    if (!existsSync(join(ROOT, normalized))) {
      fail('链接', `${relPath} 指向不存在的文件：${target}`)
    } else if (!tracked.has(normalized)) {
      fail('链接', `${relPath} 指向未纳入版本库的文件：${target}`)
    }
  }
}

// 4. 文档与代码一致
const doc = (relPath) => (tracked.has(relPath) ? namedBlocks(read(relPath)) : new Map())

const ipcDoc = doc('docs/ipc-api.md')
if (tracked.has('src-tauri/src/lib.rs')) {
  const lib = read('src-tauri/src/lib.rs')
  const handler = lib.slice(lib.indexOf('generate_handler!['), lib.indexOf('])', lib.indexOf('generate_handler![')))
  const commands = [...handler.matchAll(/commands::\w+::(\w+)/g)].map((m) => m[1])
  compareSet('文档/代码', 'docs/ipc-api.md name=ipc-commands', ipcDoc.get('ipc-commands'), 'generate_handler', commands)

  const eventSource = read('src/lib/events.ts')
  const eventBlock = eventSource.slice(eventSource.indexOf('export type AppEventName'), eventSource.indexOf('export interface AppEventPayloads'))
  const events = [...eventBlock.matchAll(/'([a-z]+:\/\/[a-z-]+)'/g)].map((m) => m[1])
  compareSet('文档/代码', 'docs/ipc-api.md name=ipc-events', ipcDoc.get('ipc-events'), 'AppEventName', events)
}

const settingsDoc = doc('docs/settings.md')
if (tracked.has('src-tauri/src/settings.rs')) {
  const settings = read('src-tauri/src/settings.rs')
  const start = settings.indexOf('pub struct Settings {')
  const body = settings.slice(start, settings.indexOf('\n}', start))
  const keys = [...body.matchAll(/^\s*pub (\w+):/gm)].map((m) => m[1])
  compareSet('文档/代码', 'docs/settings.md name=settings-keys', settingsDoc.get('settings-keys'), 'Settings', keys)
}

const modelDoc = doc('docs/data-model.md')
const migrationFiles = files.filter((relPath) => relPath.startsWith('src-tauri/migrations/') && relPath.endsWith('.sql'))
const tables = migrationFiles.flatMap((relPath) => [...read(relPath).matchAll(/CREATE TABLE IF NOT EXISTS (\w+)/gi)].map((m) => m[1]))
compareSet('文档/代码', 'docs/data-model.md name=db-tables', modelDoc.get('db-tables'), 'migrations', tables)

const proxyDoc = doc('docs/proxy.md')
if (tracked.has('src-tauri/src/proxy/routes/mod.rs')) {
  const routes = read('src-tauri/src/proxy/routes/mod.rs')
  const paths = [...routes.matchAll(/\.route\("([^"]+)"/g)].map((m) => m[1])
  compareSet('文档/代码', 'docs/proxy.md name=proxy-routes', proxyDoc.get('proxy-routes'), 'routes/mod.rs', paths)
}

// ---------------------------------------------------------------- 输出

if (failures.length) {
  console.error(`公开仓库门禁未通过：${failures.length} 项\n`)
  for (const item of failures) console.error(`  - ${item}`)
  console.error('\n修复后重新运行：node scripts/check-public-repo.mjs')
  process.exit(1)
}

console.log(`公开仓库门禁通过：${files.length} 个受版本控制的文件，0 项问题。`)
