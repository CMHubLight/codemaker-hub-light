/** 展示层格式化工具：只做字符串转换，不参与业务计算。 */

const integerFormatter = new Intl.NumberFormat('zh-CN')
const pointsFormatter = new Intl.NumberFormat('zh-CN', {
  minimumFractionDigits: 0,
  maximumFractionDigits: 2
})

const dateTimeFormatter = new Intl.DateTimeFormat('zh-CN', {
  year: 'numeric',
  month: '2-digit',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit'
})

export function formatInteger(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return integerFormatter.format(Math.round(value))
}

/** token 等大数字：12345 → 12.3K，1234567 → 1.2M。 */
export function formatCompact(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—'
  if (value >= 1_000_000) return `${trimZero(value / 1_000_000, 1)}M`
  if (value >= 10_000) return `${trimZero(value / 1_000, 1)}K`
  return integerFormatter.format(Math.round(value))
}

/** 金额按方案 §7.2 展示 4 位小数。 */
export function formatYuan(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return `¥${value.toFixed(4)}`
}

/** 云端积分：保留最多 2 位小数，不加货币符号。 */
export function formatPoints(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return pointsFormatter.format(value)
}

/** 近似人民币：常规 2 位，极小金额保留 4 位。 */
export function formatYuanApprox(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—'
  const digits = Math.abs(value) > 0 && Math.abs(value) < 0.01 ? 4 : 2
  return `≈ ¥${value.toFixed(digits)}`
}

/** iso 时间字符串或 Unix 秒/毫秒，输出本地时间。 */
export function formatDateTime(value: string | number | null | undefined): string {
  if (value == null || value === '') return '—'
  const date = typeof value === 'number' ? new Date(toMilliseconds(value)) : new Date(value)
  if (Number.isNaN(date.getTime())) return '—'
  return dateTimeFormatter.format(date)
}

export function formatDuration(milliseconds: number | null | undefined): string {
  if (milliseconds == null || !Number.isFinite(milliseconds)) return '—'
  if (milliseconds < 1000) return `${Math.round(milliseconds)} ms`
  return `${(milliseconds / 1000).toFixed(2)} s`
}

function toMilliseconds(unixTime: number): number {
  // 后端约定用 Unix 秒；容错处理误传的毫秒值。
  return unixTime < 1e12 ? unixTime * 1000 : unixTime
}

function trimZero(value: number, digits: number): string {
  return value.toFixed(digits).replace(/\.0$/, '')
}
