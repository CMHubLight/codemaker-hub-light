/** 设置页行内反馈消息（保存设置 / 重启代理 / 打开日志共用同一套样式语义）。 */
export interface SaveState {
  tone: 'ok' | 'error'
  text: string
}
