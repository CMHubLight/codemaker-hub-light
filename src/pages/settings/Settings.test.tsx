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
        case 'settings_update': {
          const patch = (args as { settings?: Partial<AppSettings> } | undefined)?.settings ?? {}
          return { ...SETTINGS, ...patch }
        }
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
