import { memo } from 'react'
import { formatCompact, formatPoints } from '../../lib/format'
import type { CloudModelUsage } from '../../lib/quota'

/**
 * 云端模型分布列表。
 *
 * `memo` + 稳定的 `models` 引用：父级（概览页）在悬停曲线、切换范围时重渲染，
 * 这里不应跟着重算每个条形宽度。
 */
export const ModelList = memo(function ModelList({ models }: { models: CloudModelUsage[] }) {
  const maxCost = Math.max(...models.map((item) => item.cost), 0.0001)

  return (
    <ul className="model-list">
      {models.map((item) => (
        <ModelRow key={item.model} item={item} maxCost={maxCost} />
      ))}
    </ul>
  )
})

/** 单行模型用量：`item` 来自 `buildCloudModels` 的稳定数组，避免整表重渲染。 */
const ModelRow = memo(function ModelRow({
  item,
  maxCost
}: {
  item: CloudModelUsage
  maxCost: number
}) {
  return (
    <li className="model-item">
      <span className="model-name mono" title={item.model}>
        {item.model}
      </span>
      <span className="model-bar" aria-hidden="true">
        <span
          className="model-bar-fill"
          style={{ width: `${Math.max(2, Math.round((item.cost / maxCost) * 100))}%` }}
        />
      </span>
      <span className="model-metrics">
        <span>{formatCompact(item.tokens)} tokens</span>
        <span className="mono">{formatPoints(item.cost)} 积分</span>
      </span>
    </li>
  )
})
