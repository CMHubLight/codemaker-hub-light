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
 * 订阅一个后端事件。浏览器预览（无 Tauri 后端）下自动跳过，
 * 避免 `pnpm dev` 时调用不存在的 IPC；订阅失败只静默降级到命令刷新。
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
        // 事件权限或后端未就绪：不阻塞界面，命令拉取仍然有效。
      })

    return () => {
      disposed = true
      unlisten?.()
    }
  }, [event])
}
