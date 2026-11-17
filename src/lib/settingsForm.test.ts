import { describe, expect, it } from 'vitest'
import {
  applyNumberDrafts,
  isSettingsDirty,
  numberDraftError,
  numberDraftsFrom,
  parseNumberDraft,
  settingsPatch,
  validateSettings
} from './settingsForm'
import type { NumberDrafts } from './settingsForm'
import { SETTINGS } from '../test/fixtures'

describe('parseNumberDraft', () => {
  it('accepts_integer_boundaries', () => {
    expect(parseNumberDraft('proxy_port', '1024')).toBe(1024)
    expect(parseNumberDraft('proxy_port', '65535')).toBe(65535)
    expect(parseNumberDraft('retention_days', '1')).toBe(1)
    expect(parseNumberDraft('retention_days', '3650')).toBe(3650)
    // 边界外一位就是非法：不能让 65536 被截断成合法值。
    expect(parseNumberDraft('proxy_port', '65536')).toBeNull()
    expect(parseNumberDraft('proxy_port', '1023')).toBeNull()
  })

  it('rejects_empty_fractional_and_non_numeric_drafts', () => {
    expect(parseNumberDraft('proxy_port', '')).toBeNull()
    expect(parseNumberDraft('proxy_port', '   ')).toBeNull()
    expect(parseNumberDraft('proxy_port', '15721.5')).toBeNull()
    // 旧实现用 Number.parseInt，'15721abc' 会被当成 15721。
    expect(parseNumberDraft('proxy_port', '15721abc')).toBeNull()
    expect(parseNumberDraft('proxy_port', '-1')).toBeNull()
    // 前后空白允许（用户复制粘贴常带空格）。
    expect(parseNumberDraft('proxy_port', ' 15721 ')).toBe(15721)
  })

  it('number_draft_error_explains_the_field_range', () => {
    expect(numberDraftError('proxy_port', '')).toContain('1024~65535')
    expect(numberDraftError('retention_days', '0')).toContain('1~3650')
    expect(numberDraftError('retention_days', '90')).toBeNull()
  })
})

describe('applyNumberDrafts', () => {
  it('merges_valid_drafts_and_rejects_any_invalid_field', () => {
    const drafts: NumberDrafts = { proxy_port: '16000', retention_days: '30' }
    expect(applyNumberDrafts(SETTINGS, drafts)).toMatchObject({
      proxy_port: 16000,
      retention_days: 30
    })

    // 任一字段非法就整体返回 null：不能保存“一半生效”的设置。
    expect(applyNumberDrafts(SETTINGS, { proxy_port: '16000', retention_days: '' })).toBeNull()
  })

  it('number_drafts_from_round_trips_strings', () => {
    expect(numberDraftsFrom(SETTINGS)).toEqual({
      proxy_port: '15721',
      retention_days: '90'
    })
  })
})

describe('validateSettings', () => {
  it('rejects_invalid_gateway_and_retention_ranges', () => {
    expect(validateSettings({ ...SETTINGS, gateway_url: '' })).toContain('不能为空')
    expect(validateSettings({ ...SETTINGS, gateway_url: 'ftp://api.example.test' })).toContain(
      'http'
    )
    expect(validateSettings({ ...SETTINGS, gateway_url: 'api.example.test' })).toContain('不是合法')
    expect(validateSettings({ ...SETTINGS, retention_days: 3651 })).toContain('1~3650')
    expect(validateSettings(SETTINGS)).toBeNull()
  })
})

describe('isSettingsDirty', () => {
  it('compares_only_editable_fields', () => {
    const drafts = numberDraftsFrom(SETTINGS)
    expect(isSettingsDirty({ settings: SETTINGS, drafts }, SETTINGS)).toBe(false)

    // 后端只读字段（例如 credit_to_yuan）变化不算用户未保存修改。
    expect(
      isSettingsDirty(
        { settings: { ...SETTINGS, credit_to_yuan: 0.02 }, drafts },
        SETTINGS
      )
    ).toBe(false)

    // 清空数字输入算修改（即使解析结果是 null），不能因为“还没输入完”就判干净。
    expect(
      isSettingsDirty({ settings: SETTINGS, drafts: { ...drafts, proxy_port: '' } }, SETTINGS)
    ).toBe(true)

    expect(
      isSettingsDirty(
        { settings: { ...SETTINGS, gateway_url: 'https://other.example.test' }, drafts },
        SETTINGS
      )
    ).toBe(true)
  })
})

describe('settingsPatch', () => {
  it('trims_gateway_and_keeps_editable_fields_only', () => {
    expect(settingsPatch({ ...SETTINGS, gateway_url: '  https://api.example.test  ' })).toEqual({
      gateway_url: 'https://api.example.test',
      proxy_port: 15721,
      record_body: false,
      retention_days: 90
    })
  })
})
