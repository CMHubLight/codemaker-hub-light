import { formatPoints, formatYuan } from './format'
import type { CommandError } from './ipc'
import type { CostTrend, QuotaInfo } from './types'

/** 模型分布条目（quota.model_tokens/model_costs 合并后）。 */
export interface CloudModelUsage {
  model: string
  tokens: number
  cost: number
}

/**
 * 从 quota 的 model_tokens/model_costs 合并出模型分布；超过 8 个时聚合“其他”。
 * 纯函数，便于脱离组件单测（本轮暂无单独用例，行为由概览页测试覆盖）。
 */
export function buildCloudModels(quota: QuotaInfo | null): {
  items: CloudModelUsage[]
  total: number
} {
  if (!quota) return { items: [], total: 0 }
  const names = new Set([
    ...Object.keys(quota.model_tokens ?? {}),
    ...Object.keys(quota.model_costs ?? {})
  ])
  const all = Array.from(names)
    .map((model) => ({
      model,
      tokens: quota.model_tokens?.[model] ?? 0,
      cost: quota.model_costs?.[model] ?? 0
    }))
    .sort((left, right) => right.cost - left.cost || right.tokens - left.tokens)
  if (all.length <= 8) {
    return { items: all, total: all.length }
  }
  const top = all.slice(0, 8)
  const rest = all.slice(8)
  const other = {
    model: `其他 ${rest.length} 个模型`,
    tokens: rest.reduce((sum, item) => sum + item.tokens, 0),
    cost: rest.reduce((sum, item) => sum + item.cost, 0)
  }
  return { items: [...top, other], total: all.length }
}

/** 额度错误 → 用户可读文案（403/404/网络/超时分别处理）。 */
export function quotaErrorText(error: CommandError, hasLastSnapshot: boolean): string {
  switch (error.kind) {
    case 'forbidden':
      return '无权限访问额度信息（HTTP 403）。请确认账号已开通额度查询权限。'
    case 'not_found':
      return '额度接口不存在（HTTP 404）。请检查网关地址或服务端版本。'
    case 'unauthenticated':
      return '尚未登录，无法获取额度数据。'
    case 'network':
      return hasLastSnapshot
        ? '额度刷新失败：网络或网关暂不可用，已保留上次成功数据。'
        : '额度请求失败：网络或网关暂不可用，请检查网络后重试。'
    case 'timeout':
      return hasLastSnapshot
        ? '额度刷新超时，已保留上次成功数据。'
        : '额度请求超时（30 秒）；请检查网络后重试。'
    default:
      return `额度刷新失败：${error.raw}`
  }
}

/** 趋势值格式化：云端积分 / 本机人民币。 */
export function formatTrendValue(trend: CostTrend, value: number): string {
  return trend.unit === 'cny' ? formatYuan(value) : `${formatPoints(value)} 积分`
}

/** 24h 的首尾都是同一时刻，轴标签补日期避免“21:00 → 21:00”。 */
export function axisLabel(trend: CostTrend, point: CostTrend['points'][number]): string {
  if (trend.range === '24h' && point.at.length >= 16) {
    return `${point.at.slice(5, 10)} ${point.at.slice(11, 16)}`
  }
  return point.label
}
