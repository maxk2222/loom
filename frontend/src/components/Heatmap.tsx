interface HeatmapProps {
  /** grid[row][col] = count; row 0 = smallest bucket, rendered bottom-up. */
  grid: number[][];
  /** Row labels in grid order (row 0 label = labels[0]). */
  labels: string[];
  /** Largest cell value — drives colour intensity (sqrt-scaled). */
  max: number;
  /** Sparse x-axis label for column i ("" for no label). */
  colLabel?: (i: number) => string;
  /** Tooltip text for a cell. */
  title?: (row: number, col: number, n: number) => string;
  /** Accent colour used for non-empty cells (rgb parts). */
  rgb?: string;
}

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
        {colLabel && (
          <div className="flex items-center gap-2">
            <div className="w-16 shrink-0" />
            <div className="grid flex-1 gap-px" style={{ gridTemplateColumns: `repeat(${cols}, minmax(0,1fr))` }}>
              {Array.from({ length: cols }, (_, c) => (
                <div key={c} className="truncate text-center text-[9px] leading-4 text-muted">{colLabel(c)}</div>
              ))}
            </div>
          </div>
        )}
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
