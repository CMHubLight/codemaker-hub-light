import { listen } from '@tauri-apps/api/event'
import type { UnlistenFn } from '@tauri-apps/api/event'
import { useEffect, useRef } from 'react'
import { isTauri } from './ipc'
import type {
  AuthCallbackEvent,
  AuthStatus,
  ProxyErrorEvent,
  ProxyStatus,
  QuotaInfo
} from './types'

/** 后端事件名（src-tauri 发射端保持同名）。 */
export type AppEventName =
  | 'auth://state'
  | 'auth://callback'
  | 'quota://updated'
  | 'proxy://status'
  | 'proxy://error'
  | 'app://second-instance'

/** 事件名 → payload 类型。 */
export interface AppEventPayloads {
  'auth://state': AuthStatus
  'auth://callback': AuthCallbackEvent
  'quota://updated': QuotaInfo
  'proxy://status': ProxyStatus
  'proxy://error': ProxyErrorEvent
  /** 后端目前只发空对象（窗口聚焦由后端完成），这里仅用于触发刷新。 */
  'app://second-instance': Record<string, never>
}

/**
 * listen 失败后的退避间隔（毫秒）。
 *
 * 启动早期 WebView 已就绪但 Tauri 事件系统/权限尚未可用时，`listen` 可能直接拒绝；
 * 旧实现静默放弃，注释里承诺的“降级到命令刷新”并不存在（只有首次挂载会读状态），
 * 于是状态会一直停在启动那一刻（N-P3-29）。这里做有限次退避重试，
 * 与 App 的 60s `auth_status` 兜底轮询互补：重试覆盖短时抖动，轮询兜底长期失败。
 */
export const LISTEN_RETRY_DELAYS_MS = [250, 500, 1000] as const

/** 总共尝试次数 = 首次订阅 + 3 次退避重试。 */
const MAX_LISTEN_ATTEMPTS = LISTEN_RETRY_DELAYS_MS.length + 1

/**
 * 订阅一个后端事件。浏览器预览（无 Tauri 后端）下自动跳过，
 * 避免 `pnpm dev` 时调用不存在的 IPC；订阅失败按退避重试，最终仍失败不阻塞界面。
 */
export function useTauriEvent<K extends AppEventName>(
  event: K,
  handler: (payload: AppEventPayloads[K]) => void
): void {
  const handlerRef = useRef(handler)

  useEffect(() => {
    handlerRef.current = handler
  })

  useEffect(() => {
    if (!isTauri()) return

    let disposed = false
    let unlisten: UnlistenFn | undefined
    let retryTimer: ReturnType<typeof setTimeout> | undefined
    /** 已发起的订阅次数（含首次）：决定下一次退避间隔与是否继续重试。 */
    let attempts = 0

    function subscribe() {
      attempts += 1
      void listen<AppEventPayloads[K]>(event, (received) => {
        if (!disposed) handlerRef.current(received.payload)
      })
        .then((dispose) => {
          if (disposed) {
            dispose()
          } else {
            unlisten = dispose
          }
        })
        .catch(() => {
          // 事件权限或后端未就绪：不阻塞界面，后台命令拉取仍然有效。
          if (disposed || attempts >= MAX_LISTEN_ATTEMPTS) return
          retryTimer = setTimeout(subscribe, LISTEN_RETRY_DELAYS_MS[attempts - 1])
        })
    }

    subscribe()

    return () => {
      disposed = true
      // 卸载/事件名变化时取消尚未触发的重试，避免对已卸载组件再次 listen。
      if (retryTimer !== undefined) clearTimeout(retryTimer)
      unlisten?.()
    }
  }, [event])
}
