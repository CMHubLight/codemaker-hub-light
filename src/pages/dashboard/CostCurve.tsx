import { memo, useEffect, useState } from 'react'
import type { KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react'
import {
  formatDateTime,
  formatInteger,
  formatYuanApprox
} from '../../lib/format'
import { axisLabel, formatTrendValue } from '../../lib/quota'
import type { CostTrend } from '../../lib/types'

/**
 * 费用曲线：24h 云端积分 / 30d 本机人民币；纯 SVG，不引入图表依赖。
 *
 * `memo`：趋势数据按 revision 更新，切换时间范围或悬停时不应连带重渲染
 * “最近请求/模型分布”等无关节点的父级子树（父级传下来的 trend 引用稳定）。
 */
export const CostCurve = memo(function CostCurve({ trend }: { trend: CostTrend }) {
  const width = 1000
  const height = 180
  const padX = 12
  const padY = 18
  const [hovered, setHovered] = useState<number | null>(null)
  const maxCost = Math.max(trend.peak_cost, 0.0001)
  const lastIndex = Math.max(trend.points.length - 1, 1)
  const coords = trend.points.map((point, index) => ({
    x: padX + (index / lastIndex) * (width - padX * 2),
    y: height - padY - (Math.max(point.cost, 0) / maxCost) * (height - padY * 2)
  }))
  const line = coords
    .map((point, index) => `${index === 0 ? 'M' : 'L'} ${point.x.toFixed(2)} ${point.y.toFixed(2)}`)
    .join(' ')
  const area = `${line} L ${coords[coords.length - 1].x.toFixed(2)} ${
    height - padY
  } L ${coords[0].x.toFixed(2)} ${height - padY} Z`

  useEffect(() => {
    setHovered(null)
  }, [trend])

  const active = hovered == null ? null : trend.points[hovered]
  const activeCoord = hovered == null ? null : coords[hovered]

  function handlePointerMove(event: ReactPointerEvent<HTMLDivElement>) {
    const rect = event.currentTarget.getBoundingClientRect()
    if (rect.width <= 0) return
    const mouseX = Math.min(
      Math.max(((event.clientX - rect.left) / rect.width) * width, padX),
      width - padX
    )
    let nearest = 0
    let nearestDistance = Number.POSITIVE_INFINITY
    coords.forEach((coord, index) => {
      const distance = Math.abs(coord.x - mouseX)
      if (distance < nearestDistance) {
        nearest = index
        nearestDistance = distance
      }
    })
    setHovered(nearest)
  }

  /** 键盘取点：容器获得焦点后左右方向键在曲线上移动（N-P3-34）。 */
  function moveHovered(delta: number) {
    if (trend.points.length === 0) return
    setHovered((current) => {
      if (current == null) return delta > 0 ? 0 : trend.points.length - 1
      return Math.min(Math.max(current + delta, 0), trend.points.length - 1)
    })
  }

  function handleKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (event.key === 'ArrowRight') {
      event.preventDefault()
      moveHovered(1)
    } else if (event.key === 'ArrowLeft') {
      event.preventDefault()
      moveHovered(-1)
    } else if (event.key === 'Home') {
      event.preventDefault()
      setHovered(0)
    } else if (event.key === 'End') {
      event.preventDefault()
      setHovered(trend.points.length - 1)
    }
  }

  // 汇总信息给读屏：曲线本身是 SVG，逐点数值只能靠键盘取点后的 tooltip 传达。
  const chartSummary = `费用曲线，共 ${trend.points.length} 个点，累计 ${formatTrendValue(
    trend,
    trend.total_cost
  )}，峰值 ${formatTrendValue(trend, trend.peak_cost)}；用左右方向键逐点查看。`

  const horizontal =
    activeCoord == null ? 'center' : activeCoord.x < width * 0.18
      ? 'start'
      : activeCoord.x > width * 0.82
        ? 'end'
        : 'center'
  const horizontalTransform =
    horizontal === 'start' ? 'translateX(0)' : horizontal === 'end' ? 'translateX(-100%)' : 'translateX(-50%)'
  const below = activeCoord != null && activeCoord.y < 70
  const tooltipTransform = `${horizontalTransform} ${
    below ? 'translateY(10px)' : 'translateY(calc(-100% - 10px))'
  }`

  return (
    <div className="cost-chart">
      <div
        className="cost-chart-plot"
        role="group"
        aria-label={chartSummary}
        tabIndex={0}
        onKeyDown={handleKeyDown}
        onPointerMove={handlePointerMove}
        onPointerLeave={() => setHovered(null)}
        onPointerCancel={() => setHovered(null)}
      >
        <svg
          className="cost-chart-svg"
          viewBox={`0 0 ${width} ${height}`}
          role="img"
          aria-label={`费用曲线，共 ${trend.points.length} 个点`}
        >
          <defs>
            <linearGradient id="cost-area-gradient" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="var(--accent)" stopOpacity="0.22" />
              <stop offset="100%" stopColor="var(--accent)" stopOpacity="0.02" />
            </linearGradient>
          </defs>
          {[padY, height / 2, height - padY].map((y) => (
            <line
              key={y}
              x1={padX}
              x2={width - padX}
              y1={y}
              y2={y}
              className="cost-chart-grid"
            />
          ))}
          <path d={area} fill="url(#cost-area-gradient)" />
          {activeCoord ? (
            <line
              x1={activeCoord.x}
              x2={activeCoord.x}
              y1={activeCoord.y}
              y2={height - padY}
              className="cost-chart-crosshair"
            />
          ) : null}
          <path d={line} className="cost-chart-line" />
          {coords.map((point, index) => (
            <g key={`${trend.points[index].at}-${index}`}>
              {hovered === index ? (
                <circle cx={point.x} cy={point.y} r="8" className="cost-chart-halo" />
              ) : null}
              <circle
                cx={point.x}
                cy={point.y}
                r={hovered === index ? 4.5 : 3}
                className={hovered === index ? 'cost-chart-dot is-active' : 'cost-chart-dot'}
              />
            </g>
          ))}
        </svg>
        {active && activeCoord ? (
          <div
            className="cost-tooltip"
            role="tooltip"
            aria-live="polite"
            style={{
              left: `${(activeCoord.x / width) * 100}%`,
              top: `${(activeCoord.y / height) * 100}%`,
              transform: tooltipTransform
            }}
          >
            <p className="cost-tooltip-time">{formatDateTime(active.at)}</p>
            <div className="cost-tooltip-row">
              <span>Token</span>
              <span className="mono">
                {active.tokens == null ? '—' : formatInteger(active.tokens)}
              </span>
            </div>
            <div className="cost-tooltip-row">
              <span>{trend.unit === 'cny' ? '花费' : '消耗'}</span>
              <span className="mono">{formatTrendValue(trend, active.cost)}</span>
            </div>
            {trend.unit === 'points' && trend.credit_to_yuan > 0 ? (
              <p className="cost-tooltip-approx">
                {formatYuanApprox(active.cost * trend.credit_to_yuan)}
              </p>
            ) : null}
          </div>
        ) : null}
      </div>
      <div className="trend-axis">
        <span>{axisLabel(trend.points[0])}</span>
        <span>{axisLabel(trend.points[trend.points.length - 1])}</span>
      </div>
      <p className="trend-note">
        累计 {formatTrendValue(trend, trend.total_cost)} · 峰值{' '}
        {formatTrendValue(trend, trend.peak_cost)}
        {trend.updated_at ? ` · 快照更新于 ${formatDateTime(trend.updated_at)}` : ''}
      </p>
    </div>
  )
})
