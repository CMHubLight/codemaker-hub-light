import type { AppSettings, SettingsPatch } from './types'

/**
 * 设置表单的纯逻辑：校验、脏检查、数字字段的“编辑态字符串”。
 *
 * 为什么数字字段要单独保持字符串：`<input type="number">` 被清空时
 * `Number.parseInt('')` 是 `NaN`，旧实现立刻回填成 0，等于把“还没输入完”
 * 当成“设为 0”。这里保留用户原始字符串，失焦/提交时才解析校验。
 */

export type NumberField = 'proxy_port' | 'retention_days'

/** 数字字段的编辑态：保留用户输入原文，空串/非法值不落到 settings。 */
export type NumberDrafts = Record<NumberField, string>

interface NumberFieldSpec {
  min: number
  max: number
  /** 非法或越界时的提示文案。 */
  message: string
}

export const NUMBER_FIELD_SPECS: Record<NumberField, NumberFieldSpec> = {
  proxy_port: { min: 1024, max: 65535, message: '代理端口需为 1024~65535 之间的整数。' },
  retention_days: { min: 1, max: 3650, message: '保留天数需为 1~3650 之间的整数。' }
}

/** 可编辑字段集合（设置页只允许改这些；其余字段只读展示）。 */
export interface SettingsEditing {
  settings: AppSettings
  drafts: NumberDrafts
}

export function numberDraftsFrom(settings: AppSettings): NumberDrafts {
  return {
    proxy_port: String(settings.proxy_port),
    retention_days: String(settings.retention_days)
  }
}

/**
 * 严格解析十进制整数：空串、小数、带单位/空白以外的字符都返回 null。
 * 不用 `Number.parseInt`：它会接受 `"15721abc"`，也会把空串变成 NaN 后再被误当 0。
 */
export function parseNumberDraft(field: NumberField, raw: string): number | null {
  const text = raw.trim()
  if (!/^\d+$/.test(text)) return null
  const value = Number(text)
  if (!Number.isInteger(value)) return null
  const spec = NUMBER_FIELD_SPECS[field]
  if (value < spec.min || value > spec.max) return null
  return value
}

/** 失焦校验：返回错误文案；合法返回 null。 */
export function numberDraftError(field: NumberField, raw: string): string | null {
  return parseNumberDraft(field, raw) == null ? NUMBER_FIELD_SPECS[field].message : null
}

/**
 * 把编辑态合并成完整设置；任一数字字段非法返回 null。
 * 调用方先做 `numberDraftError` 拿到具体提示，再调用本函数。
 */
export function applyNumberDrafts(
  settings: AppSettings,
  drafts: NumberDrafts
): AppSettings | null {
  const proxyPort = parseNumberDraft('proxy_port', drafts.proxy_port)
  const retentionDays = parseNumberDraft('retention_days', drafts.retention_days)
  if (proxyPort == null || retentionDays == null) return null
  return { ...settings, proxy_port: proxyPort, retention_days: retentionDays }
}

/** 保存前的完整校验（数字范围已在编辑态校验，这里再兜一次并校验网关 URL）。 */
export function validateSettings(settings: AppSettings): string | null {
  if (
    !Number.isInteger(settings.proxy_port) ||
    settings.proxy_port < 1024 ||
    settings.proxy_port > 65535
  ) {
    return NUMBER_FIELD_SPECS.proxy_port.message
  }

  const gateway = settings.gateway_url.trim()
  if (!gateway) return '网关地址不能为空。'
  try {
    const url = new URL(gateway)
    if (url.protocol !== 'https:' && url.protocol !== 'http:') {
      return '网关地址需以 http:// 或 https:// 开头。'
    }
  } catch {
    return '网关地址不是合法 URL。'
  }

  if (
    !Number.isInteger(settings.retention_days) ||
    settings.retention_days < 1 ||
    settings.retention_days > 3650
  ) {
    return NUMBER_FIELD_SPECS.retention_days.message
  }

  return null
}

/**
 * 是否有未保存修改。
 *
 * 只比较用户可编辑的字段：数字用解析后的值比较（清空/非法输入算修改），
 * 避免后端只读字段变化把表单误判为“有未保存修改”。
 */
export function isSettingsDirty(editing: SettingsEditing, saved: AppSettings): boolean {
  const proxyPort = parseNumberDraft('proxy_port', editing.drafts.proxy_port)
  const retentionDays = parseNumberDraft('retention_days', editing.drafts.retention_days)
  if (proxyPort !== saved.proxy_port || retentionDays !== saved.retention_days) return true
  return (
    editing.settings.gateway_url !== saved.gateway_url ||
    editing.settings.record_body !== saved.record_body
  )
}

/** 设置 → `settings_update` 的局部补丁（未提交字段由后端保持原值）。 */
export function settingsPatch(settings: AppSettings): SettingsPatch {
  return {
    gateway_url: settings.gateway_url.trim(),
    proxy_port: settings.proxy_port,
    record_body: settings.record_body,
    retention_days: settings.retention_days
  }
}
