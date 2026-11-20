import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Area, AreaChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { api } from "../lib/api";
import { fmtNum, fmtTimeS, fmtDayT, fmtDay } from "../lib/format";

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

/** The compact tile is a "right now" sparkline — a short trailing window,
 *  trimmed by wall-clock so slow/fast polls cover the same span. Long
 *  history lives in the persisted series behind CapacityChart. */
const LIVE_WINDOW_MS = 30 * 60_000;
const CAP = 600; // hard cap — 30 min at a 3 s poll fits with margin

/** Accumulates one stamped point per `tick` identity change (i.e. per
 *  poll), trimmed to the trailing LIVE_WINDOW_MS so the tile always shows
 *  "the last ~30 minutes" regardless of poll cadence. */
export function useCapacitySeries(sample: CapSample | null, tick: unknown): CapSample[] {
  const ref = useRef<CapSample | null>(null);
  ref.current = sample;
  const [series, setSeries] = useState<CapSample[]>([]);
  useEffect(() => {
    const s = ref.current;
    if (!s) return;
    const now = Date.now();
    setSeries((prev) =>
      [...prev, { ...s, t: now }]
        .filter((p) => (p.t ?? 0) >= now - LIVE_WINDOW_MS)
        .slice(-CAP)
    );
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

interface CapBucket {
  /** bucket start, unix seconds */
  t: number;
  kv_used: number;
  kv_budget: number;
  slots_total: number;
  slots_active: number;
  /** per-tick utilisation percentiles within the bucket (null = no
   *  gated/live capacity that tick → honest gap) */
  kv_p50?: number | null;
  kv_p95?: number | null;
  kv_p99?: number | null;
  slots_p50?: number | null;
  slots_p95?: number | null;
  slots_p99?: number | null;
}

/** One p50/p95/p99 area panel — same visual language as the latency
 *  percentile cards (green/amber/rose). */
function PctPanel({ label, prefix, data, tick, footer }: {
  label: string;
  /** "kv" | "slots" — dataKey prefix in each row */
  prefix: "kv" | "slots";
  data: Record<string, number | null>[];
  tick: (ms: number) => string;
  footer?: string | null;
}) {
  const sparse = data.length < 6;
  const area = (k: string, name: string, stroke: string) => (
    <Area
      key={k}
      type="monotone"
      dataKey={`${prefix}_${k}`}
      name={name}
      stroke={stroke}
      fill={stroke}
      fillOpacity={0.10}
      strokeWidth={2}
      dot={sparse ? { r: 3, fill: stroke, strokeWidth: 0 } : false}
      connectNulls={false}
      isAnimationActive={false}
    />
  );
  return (
    <div>
      <div className="mb-1 flex items-baseline justify-between">
        <span className="text-[11px] font-medium text-muted">{label}</span>
        {footer && <span className="font-mono text-[10px] text-muted/70">{footer}</span>}
      </div>
      <ResponsiveContainer width="100%" height={170}>
        <AreaChart data={data} margin={{ top: 4, right: 8, bottom: 0, left: -18 }}>
          <CartesianGrid stroke="#1c2740" vertical={false} />
          <XAxis
            dataKey="t"
            type="number"
            domain={["dataMin", "dataMax"]}
            tick={{ fontSize: 10, fill: "#5b6b8c" }}
            tickLine={false}
            axisLine={false}
            tickFormatter={(v) => tick(Number(v))}
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
            labelFormatter={(v) => fmtDayT(Number(v))}
            formatter={(v, name) => [`${Number(v).toFixed(1)}%`, name]}
          />
          <Legend wrapperStyle={{ fontSize: 11 }} />
          {area("p50", "p50", "#34d399")}
          {area("p95", "p95", "#fbbf24")}
          {area("p99", "p99", "#fb7185")}
        </AreaChart>
      </ResponsiveContainer>
    </div>
  );
}

/** Capacity history card: two side-by-side percentile panels (KV budget /
 *  request slots) over a real time axis, fed by the persisted sampler
 *  series (server /api/stats/capacity-series or client
 *  /local/api/capacity-series). Null percentiles leave honest gaps. */
export function CapacityChart({ title, url, win, note }: {
  title: string;
  /** series endpoint accepting ?window= */
  url: string;
  /** shared window selector — controlled by the page-level picker like
   *  the other history charts */
  win: string;
  note?: string;
}) {
  const q = useQuery({
    queryKey: ["capacity-series", url, win],
    queryFn: () => api.get<{ buckets: CapBucket[] }>(`${url}?window=${win}`),
    refetchInterval: 15000,
  });
  // Same window→formatter convention as the monitoring charts: sub-day
  // windows get time-of-day ticks, day-plus windows get day+time.
  const tick = win === "1h" || win === "6h" || win === "24h" ? fmtTimeS
    : win === "all" || win === "1y" || win === "365d" ? fmtDay : fmtDayT;
  const buckets = q.data?.buckets ?? [];
  const data: Record<string, number | null>[] = buckets.map((b) => ({
    t: b.t * 1000,
    kv_p50: b.kv_p50 ?? null,
    kv_p95: b.kv_p95 ?? null,
    kv_p99: b.kv_p99 ?? null,
    slots_p50: b.slots_p50 ?? null,
    slots_p95: b.slots_p95 ?? null,
    slots_p99: b.slots_p99 ?? null,
  }));
  const last = buckets[buckets.length - 1];

  return (
    <div className="card p-4">
      <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted">{title}</div>
      {data.length > 0 ? (
        <div className="grid gap-6 md:grid-cols-2">
          <PctPanel
            label="kv budget used"
            prefix="kv"
            data={data}
            tick={tick}
            footer={last && last.kv_budget > 0 ? `kv ${fmtTok(last.kv_used)} / ${fmtTok(last.kv_budget)}` : "slots only"}
          />
          <PctPanel
            label="request slots used"
            prefix="slots"
            data={data}
            tick={tick}
            footer={last && last.slots_total > 0 ? `slots ${last.slots_active} / ${last.slots_total}` : null}
          />
        </div>
      ) : (
        <div className="flex h-[200px] items-center justify-center text-[11px] text-muted">
          {q.isError ? "series unavailable" : q.isLoading ? "loading…" : "collecting samples…"}
        </div>
      )}
      <div className="mt-1 text-[10px] text-muted/70">
        {note ?? "rolling window — collected while this page is open"}
      </div>
    </div>
  );
}
