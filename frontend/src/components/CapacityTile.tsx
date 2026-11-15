import { useEffect, useRef, useState } from "react";
import { Area, AreaChart, CartesianGrid, Legend, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { fmtNum, fmtTimeS } from "../lib/format";

export interface CapSample {
  /** leased KV tokens across budget-gated deployments */
  used: number;
  /** summed effective KV budget (0 = fleet has no token gate) */
  budget: number;
  /** occupied request slots */
  slotsA: number;
  /** total request slots */
  slotsT: number;
  /** sample wall-clock time, stamped by useCapacitySeries */
  t?: number;
}

const CAP = 240; // ~20 min at a 5 s poll (12 min at 3 s)

/** Accumulates one point per `tick` identity change (i.e. per poll) into a
 *  capped rolling window. Each sample is wall-clock stamped so charts can
 *  use a real time axis instead of assuming uniform spacing. */
export function useCapacitySeries(sample: CapSample | null, tick: unknown): CapSample[] {
  const ref = useRef<CapSample | null>(null);
  ref.current = sample;
  const [series, setSeries] = useState<CapSample[]>([]);
  useEffect(() => {
    const s = ref.current;
    if (!s) return;
    setSeries((prev) => [...prev, { ...s, t: Date.now() }].slice(-CAP));
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

/** Full-width capacity chart: KV-budget and request-slot usage as two
 *  percentage lines over a real time axis (rolling window, see CAP).
 *  A null percentage (no token budget / no slots configured) leaves an
 *  honest gap instead of a misleading flat zero. */
export function CapacityChart({ title, series, note }: { title: string; series: CapSample[]; note?: string }) {
  // t-less samples (not stamped by the hook) would sit at the epoch and
  // stretch the axis — drop them rather than distort the chart.
  const data = series.filter((s) => s.t != null).map((s) => ({
    t: s.t!,
    kv: s.budget > 0 ? +((s.used / s.budget) * 100).toFixed(1) : null,
    slots: s.slotsT > 0 ? +((s.slotsA / s.slotsT) * 100).toFixed(1) : null,
  }));
  const cur = data[data.length - 1];
  const last = [...series].reverse().find((s) => s.t != null);

  return (
    <div className="card p-4">
      <div className="mb-2 flex items-baseline justify-between">
        <div className="text-xs font-medium uppercase tracking-wide text-muted">{title}</div>
        <div className="flex items-center gap-4 text-[11px]">
          {cur?.kv != null && (
            <span className={`font-medium ${cur.kv >= 85 ? "text-rose-400" : cur.kv >= 65 ? "text-amber-300" : "text-emerald-400"}`}>
              {cur.kv.toFixed(0)}% kv
            </span>
          )}
          {cur?.slots != null && (
            <span className={`font-medium ${cur.slots >= 85 ? "text-rose-400" : cur.slots >= 65 ? "text-amber-300" : "text-sky-400"}`}>
              {cur.slots.toFixed(0)}% slots
            </span>
          )}
        </div>
      </div>
      {data.length > 1 ? (
        <ResponsiveContainer width="100%" height={200}>
          <LineChart data={data} margin={{ top: 4, right: 8, bottom: 0, left: -18 }}>
            <CartesianGrid stroke="#1c2740" vertical={false} />
            <XAxis
              dataKey="t"
              type="number"
              domain={["dataMin", "dataMax"]}
              tick={{ fontSize: 10, fill: "#5b6b8c" }}
              tickLine={false}
              axisLine={false}
              tickFormatter={(v) => fmtTimeS(Number(v))}
              minTickGap={40}
            />
            <YAxis
              domain={[0, (dataMax: number) => Math.max(100, Math.ceil(dataMax))]}
              tick={{ fontSize: 10, fill: "#5b6b8c" }}
              tickLine={false}
              axisLine={false}
              tickFormatter={(v) => `${v}%`}
            />
            <Tooltip
              cursor={{ stroke: "#4a86c8", strokeOpacity: 0.35 }}
              contentStyle={{ background: "#101828", border: "1px solid #1c2740", borderRadius: 8, fontSize: 12 }}
              labelFormatter={(v) => fmtTimeS(Number(v))}
              formatter={(v, name) => [`${Number(v).toFixed(1)}%`, name]}
            />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            <Line type="monotone" dataKey="kv" name="kv budget used" stroke="#34d399" strokeWidth={2} dot={false} connectNulls={false} isAnimationActive={false} />
            <Line type="monotone" dataKey="slots" name="request slots used" stroke="#38bdf8" strokeWidth={2} dot={false} connectNulls={false} isAnimationActive={false} />
          </LineChart>
        </ResponsiveContainer>
      ) : (
        <div className="flex h-[200px] items-center justify-center text-[11px] text-muted">collecting samples…</div>
      )}
      <div className="mt-1 flex items-baseline justify-between text-[10px] text-muted/70">
        <span>{note ?? "rolling window — collected while this page is open"}</span>
        {cur && last && (
          <span className="font-mono">
            {cur.kv != null && `kv ${fmtTok(last.used)} / ${fmtTok(last.budget)}`}
            {cur.kv != null && cur.slots != null && " · "}
            {cur.slots != null && `slots ${last.slotsA} / ${last.slotsT}`}
          </span>
        )}
      </div>
    </div>
  );
}
