// @vitest-environment jsdom
import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
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

function renderPanel(auth: AuthStatus | null) {
  return render(
    <AccountPanel
      auth={auth}
      loggingOut={false}
      logoutError={null}
      onLogout={() => {}}
    />
  )
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
})
