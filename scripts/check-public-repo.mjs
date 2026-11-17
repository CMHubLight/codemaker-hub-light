#!/usr/bin/env node
/**
 * 公开仓库门禁（零依赖，只用 Node 内置模块 + git）。
 *
 * 四项检查：
 *   1. 敏感信息：本机绝对路径、企业/内网域名形状、个人邮箱域名、常见凭据形状、
 *      公司/事业部字样；具体厂商与个人词表不入库，维护者可用
 *      REDACTION_TERMS_FILE 指向一份 gitignored 词表追加；
 *   2. 路径：本地参考材料与旧版文档不得进入版本库；
 *   3. 链接：Markdown 相对链接必须指向版本库内的真实文件；
 *   4. 文档与代码一致：IPC 命令 / 事件 / 设置键 / 数据表 / 代理路由逐项对账。
 *
 * 用法：node scripts/check-public-repo.mjs
 * 退出码：0 通过；1 有失败项。
 */
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = process.env.CHECK_REPO_ROOT
  ? resolve(process.env.CHECK_REPO_ROOT)
  : resolve(dirname(fileURLToPath(import.meta.url)), '..')
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

/** 读取 Markdown 中 ```text name=xxx 形式的清单块；同名重复直接失败。 */
function namedBlocks(markdown) {
  const blocks = new Map()
  const fence = /```[a-zA-Z]*[ \t]+name=([a-z0-9-]+)[ \t]*\r?\n([\s\S]*?)```/g
  let match
  while ((match = fence.exec(markdown)) !== null) {
    if (blocks.has(match[1])) {
      fail('文档/代码', `清单块 name=${match[1]} 重复定义（后一个会静默覆盖前一个）`)
    }
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

// 只保留"形状类"通用规则。具体厂商/个人/内部标识**不写进仓库**：否则为了保护
// 仓库反而把它保护的词公开了。维护者本地可设 REDACTION_TERMS_FILE 指向一份
// gitignored 词表（每行一个词，# 开头为注释）做更严格的扫描；CI 只跑通用规则。
const GENERIC_TERMS = [
  // 盘符 + 常见开发/用户目录（真实本机路径）；不要匹配文档里的 `C:\data` 这类夹具。
  [
    '本机绝对路径',
    /[A-Za-z]:\\(?:Users|Documents|Desktop|Downloads|Code|dev|repo|projects|src)\\/i
  ],
  ['UNC 路径', /\\\\[A-Za-z0-9._-]+\\[A-Za-z0-9$._-]+/],
  ['类 Unix 用户主目录', /\/home\/[a-z_][a-z0-9_-]*\//i],
  ['企业/内网域名', /\.(?:internal|intra|corp)\b/i],
  ['公司/事业部字样', /事业部|事业群/],
  ['个人邮箱域名', /@(?:qq|163|126|gmail|outlook)\.com/i],
  [
    'API 凭据形状',
    /\b(?:sk-[A-Za-z0-9]{20,}|sk-ant-[A-Za-z0-9-]{20,}|sk-proj-[A-Za-z0-9_-]{20,}|(?:ghp|gho|ghs)_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16})\b/
  ],
  ['私钥块', /-----BEGIN [A-Z ]*PRIVATE KEY-----/],
  ['JWT 形状', /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b/]
]

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

/** 读取本地词表（可选，不随仓库分发）。 */
function localTerms() {
  const file = process.env.REDACTION_TERMS_FILE
  if (!file) return []
  if (!existsSync(file)) {
    // 显式指定了词表却读不到：静默放过会让"门禁绿"被误读成"已脱敏"。
    fail('敏感信息', `REDACTION_TERMS_FILE 指向的文件不存在：${file}`)
    return []
  }
  return readFileSync(file, 'utf8')
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith('#'))
}

const FORBIDDEN = [
  ...GENERIC_TERMS,
  ...localTerms().map((term) => ['本地词表', new RegExp(escapeRegExp(term), 'i')])
]

/** 允许出现关键字的文件：本脚本自身与其负向测试（必须写出坏样本）。 */
const SCAN_EXEMPT = new Set([SELF, 'scripts/check-public-repo.test.mjs'])

// ---------------------------------------------------------------- 主流程

const files = trackedFiles()
const tracked = new Set(files)

// 1. 敏感信息
const ARCHIVE_EXTENSIONS = ['.zip', '.7z', '.rar', '.tar', '.gz', '.tgz']
const BINARY_EXTENSIONS = [
  '.ico', '.png', '.jpg', '.jpeg', '.gif', '.webp', '.exe', '.dll', '.so', '.dylib',
  '.woff', '.woff2', '.ttf', '.pdf'
]
const MAX_SCAN_BYTES = 5 * 1024 * 1024
for (const relPath of files) {
  if (SCAN_EXEMPT.has(relPath)) continue
  if (ARCHIVE_EXTENSIONS.some((ext) => relPath.toLowerCase().endsWith(ext))) {
    fail('敏感信息', `${relPath} 是归档文件：必须先解包审查再入库，门禁无法扫描其内容`)
    continue
  }
  if (BINARY_EXTENSIONS.some((ext) => relPath.toLowerCase().endsWith(ext))) continue
  if (statSync(join(ROOT, relPath)).size > MAX_SCAN_BYTES) continue
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
  [/^research\//, '本地参考材料目录'],
  [/^docs\/appendix\//, '旧版文档（附录）'],
  [/^docs\/review\//, '内部评审记录'],
  [/^docs\/design\//, '旧版设计文档'],
  [/^docs\/\d{2}-/, '旧版功能文档']
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
const EXPECTED_SOURCES = [
  'src-tauri/src/lib.rs',
  'src-tauri/src/settings.rs',
  'src-tauri/src/proxy/routes/mod.rs',
  'src/lib/events.ts'
]
for (const relPath of EXPECTED_SOURCES) {
  // 重构移动/重命名这些文件时必须同步更新本脚本：否则对账会静默停用。
  if (!tracked.has(relPath)) {
    fail('文档/代码', `对账源文件缺失：${relPath}（如已重命名，请同步 scripts/check-public-repo.mjs）`)
  }
}

const doc = (relPath) => (tracked.has(relPath) ? namedBlocks(read(relPath)) : new Map())

const ipcDoc = doc('docs/ipc-api.md')
if (tracked.has('src-tauri/src/lib.rs') && tracked.has('src/lib/events.ts')) {
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

// 4b. 定量断言：版本号三处一致、文档里的命令数量与代码一致。
const readJson = (relPath) => JSON.parse(read(relPath))
const versionFiles = [
  ['package.json', () => readJson('package.json').version],
  ['src-tauri/Cargo.toml', () => read('src-tauri/Cargo.toml').match(/^version = "([^"]+)"/m)?.[1]],
  ['src-tauri/tauri.conf.json', () => readJson('src-tauri/tauri.conf.json').version]
]
if (versionFiles.every(([relPath]) => tracked.has(relPath))) {
  const versions = versionFiles.map(([relPath, readVersion]) => [relPath, readVersion()])
  const distinctVersions = new Set(versions.map(([, version]) => version))
  if (distinctVersions.size !== 1 || versions.some(([, version]) => !version)) {
    fail('版本', `版本号不一致：${versions.map(([file, version]) => `${file}=${version ?? '<缺失>'}`).join(', ')}`)
  }
}

if (tracked.has('src-tauri/src/lib.rs')) {
  const lib = read('src-tauri/src/lib.rs')
  const commandCount = (lib.slice(lib.indexOf('generate_handler![')).match(/commands::\w+::\w+/g) ?? []).length
  for (const [relPath, pattern] of [
    ['docs/ipc-api.md', /(\d+) 个 Tauri 命令/],
    ['docs/README.md', /(\d+) 个命令、6 个事件/]
  ]) {
    if (!tracked.has(relPath)) continue
    const match = read(relPath).match(pattern)
    if (!match) {
      fail('版本/计数', `${relPath} 缺少命令数量断言（应写「${commandCount} 个命令」）`)
    } else if (Number(match[1]) !== commandCount) {
      fail('版本/计数', `${relPath} 写 ${match[1]} 个命令，代码实际 ${commandCount} 个`)
    }
  }
}

// ---------------------------------------------------------------- 输出

if (failures.length) {
  console.error(`公开仓库门禁未通过：${failures.length} 项\n`)
  for (const item of failures) console.error(`  - ${item}`)
  console.error('\n修复后重新运行：node scripts/check-public-repo.mjs')
  process.exit(1)
}

console.log(`公开仓库门禁通过：${files.length} 个受版本控制的文件，0 项问题。`)
