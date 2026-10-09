type Row = { label: string; rust: number; go: number }

type Props = {
  title: string
  unit: string
  note?: string
  lowerIsBetter?: boolean
  rows: Row[]
}

const RUST = 'var(--color-fd-primary)'
const GO = '#00ADD8'

function format(value: number) {
  return value.toLocaleString('en-US', { maximumFractionDigits: value < 10 ? 2 : 0 }).replaceAll(',', ' ')
}

function ratio(row: Row, lowerIsBetter: boolean) {
  const [better, worse] = lowerIsBetter ? [row.rust, row.go] : [row.go, row.rust]
  return worse / better
}

function Bar({ value, max, color, name, unit }: { value: number; max: number; color: string; name: string; unit: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="w-8 shrink-0 text-xs text-fd-muted-foreground">{name}</span>
      <div className="h-4 flex-1 rounded-sm bg-fd-secondary">
        <div className="h-4 rounded-sm" style={{ width: `${Math.max((value / max) * 100, 1)}%`, background: color }} />
      </div>
      <span className="w-28 shrink-0 text-right font-mono text-xs">
        {format(value)} {unit}
      </span>
    </div>
  )
}

export function CompareChart({ title, unit, note, lowerIsBetter = false, rows }: Props) {
  return (
    <figure className="not-prose my-8 rounded-lg border bg-fd-card p-5">
      <figcaption className="mb-4 flex flex-wrap items-baseline justify-between gap-2">
        <span className="font-semibold">{title}</span>
        <span className="flex items-center gap-3 text-xs text-fd-muted-foreground">
          <span className="flex items-center gap-1">
            <span className="inline-block size-3 rounded-sm" style={{ background: RUST }} /> Rust
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block size-3 rounded-sm" style={{ background: GO }} /> Go
          </span>
          {note && <span>{note}</span>}
        </span>
      </figcaption>
      <div className="flex flex-col gap-4">
        {rows.map((row) => {
          const max = Math.max(row.rust, row.go)
          const factor = ratio(row, lowerIsBetter)
          const rustWins = factor > 1.1
          const goWins = factor < 1 / 1.1
          return (
            <div key={row.label}>
              <div className="mb-1 flex justify-between text-sm">
                <span>{row.label}</span>
                <span
                  className={`font-mono text-xs ${rustWins ? 'text-fd-primary' : 'text-fd-muted-foreground'}`}
                >
                  {rustWins ? `Rust ${factor.toFixed(1)}×` : goWins ? `Go ${(1 / factor).toFixed(1)}×` : '≈'}
                </span>
              </div>
              <div className="flex flex-col gap-1">
                <Bar value={row.rust} max={max} color={RUST} name="Rust" unit={unit} />
                <Bar value={row.go} max={max} color={GO} name="Go" unit={unit} />
              </div>
            </div>
          )
        })}
      </div>
    </figure>
  )
}
