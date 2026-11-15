import { useEffect, useRef, useState } from "react";
import { Area, AreaChart, ResponsiveContainer, Tooltip, YAxis } from "recharts";
import { fmtNum } from "../lib/format";

export interface CapSample {
  /** leased KV tokens across budget-gated deployments */
  used: number;
  /** summed effective KV budget (0 = fleet has no token gate) */
  budget: number;
  /** occupied request slots */
  slotsA: number;
  /** total request slots */
  slotsT: number;
}

const CAP = 120; // ~10 min at a 5 s poll

/** Accumulates one point per `tick` identity change (i.e. per poll) into a
 *  capped rolling window — the x axis is "samples", which at a fixed
 *  refetchInterval approximates wall time closely enough for a sparkline. */
export function useCapacitySeries(sample: CapSample | null, tick: unknown): CapSample[] {
  const ref = useRef<CapSample | null>(null);
  ref.current = sample;
  const [series, setSeries] = useState<CapSample[]>([]);
  useEffect(() => {
    const s = ref.current;
    if (!s) return;
    setSeries((prev) => [...prev, s].slice(-CAP));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick]);
  return series;
}

function fmtTok(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** Compact live-capacity tile: KV token budget usage as a rolling sparkline
 *  plus a request-slot bar. `budget == 0` renders slots-only. */
export function CapacityTile({ title, series, note }: { title: string; series: CapSample[]; note?: string }) {
  const cur = series[series.length - 1];
  const pct = cur && cur.budget > 0 ? (cur.used / cur.budget) * 100 : null;
  const chart = series.map((s, i) => ({ i, used: s.used, budget: s.budget }));
  const slotPct = cur && cur.slotsT > 0 ? (cur.slotsA / cur.slotsT) * 100 : 0;

  return (
    <div className="card flex flex-col p-4">
      <div className="mb-1 flex items-baseline justify-between">
        <div className="text-xs font-medium uppercase tracking-wide text-muted">{title}</div>
        {pct != null && (
          <span className={`text-[11px] font-medium ${pct >= 85 ? "text-rose-400" : pct >= 65 ? "text-amber-300" : "text-emerald-400"}`}>
            {pct.toFixed(0)}% KV used
          </span>
        )}
      </div>
      <div className="min-h-[70px] flex-1">
        {chart.length > 1 ? (
          <ResponsiveContainer width="100%" height={80}>
            <AreaChart data={chart} margin={{ top: 4, right: 2, bottom: 0, left: 2 }}>
              <YAxis hide domain={[0, (dataMax: number) => Math.max(dataMax, cur?.budget ?? 0)]} />
              <Tooltip
                cursor={{ fill: "rgba(61,111,174,0.09)" }}
                contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }}
                labelFormatter={() => ""}
                formatter={(v, name) => [fmtNum(Number(v)), name === "used" ? "kv used" : "budget"]}
              />
              <Area type="stepAfter" dataKey="budget" name="budget" stroke="#33415c" fill="none" strokeWidth={1} strokeDasharray="3 3" dot={false} isAnimationActive={false} />
              <Area type="monotone" dataKey="used" name="used" stroke="#34d399" fill="#34d399" fillOpacity={0.15} strokeWidth={2} isAnimationActive={false} />
            </AreaChart>
          </ResponsiveContainer>
        ) : (
          <div className="flex h-[80px] items-center justify-center text-[11px] text-muted">collecting samples…</div>
        )}
      </div>
      <div className="mt-1 space-y-1.5 text-[11px]">
        <div className="flex items-center justify-between">
          <span className="text-muted">token budget</span>
          <span className="font-mono text-text">
            {cur && cur.budget > 0 ? `${fmtTok(cur.used)} / ${fmtTok(cur.budget)}` : "slots only"}
          </span>
        </div>
        <div className="flex items-center justify-between">
          <span className="text-muted">request slots</span>
          <span className="font-mono text-text">{cur && cur.slotsT > 0 ? `${cur.slotsA} / ${cur.slotsT} active` : "—"}</span>
        </div>
        <div className="h-1.5 rounded bg-white/10">
          <div
            className={`h-1.5 rounded ${slotPct >= 85 ? "bg-rose-400" : slotPct >= 65 ? "bg-amber-300" : "bg-sky-400"}`}
            style={{ width: `${Math.min(100, slotPct)}%` }}
          />
        </div>
        {note && <div className="pt-0.5 text-[10px] text-muted/70">{note}</div>}
      </div>
    </div>
  );
}
