interface HeatmapProps {
  /** grid[row][col] = count; row 0 = smallest bucket, rendered bottom-up. */
  grid: number[][];
  /** Row labels in grid order (row 0 label = labels[0]). */
  labels: string[];
  /** Largest cell value — drives colour intensity (sqrt-scaled). */
  max: number;
  /** X-axis label text for column i; the component picks a sparse subset. */
  colLabel?: (i: number) => string;
  /** Tooltip text for a cell. */
  title?: (row: number, col: number, n: number) => string;
  /** Accent colour used for non-empty cells (rgb parts). */
  rgb?: string;
}

export interface HeatmapData {
  bucket_secs: number;
  token_buckets: string[];
  cells: { t: string; b: number; n: number }[];
}

/** Expand the server's sparse {t,b,n} cells into a dense time×bucket grid
    from the first data point to now (empty columns stay dark). */
export function expandHeatmap(d: HeatmapData | undefined, tick: (ms: number) => string) {
  if (!d || d.cells.length === 0) return null;
  const bw = d.bucket_secs * 1000;
  const t0 = Math.min(...d.cells.map((c) => new Date(c.t).getTime()));
  const cols = Math.max(1, Math.ceil((Date.now() - t0) / bw));
  const grid = Array.from({ length: d.token_buckets.length }, () => new Array<number>(cols).fill(0));
  let max = 0;
  for (const c of d.cells) {
    const i = Math.min(cols - 1, Math.floor((new Date(c.t).getTime() - t0) / bw));
    grid[c.b][i] += c.n;
    if (grid[c.b][i] > max) max = grid[c.b][i];
  }
  return { grid, max, t0, bw, tick };
}

const MAX_X_LABELS = 8;

/// Token-per-request heatmap: rows = token-size buckets (largest on top),
/// columns = time buckets. Cell intensity is sqrt-scaled — a single hot
/// column must not flatten everything else to invisible. Rendered as a
/// CSS grid — recharts has no heatmap type.
export function TokenHeatmap({ grid, labels, max, colLabel, title, rgb = "91,141,239" }: HeatmapProps) {
  const cols = grid[0]?.length ?? 0;
  if (grid.length === 0 || cols === 0) return null;
  const total = grid.flat().reduce((a, b) => a + b, 0);
  const alpha = (n: number) => (0.10 + 0.90 * Math.sqrt(n / max)).toFixed(2);
  return (
    <div>
      <div className="space-y-0.5">
        {[...grid.keys()].reverse().map((r) => (
          <div key={r} className="flex items-center gap-2">
            <div className="w-16 shrink-0 text-right text-[10px] text-muted">{labels[r] ?? r}</div>
            <div className="grid flex-1 gap-px" style={{ gridTemplateColumns: `repeat(${cols}, minmax(0,1fr))` }}>
              {grid[r].map((n, c) => (
                <div
                  key={c}
                  className="h-3.5 rounded-[2px] transition-transform duration-75 hover:scale-110 hover:ring-1 hover:ring-white/40"
                  title={title?.(r, c, n)}
                  style={{ background: n === 0 ? "#131a22" : `rgba(${rgb},${alpha(n)})` }}
                />
              ))}
            </div>
          </div>
        ))}
        {colLabel && (() => {
          const step = Math.max(1, Math.ceil(cols / MAX_X_LABELS));
          // Labels overflow their own (tiny) grid cell and centre on it — a
          // sparse subset never collides, a clipped "1…" is worse.
          return (
            <div className="flex items-center gap-2">
              <div className="w-16 shrink-0" />
              <div className="grid flex-1 gap-px" style={{ gridTemplateColumns: `repeat(${cols}, minmax(0,1fr))` }}>
                {Array.from({ length: cols }, (_, c) => (
                  <div key={c} className="whitespace-nowrap text-center text-[9px] leading-4 text-muted">
                    {c % step === 0 ? colLabel(c) : ""}
                  </div>
                ))}
              </div>
            </div>
          );
        })()}
      </div>
      <div className="mt-2 flex items-center justify-end gap-2 text-[10px] text-muted">
        <span>{total.toLocaleString()} requests</span>
        <span>fewer</span>
        <div className="flex gap-px">
          {[0.05, 0.15, 0.3, 0.5, 0.75, 1].map((f) => (
            <div key={f} className="h-2.5 w-4 rounded-[2px]" style={{ background: `rgba(${rgb},${(0.10 + 0.90 * Math.sqrt(f)).toFixed(2)})` }} />
          ))}
        </div>
        <span>more (max {max.toLocaleString()})</span>
      </div>
    </div>
  );
}
