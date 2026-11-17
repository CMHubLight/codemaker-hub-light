// 公开仓库门禁的负向测试：坏清单/坏路径/凭据形状/归档/缺失词表必须被抓到。
// 每个用例在独立临时 git 仓库里跑，避免污染真实工作树。
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync, spawnSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const SCRIPT = resolve(HERE, 'check-public-repo.mjs')
const REPO_ROOT = resolve(HERE, '..')

function makeRepo(files) {
  const dir = mkdtempSync(join(tmpdir(), 'chl-gate-'))
  execFileSync('git', ['init', '-q'], { cwd: dir })
  for (const [relPath, content] of Object.entries(files)) {
    const full = join(dir, relPath)
    mkdirSync(dirname(full), { recursive: true })
    writeFileSync(full, content)
  }
  execFileSync('git', ['add', '.'], { cwd: dir })
  return dir
}

function runGate(root, env = {}) {
  const result = spawnSync(process.execPath, [SCRIPT], {
    cwd: root,
    env: { ...process.env, CHECK_REPO_ROOT: root, ...env },
    encoding: 'utf8'
  })
  return { code: result.status, output: `${result.stdout}${result.stderr}` }
}

function withRepo(files, fn) {
  const dir = makeRepo(files)
  try {
    fn(dir)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
}

test('真实仓库当前通过', () => {
  const result = runGate(REPO_ROOT)
  assert.equal(result.code, 0, result.output)
})

test('抓本机绝对路径（D:\\Code 形式）', () => {
  withRepo(
    { 'README.md': '构建目录：D:\\Code\\codemaker-hub-light\n' },
    (dir) => {
      const result = runGate(dir)
      assert.equal(result.code, 1)
      assert.match(result.output, /本机绝对路径/)
    }
  )
})

test('抓带连字符的 API 凭据形状', () => {
  withRepo({ 'notes.md': 'token: sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345\n' }, (dir) => {
    const result = runGate(dir)
    assert.equal(result.code, 1)
    assert.match(result.output, /API 凭据形状/)
  })
})

test('同名清单块重复必须失败', () => {
  const block = '```text name=ipc-commands\na\n```\n'
  withRepo({ 'docs/ipc-api.md': `# IPC\n${block}${block}` }, (dir) => {
    const result = runGate(dir)
    assert.equal(result.code, 1)
    assert.match(result.output, /重复定义/)
  })
})

test('归档文件必须解包后入库', () => {
  withRepo({ 'artifact.zip': 'PK not really a zip' }, (dir) => {
    const result = runGate(dir)
    assert.equal(result.code, 1)
    assert.match(result.output, /归档文件/)
  })
})

test('显式指定但不存在的脱敏词表必须失败', () => {
  withRepo({ 'README.md': 'hello\n' }, (dir) => {
    const result = runGate(dir, { REDACTION_TERMS_FILE: join(dir, 'missing-terms.txt') })
    assert.equal(result.code, 1)
    assert.match(result.output, /REDACTION_TERMS_FILE/)
  })
})
