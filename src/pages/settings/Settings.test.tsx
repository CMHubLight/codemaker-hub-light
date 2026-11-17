// @vitest-environment jsdom
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { Settings } from './index'
import { invokeCmd } from '../../lib/ipc'
import type { AppSettings } from '../../lib/types'
import { APP_INFO, PROXY, SETTINGS } from '../../test/fixtures'

vi.mock('../../lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/ipc')>()
  return { ...actual, isTauri: () => true, invokeCmd: vi.fn() }
})

const invokeMock = vi.mocked(invokeCmd)

function settingsPatchOf(args: unknown): Partial<AppSettings> {
  return (args as { settings?: Partial<AppSettings> } | undefined)?.settings ?? {}
}

describe('Settings 数字输入', () => {
  it('number_input_accepts_empty_then_validates：可清空，提交时才校验；非法值不保存', async () => {
    const user = userEvent.setup()
    invokeMock.mockImplementation(async (command, args) => {
      switch (command) {
        case 'settings_get':
          return SETTINGS
        case 'proxy_status':
          return PROXY
        case 'app_info':
          return APP_INFO
        case 'settings_update':
          return { ...SETTINGS, ...settingsPatchOf(args) }
        default:
          throw new Error(`未 mock 的命令：${String(command)}`)
      }
    })

    render(<Settings auth={null} onLogout={vi.fn()} />)
    const portInput = await screen.findByLabelText(/代理端口/)
    expect(portInput).toHaveValue(15721)

    await user.clear(portInput)
    // 关键：清空后保持空串，不能被 Number.parseInt('') 立刻回填成 0。
    expect(portInput).toHaveValue(null)

    await user.click(screen.getByRole('button', { name: '保存设置' }))
    expect(await screen.findByText('代理端口需为 1024~65535 之间的整数。')).toBeInTheDocument()
    expect(invokeMock.mock.calls.some(([command]) => command === 'settings_update')).toBe(false)

    await user.type(portInput, '15721')
    await user.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith(
        'settings_update',
        expect.objectContaining({ settings: expect.objectContaining({ proxy_port: 15721 }) })
      )
    )
  })
})

describe('Settings 保存与端口回退', () => {
  it('save_failure_shows_backend_error_and_keeps_edits', async () => {
    const user = userEvent.setup()
    invokeMock.mockImplementation(async (command) => {
      switch (command) {
        case 'settings_get':
          return SETTINGS
        case 'proxy_status':
          return PROXY
        case 'app_info':
          return APP_INFO
        case 'settings_update':
          throw new Error('写入设置文件失败：磁盘已满')
        default:
          throw new Error(`未 mock 的命令：${String(command)}`)
      }
    })

    render(<Settings auth={null} onLogout={vi.fn()} />)
    const gatewayInput = await screen.findByLabelText(/网关地址/)
    await user.clear(gatewayInput)
    await user.type(gatewayInput, 'https://gateway.example.test')
    await user.click(screen.getByRole('button', { name: '保存设置' }))

    // 失败必须如实显示后端原因，且不能给出“已保存/请重启”这类成功提示。
    expect(await screen.findByText('写入设置文件失败：磁盘已满')).toBeInTheDocument()
    expect(screen.queryByText(/设置已保存/)).toBeNull()
    // 编辑内容保留，用户可以直接改完再存。
    expect(gatewayInput).toHaveValue('https://gateway.example.test')
    expect(screen.getByRole('button', { name: '撤销修改' })).toBeEnabled()
  })

  it('saved_port_change_shows_restart_hint_without_fake_fallback', async () => {
    const user = userEvent.setup()
    invokeMock.mockImplementation(async (command, args) => {
      switch (command) {
        case 'settings_get':
          return SETTINGS
        // 保存后代理仍在旧端口上运行：重启前这是预期状态，不是端口回退。
        case 'proxy_status':
          return PROXY
        case 'app_info':
          return APP_INFO
        case 'settings_update':
          return { ...SETTINGS, ...settingsPatchOf(args) }
        // 真正的回退：重启后仍拿不到 16000，落到 15722。
        case 'proxy_restart':
          return {
            ...PROXY,
            port: 15722,
            base_url: 'http://127.0.0.1:15722',
            health_url: 'http://127.0.0.1:15722/health'
          }
        default:
          throw new Error(`未 mock 的命令：${String(command)}`)
      }
    })

    render(<Settings auth={null} onLogout={vi.fn()} />)
    const portInput = await screen.findByLabelText(/代理端口/)
    await user.clear(portInput)
    await user.type(portInput, '16000')
    await user.click(screen.getByRole('button', { name: '保存设置' }))

    expect(
      await screen.findByText('设置已保存。端口或网关已变更，请重启代理后生效。')
    ).toBeInTheDocument()
    // N-P2-3：保存后、重启前不能出现“配置端口 16000 不可用”的假回退警告。
    expect(screen.queryByText(/端口回退/)).toBeNull()

    // 重启后后端确实回退到 15722：此时才应该提示回退。
    await user.click(screen.getByRole('button', { name: '重启代理' }))
    expect(
      await screen.findByText(/端口回退：配置端口 16000 不可用，代理实际监听 15722/)
    ).toBeInTheDocument()
  })

  it('dirty_state_is_reported_to_shell_for_leave_confirmation', async () => {
    const user = userEvent.setup()
    const onDirtyChange = vi.fn()
    invokeMock.mockImplementation(async (command) => {
      switch (command) {
        case 'settings_get':
          return SETTINGS
        case 'proxy_status':
          return PROXY
        case 'app_info':
          return APP_INFO
        default:
          throw new Error(`未 mock 的命令：${String(command)}`)
      }
    })

    render(<Settings auth={null} onLogout={vi.fn()} onDirtyChange={onDirtyChange} />)
    const gatewayInput = await screen.findByLabelText(/网关地址/)
    // 初始加载不算脏态。
    await waitFor(() => expect(onDirtyChange).toHaveBeenLastCalledWith(false))

    await user.type(gatewayInput, '/v1')
    await waitFor(() => expect(onDirtyChange).toHaveBeenLastCalledWith(true))

    await user.click(screen.getByRole('button', { name: '撤销修改' }))
    await waitFor(() => expect(onDirtyChange).toHaveBeenLastCalledWith(false))
  })
})
