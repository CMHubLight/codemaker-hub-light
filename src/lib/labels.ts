import type { PillStatus } from '../components/StatusPill'

/** 协议枚举 → 展示名（与 src-tauri usage::RecentRequest.protocol 对齐）。 */
export function protocolLabel(protocol: string): string {
  switch (protocol) {
    case 'anthropic':
      return 'Anthropic'
    case 'openai_chat':
      return 'OpenAI Chat'
    case 'openai_responses':
      return 'OpenAI Responses'
    case 'unknown':
      return '未知'
    default:
      return protocol || '—'
  }
}

/** 请求状态 → 状态胶囊。 */
export function requestStatus(status: string): { tone: PillStatus; label: string } {
  switch (status) {
    case 'success':
      return { tone: 'ok', label: '成功' }
    case 'error':
      return { tone: 'error', label: '失败' }
    case 'cancelled':
      return { tone: 'warn', label: '已取消' }
    default:
      return { tone: 'idle', label: status || '未知' }
  }
}

/** 秒数 → “1 小时 2 分 / 32 秒”这类运行时长。 */
export function formatUptime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '—'
  if (seconds < 60) return `${Math.floor(seconds)} 秒`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分 ${Math.floor(seconds % 60)} 秒`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时 ${minutes % 60} 分`
  const days = Math.floor(hours / 24)
  return `${days} 天 ${hours % 24} 小时`
}
