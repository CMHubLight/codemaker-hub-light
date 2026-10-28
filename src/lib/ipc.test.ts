import { describe, expect, it } from 'vitest'
import {
  CommandTimeoutError,
  DEFAULT_COMMAND_TIMEOUT_MS,
  commandTimeoutMs,
  describeError,
  withTimeout
} from './ipc'

describe('withTimeout', () => {
  it('超时后以 CommandTimeoutError 拒绝，并带上命令名与秒数', async () => {
    const never = new Promise<never>(() => {})
    const started = Date.now()

    const error: unknown = await withTimeout(never, 20, 'proxy_restart').catch(
      (cause: unknown) => cause
    )

    expect(error).toBeInstanceOf(CommandTimeoutError)
    expect(error).toMatchObject({ command: 'proxy_restart', timeoutMs: 20 })
    expect((error as CommandTimeoutError).message).toContain('proxy_restart')
    expect((error as CommandTimeoutError).message).toContain('超时')
    expect(Date.now() - started).toBeLessThan(1000)
  })

  it('未超时时原样透传结果', async () => {
    await expect(withTimeout(Promise.resolve(42), 500, 'auth_status')).resolves.toBe(42)
  })

  it('结果晚于超时也不会产生未处理拒绝', async () => {
    const late = new Promise<number>((resolve) => {
      setTimeout(() => resolve(1), 30)
    })

    await expect(withTimeout(late, 5, 'quota_get')).rejects.toBeInstanceOf(CommandTimeoutError)
    // 让"晚到的结果"走完；若竞速留下了未处理拒绝，vitest 会在此用例失败。
    await new Promise((resolve) => setTimeout(resolve, 80))
  })
})

describe('describeError', () => {
  it('把超时与其他错误分开分类', () => {
    expect(describeError(new CommandTimeoutError('quota_get', 30_000)).kind).toBe('timeout')
    expect(describeError('上游返回 HTTP 403').kind).toBe('forbidden')
    expect(describeError('额度接口不存在（HTTP 404）').kind).toBe('not_found')
    expect(describeError('尚未登录，无法获取额度数据').kind).toBe('unauthenticated')
    expect(describeError(new Error('网络请求失败：无法连接网关')).kind).toBe('network')
    expect(describeError('完全未知的失败')).toEqual({
      kind: 'unknown',
      raw: '完全未知的失败'
    })
  })

  it('超时表：慢命令放宽到 30 秒，其余走默认值', () => {
    expect(DEFAULT_COMMAND_TIMEOUT_MS).toBe(10_000)
    expect(commandTimeoutMs('usage_export_csv')).toBe(30_000)
    expect(commandTimeoutMs('app_open_logs')).toBe(15_000)
    expect(commandTimeoutMs('auth_status')).toBe(DEFAULT_COMMAND_TIMEOUT_MS)
  })
})
