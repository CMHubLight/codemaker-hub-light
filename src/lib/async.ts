import { useCallback, useEffect, useRef, useState } from 'react'
import { describeError, invokeCmd } from './ipc'
import type { CommandError, CommandName } from './ipc'

/**
 * 前端数据获取基建。
 *
 * 背景：四个页面此前各写一套 `cancelled` + `loading` + `refreshing` + `error` + 重试
 * （语义靠注释维持，容易漂移）；App 又让每个后台事件各自递增 revision，一次后台 tick
 * 连发多个事件会触发多轮重取。这里把三件事收敛成可测试的原语：
 *
 * - `useCommand`：单条命令的“最新调用获胜”（慢请求不覆盖新结果）+ 卸载止损 + 错误归一；
 * - `useAsyncResource`：按 key 重取的资源，区分首次加载与后台静默刷新，失败保留旧数据；
 * - `useCoalescedRefresh`：把窗口内的 N 个刷新信号合并成一次 revision 递增。
 *
 * 注意：Tauri 的 `invoke` 没有取消机制，这里的“取消/过期”只保证**界面状态**不被旧结果
 * 覆盖；Rust 侧命令仍会跑完（写操作可能已在后台生效，`ipc.ts` 的超时注释同理）。
 */

// ---------------------------------------------------------------------------
// useCommand：命令执行器
// ---------------------------------------------------------------------------

/** 一次命令执行的结果。 */
export type CommandOutcome<T> =
  | { status: 'ok'; data: T }
  | { status: 'error'; error: CommandError }
  /** 已被同一条命令的更新调用取代，或组件已卸载：调用方必须丢弃这个结果。 */
  | { status: 'stale' }

export interface CommandRunner<T> {
  /** 执行命令；同一 runner 上后发起的调用会让先前的调用返回 `stale`。 */
  run: (args?: Record<string, unknown>) => Promise<CommandOutcome<T>>
  /** 是否有执行中的调用（按钮禁用 / “正在…”文案用）。 */
  running: boolean
}

/**
 * 为一条 IPC 命令创建执行器。
 *
 * 一个 runner 只对应一条命令：不同命令互不取消（例如设置页同时保存与重启代理，
 * 不能让先发出的保存结果变成 stale）。需要并发互不干扰时，分别调用本 hook。
 */
export function useCommand<T>(command: CommandName): CommandRunner<T> {
  const [running, setRunning] = useState(false)
  const mounted = useRef(true)
  /** 调用代次：只有最后一次调用的结果可以写回界面。 */
  const ticket = useRef(0)
  /** 执行中调用计数：并行调用时，最后一个结束才收起 running。 */
  const active = useRef(0)

  useEffect(() => {
    mounted.current = true
    return () => {
      // 卸载后不再 setState；迟到的结果统一按 stale 丢弃。
      mounted.current = false
    }
  }, [])

  const run = useCallback(
    async (args?: Record<string, unknown>): Promise<CommandOutcome<T>> => {
      const current = ++ticket.current
      active.current += 1
      setRunning(true)
      try {
        const data = await invokeCmd<T>(command, args)
        return current === ticket.current && mounted.current
          ? { status: 'ok', data }
          : { status: 'stale' }
      } catch (cause) {
        return current === ticket.current && mounted.current
          ? { status: 'error', error: describeError(cause) }
          : { status: 'stale' }
      } finally {
        active.current -= 1
        if (active.current === 0 && mounted.current) setRunning(false)
      }
    },
    [command]
  )

  return { run, running }
}

// ---------------------------------------------------------------------------
// useAsyncResource：按键重取的资源
// ---------------------------------------------------------------------------

interface ResourceState<T> {
  data: T | null
  error: CommandError | null
  loading: boolean
  refreshing: boolean
}

export interface AsyncResource<T> extends ResourceState<T> {
  /** 手动重新读取：与事件刷新等价（已有数据时不闪加载态）。 */
  reload: () => void
  /** 用命令结果直接替换数据（例如「刷新额度」强制拉取后不必再读一次）。 */
  mutate: (next: T) => void
}

/**
 * 按 `key` 重取的资源。
 *
 * - `key` 必须包含全部取数输入（事件 revision、筛选、页码、时间范围…），
 *   只有 key 变化才重新执行 `load`；`load` 闭包本身不必稳定。
 * - 首次加载（还没有完成过任何一次请求）置 `loading`；之后的 key 变化/`reload`
 *   只置 `refreshing`，界面继续展示旧数据，避免后台刷新把内容闪成加载态。
 * - 失败时**保留旧数据**，只更新 `error`；调用方据此显示“旧数据/重试”而不是空表。
 */
export function useAsyncResource<T>(key: string, load: () => Promise<T>): AsyncResource<T> {
  const [state, setState] = useState<ResourceState<T>>({
    data: null,
    error: null,
    // 初始即进入首次加载态：首帧不闪现“暂无数据”。
    loading: true,
    refreshing: false
  })
  const loadRef = useRef(load)
  /** 是否已经发起过至少一次请求；用来区分首次加载与后台刷新。 */
  const attempted = useRef(false)
  const [nonce, setNonce] = useState(0)

  // `load` 每次渲染都是新闭包，但只在 key/nonce 变化时执行；
  // 用 ref 取最新闭包，避免把不稳定的 load 放进 effect 依赖里导致每渲染重取。
  useEffect(() => {
    loadRef.current = load
  })

  useEffect(() => {
    let cancelled = false
    const first = !attempted.current
    attempted.current = true
    setState((current) => ({
      // 保留旧数据：后台刷新期间表格/卡片继续可读。
      data: current.data,
      error: null,
      loading: first,
      refreshing: !first
    }))

    void (async () => {
      try {
        const data = await loadRef.current()
        if (cancelled) return
        setState({ data, error: null, loading: false, refreshing: false })
      } catch (cause) {
        if (cancelled) return
        setState((current) => ({
          data: current.data,
          error: describeError(cause),
          loading: false,
          refreshing: false
        }))
      }
    })()

    return () => {
      // key 变化 / 卸载：丢弃这次请求的结果（latest-wins）。
      cancelled = true
    }
  }, [key, nonce])

  const reload = useCallback(() => setNonce((value) => value + 1), [])

  const mutate = useCallback((next: T) => {
    setState({ data: next, error: null, loading: false, refreshing: false })
  }, [])

  return { ...state, reload, mutate }
}

// ---------------------------------------------------------------------------
// useCoalescedRefresh：合并刷新信号
// ---------------------------------------------------------------------------

export interface CoalescedRefresh {
  /** 合并窗口结束后递增的 revision，作为页面重取的依赖。 */
  revision: number
  /** 上报一次刷新信号；同一窗口内的多次调用只产生一次 revision 递增。 */
  bump: () => void
}

/**
 * 把窗口内的 N 个刷新信号合并成一次 revision 递增（尾部防抖）。
 *
 * 后端一次后台 tick 可能连发 `auth://state` + `quota://updated`，它们表达的是
 * 同一批变化；50ms 的合并窗口既能把多轮重取压成一轮，又远低于可感知延迟。
 */
export function useCoalescedRefresh(delayMs = 50): CoalescedRefresh {
  const [revision, setRevision] = useState(0)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(
    () => () => {
      if (timer.current != null) clearTimeout(timer.current)
    },
    []
  )

  const bump = useCallback(() => {
    // 窗口已开启：这次信号与前一次属于同一批刷新，直接丢弃。
    if (timer.current != null) return
    timer.current = setTimeout(() => {
      timer.current = null
      setRevision((value) => value + 1)
    }, delayMs)
  }, [delayMs])

  return { revision, bump }
}
