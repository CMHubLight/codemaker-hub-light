#!/usr/bin/env node
/**
 * 构建防呆脚本的单元测试：路径判定与提示文案。
 *
 * 只测纯函数，不启动进程、不依赖 Windows：CI 的 ubuntu frontend job 也能跑。
 * 用法：node --test scripts/ensure-app-closed.test.mjs
 */
import assert from 'node:assert/strict'
import { join, resolve } from 'node:path'
import { test } from 'node:test'

import { findLockingProcesses, formatBlockedMessage } from './ensure-app-closed.mjs'

const releaseDir = resolve('tmp-fixture', 'src-tauri', 'target', 'release')

/** 构造一条 Win32_Process 形状的记录。 */
function proc(pid, processPath) {
  return { ProcessId: pid, Name: 'codemaker-hub-light.exe', ExecutablePath: processPath }
}

test('命中本仓库 target/release 下的 exe 与安装包', () => {
  const hits = findLockingProcesses(
    [
      proc(1, join(releaseDir, 'codemaker-hub-light.exe')),
      proc(2, join(releaseDir, 'bundle', 'nsis', 'CodeMaker Hub Light_0.1.1_x64-setup.exe'))
    ],
    releaseDir
  )
  assert.deepEqual(
    hits.map((hit) => hit.pid),
    [1, 2]
  )
})

test('路径大小写不敏感（Windows 盘符/目录大小写差异）', () => {
  const hits = findLockingProcesses(
    [proc(3, join(releaseDir, 'CODEMAKER-HUB-LIGHT.EXE').toUpperCase())],
    releaseDir
  )
  assert.equal(hits.length, 1)
})

test('不误拦 dev 实例、安装副本、相似前缀与空路径', () => {
  const candidates = [
    join(releaseDir, '..', 'debug', 'codemaker-hub-light.exe'),
    resolve('tmp-fixture', 'installed', 'codemaker-hub-light.exe'),
    join(`${releaseDir}2`, 'codemaker-hub-light.exe'),
    '',
    '   '
  ]
  const hits = findLockingProcesses(
    candidates.map((candidate, index) => proc(index + 10, candidate)),
    releaseDir
  )
  assert.deepEqual(hits, [])
})

test('忽略缺失 ExecutablePath 的进程记录', () => {
  const hits = findLockingProcesses(
    [null, { ProcessId: 1 }, { ProcessId: 2, ExecutablePath: '   ' }],
    releaseDir
  )
  assert.deepEqual(hits, [])
})

test('阻断提示包含 pid、路径与托盘退出指引', () => {
  const exePath = join(releaseDir, 'codemaker-hub-light.exe')
  const message = formatBlockedMessage([
    { pid: 42, name: 'codemaker-hub-light.exe', path: exePath }
  ])
  assert.match(message, /pid 42/)
  assert.ok(message.includes(exePath), '提示应给出可执行文件路径')
  assert.match(message, /托盘/)
  assert.match(message, /session\.marker/)
})
