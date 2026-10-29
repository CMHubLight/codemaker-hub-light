// @vitest-environment jsdom
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { AccountPanel } from './AccountPanel'
import type { AuthStatus } from '../../lib/types'
import { AUTH_UNAUTHENTICATED } from '../../test/fixtures'

/** 已登录态：`user` 是 SSO 账号（不是工号）。 */
function authenticated(overrides: Partial<AuthStatus> = {}): AuthStatus {
  return {
    ...AUTH_UNAUTHENTICATED,
    state: 'authenticated',
    user: 'test-user',
    issued_at: 1793083891,
    expires_at: 1793170291,
    credentials_valid: true,
    last_validated_at: '2026-10-27T06:51:31Z',
    dept: '测试部门',
    server_url: 'https://api.example.test',
    ...overrides
  }
}

function renderPanel(auth: AuthStatus | null, overrides: Partial<Parameters<typeof AccountPanel>[0]> = {}) {
  const props = {
    auth,
    loggingOut: false,
    logoutError: null,
    retryingRenewal: false,
    renewalMessage: null,
    onRetryRenewal: vi.fn(),
    onLogout: vi.fn(),
    ...overrides
  }
  return { ...render(<AccountPanel {...props} />), props }
}

describe('AccountPanel', () => {
  it('account_row_shows_sso_user_when_display_name_missing', () => {
    renderPanel(authenticated())

    expect(screen.getByText('账号').parentElement?.textContent).toContain('test-user')
    // 工号没有服务端来源，界面不再展示该字段。
    expect(screen.queryByText('工号')).toBeNull()
  })

  it('display_name_is_preferred_for_account_label', () => {
    renderPanel(authenticated({ display_name: '测试用户' }))

    const accountRow = screen.getByText('账号').parentElement
    expect(accountRow?.textContent).toContain('测试用户')
    expect(accountRow?.textContent).not.toContain('test-user')
  })

  it('unauthenticated_shows_placeholder', () => {
    renderPanel(null)

    expect(screen.getByText('当前没有可展示的账号信息，请重新登录。')).toBeInTheDocument()
    expect(screen.queryByText('账号')).toBeNull()
  })

  it('logout_requires_confirmation_before_calling_backend', async () => {
    const user = userEvent.setup()
    const { props } = renderPanel(authenticated())

    await user.click(screen.getByRole('button', { name: '退出登录' }))
    expect(props.onLogout).not.toHaveBeenCalled()
    expect(screen.getByText(/确认退出登录/)).toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: '取消' }))
    expect(screen.getByRole('button', { name: '退出登录' })).toBeInTheDocument()
    expect(props.onLogout).not.toHaveBeenCalled()

    await user.click(screen.getByRole('button', { name: '退出登录' }))
    await user.click(screen.getByRole('button', { name: '确认退出' }))
    expect(props.onLogout).toHaveBeenCalledTimes(1)
  })

  it('retry_renewal_button_calls_backend_and_shows_result', async () => {
    const user = userEvent.setup()
    const { props } = renderPanel(authenticated())

    await user.click(screen.getByRole('button', { name: '重新检测 / 续期' }))
    expect(props.onRetryRenewal).toHaveBeenCalledTimes(1)

    // 结果提示由上层传入；错误态需可被读屏识别。
    renderPanel(authenticated(), {
      renewalMessage: { tone: 'error', text: '重新检测失败：网络错误' }
    })
    expect(screen.getByRole('alert').textContent).toContain('重新检测失败')
  })
})
