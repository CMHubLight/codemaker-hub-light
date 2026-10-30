#!/usr/bin/env node
/**
 * 构建前占用检查：发布版实例正在运行时，Windows 会锁住
 * `src-tauri/target/release/codemaker-hub-light.exe`，构建会在链接阶段报
 * “拒绝访问”。这里把检查放在 `beforeBuildCommand` 的最前面：命中就立即失败
 * 并提示用托盘「退出」正常关闭，**绝不自动结束进程**。
 *
 * 作用域只认本仓库的 `src-tauri/target/release/` 目录：
 * - dev 实例（`target/debug`）、已安装副本、复制到别处的 exe 不占用本仓库的
 *   release exe，不应误拦；
 * - 构建产物 `bundle/nsis/*setup.exe` 若正在运行同样会锁文件，也一并拦。
 *
 * 退出码：0 = 可以继续构建；1 = 有实例在运行，构建应立即中止。
 * 进程枚举失败时 fail-open（警告后放行）：构建工具故障不应阻断正常发布。
 */
import { execFileSync } from 'node:child_process'
import { realpathSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const RELEASE_DIR = resolve(ROOT, 'src-tauri', 'target', 'release')

const PS_SCRIPT = [
  '[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;',
  'Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath } |',
  'Select-Object ProcessId,Name,ExecutablePath | ConvertTo-Json -Compress'
].join(' ')

/** 列出 Windows 上可执行路径可见的进程；调用失败时抛错（由调用方 fail-open）。 */
function listWindowsProcesses() {
  const output = execFileSync(
    'powershell.exe',
    ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', PS_SCRIPT],
    {
      encoding: 'utf8',
      timeout: 15_000,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
      maxBuffer: 16 << 20
    }
  )
  const text = output.replace(/^\uFEFF/, '').trim()
  if (!text) return []
  const parsed = JSON.parse(text)
  return Array.isArray(parsed) ? parsed : [parsed]
}

/** 归一化路径：统一绝对路径、反斜杠与小写，规避盘符/分隔符大小写差异。 */
function normalizePath(path) {
  return resolve(path).replaceAll('/', '\\').toLowerCase()
}

/**
 * 从进程快照里挑出占用本仓库 release 目录的进程。
 *
 * 纯函数，便于将来的单元测试；不依赖系统状态。
 * @param {Array<{ ProcessId?: number, Name?: string, ExecutablePath?: string }>} processes
 * @param {string} releaseDir
 * @returns {Array<{ pid: number, name: string, path: string }>}
 */
export function findLockingProcesses(processes, releaseDir = RELEASE_DIR) {
  const prefix = `${normalizePath(releaseDir)}\\`
  const releaseExe = normalizePath(join(releaseDir, 'codemaker-hub-light.exe'))
  const matches = []
  for (const proc of processes ?? []) {
    const exePath = typeof proc?.ExecutablePath === 'string' ? proc.ExecutablePath.trim() : ''
    if (!exePath) continue
    const normalized = normalizePath(exePath)
    if (normalized === releaseExe || normalized.startsWith(prefix)) {
      matches.push({
        pid: Number(proc.ProcessId ?? 0),
        name: typeof proc.Name === 'string' && proc.Name ? proc.Name : 'codemaker-hub-light.exe',
        path: exePath
      })
    }
  }
  return matches.sort((left, right) => left.pid - right.pid)
}

/** 构建中止原因：列出占用进程，并给出“正常关闭”的操作指引。 */
export function formatBlockedMessage(processes) {
  const lines = ['[build guard] 检测到发布版实例正在运行，构建会失败（Windows 会锁住 exe）：']
  for (const proc of processes) {
    lines.push(`  - pid ${proc.pid}（${proc.name}）：${proc.path}`)
  }
  lines.push(
    '',
    '请先正常关闭应用：托盘右键 →「退出」（点窗口 X 只是隐藏到托盘，不会退出）。',
    '不要用任务管理器 / taskkill / Stop-Process 强杀：会留下 logs\\session.marker，',
    '下次启动会告警「上次运行未正常退出」，也给崩溃排查制造假现场。',
    '关闭后等 2–3 秒再重试构建。'
  )
  return lines.join('\n')
}

function main() {
  // 产品只支持 Windows 构建；其它平台不上进程检查（CI 的 frontend/docs job 也不经过这里）。
  if (process.platform !== 'win32') return

  let processes
  try {
    processes = listWindowsProcesses()
  } catch (err) {
    console.warn(`[build guard] 进程检查失败，跳过本次检查（不影响构建）：${err.message}`)
    return
  }

  const blockers = findLockingProcesses(processes)
  if (blockers.length === 0) return

  console.error(formatBlockedMessage(blockers))
  process.exitCode = 1
}

/** 判断两个路径是否指向同一文件（Windows 大小写不敏感；解析符号链接）。 */
function sameFile(left, right) {
  let leftPath = left
  let rightPath = right
  try {
    leftPath = realpathSync(left)
    rightPath = realpathSync(right)
  } catch {
    // 任一路径不可解析时退回字符串比较，避免静默跳过检查。
  }
  return process.platform === 'win32'
    ? leftPath.toLowerCase() === rightPath.toLowerCase()
    : leftPath === rightPath
}

const invokedDirectly = Boolean(
  process.argv[1] && sameFile(process.argv[1], fileURLToPath(import.meta.url))
)
if (invokedDirectly) main()
