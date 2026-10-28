interface StatCardProps {
  label: string
  value: string
  /** 数值单位，例如「积分」；以次要样式跟在数值后。 */
  unit?: string
  /** 强调第一张卡（今日 token）。 */
  accent?: boolean
}

export function StatCard({ label, value, unit, accent = false }: StatCardProps) {
  return (
    <article className={accent ? 'stat-card stat-card--accent' : 'stat-card'}>
      <p className="stat-label">{label}</p>
      <p className="stat-value">
        {value}
        {unit ? <span className="stat-unit">{unit}</span> : null}
      </p>
    </article>
  )
}
