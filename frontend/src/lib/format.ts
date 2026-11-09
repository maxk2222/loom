// European/international date+time display — locale-independent, always
// "YYYY-MM-DD HH:mm" (24h). Use everywhere instead of toLocaleString(),
// which renders per-browser locale (US format on en-US systems).
const dtf = new Intl.DateTimeFormat("sv-SE", {
  year: "numeric", month: "2-digit", day: "2-digit",
  hour: "2-digit", minute: "2-digit", hour12: false,
});

export function fmtTs(v: string | number | Date | null | undefined): string {
  if (v == null || v === "") return "—";
  const d = v instanceof Date ? v : new Date(v);
  return Number.isNaN(d.getTime()) ? "—" : dtf.format(d);
}

const tf = new Intl.DateTimeFormat("sv-SE", { hour: "2-digit", minute: "2-digit", hour12: false });
const tfs = new Intl.DateTimeFormat("sv-SE", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });

export function fmtTime(v: string | number | Date | null | undefined): string {
  if (v == null || v === "") return "—";
  const d = v instanceof Date ? v : new Date(v);
  return Number.isNaN(d.getTime()) ? "—" : tf.format(d);
}

export function fmtTimeS(v: string | number | Date | null | undefined): string {
  if (v == null || v === "") return "—";
  const d = v instanceof Date ? v : new Date(v);
  return Number.isNaN(d.getTime()) ? "—" : tfs.format(d);
}

const df = new Intl.DateTimeFormat("sv-SE", { year: "numeric", month: "2-digit", day: "2-digit" });
const dtfShort = new Intl.DateTimeFormat("sv-SE", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false });

// Date-only "YYYY-MM-DD" — long-window chart ticks.
export function fmtDay(v: string | number | Date | null | undefined): string {
  if (v == null || v === "") return "—";
  const d = v instanceof Date ? v : new Date(v);
  return Number.isNaN(d.getTime()) ? "—" : df.format(d);
}

// Short "MM-DD HH:mm" — mid-window (week/month) chart ticks.
export function fmtDayT(v: string | number | Date | null | undefined): string {
  if (v == null || v === "") return "—";
  const d = v instanceof Date ? v : new Date(v);
  return Number.isNaN(d.getTime()) ? "—" : dtfShort.format(d);
}

// Percent display: sub-1% values get one decimal so an idle-but-alive host
// reads "0.3%" instead of a broken-looking "0%".
export function fmtPct(v: number | null | undefined): string {
  if (v == null) return "—";
  return v > 0 && v < 1 ? v.toFixed(1) : v.toFixed(0);
}

// Compact count display: 1234 → "1.2k", 1234567 → "1.2M". Used for token
// counts and other large numbers; values < 1000 render verbatim.
export function fmtNum(v: number | null | undefined): string {
  if (v == null || Number.isNaN(v)) return "—";
  const abs = Math.abs(v);
  if (abs >= 1e9) return `${(v / 1e9).toFixed(1)}B`;
  if (abs >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
  if (abs >= 1e3) return `${(v / 1e3).toFixed(1)}k`;
  return `${v}`;
}

// Millisecond duration, auto-scaled: 850ms → "850ms", 3200 → "3.2s",
// 250_000 → "4m 10s", 3_900_000 → "1h 5m".
export function fmtMs(v: number | null | undefined): string {
  if (v == null || Number.isNaN(v)) return "—";
  return fmtDur(v / 1000, true);
}

// Second duration, auto-scaled: 3.2 → "3.2s", 45 → "45s", 150 → "2m 30s",
// 4200 → "1h 10m", 200_000 → "2d 7h". `sub` renders sub-second ms values.
export function fmtDur(v: number | null | undefined, sub = false): string {
  if (v == null || Number.isNaN(v)) return "—";
  if (sub && v < 1) {
    const ms = Math.round(v * 1000);
    return ms >= 1000 ? "1.0s" : `${ms}ms`;
  }
  if (v < 1) return "<1s";
  const d = Math.floor(v / 86400);
  const h = Math.floor((v % 86400) / 3600);
  const m = Math.floor((v % 3600) / 60);
  const s = Math.floor(v % 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return s > 0 ? `${m}m ${s}s` : `${m}m`;
  return v < 10 && v % 1 !== 0 ? `${v.toFixed(1)}s` : `${s}s`;
}

// Uptime-style display from seconds: "2m 30s" → "1h 10m" → "2d 7h".
// Drops the smallest unit as magnitudes grow (uptime precision shrinks).
export function fmtUptime(v: number | null | undefined): string {
  if (v == null || Number.isNaN(v)) return "—";
  const d = Math.floor(v / 86400);
  const h = Math.floor((v % 86400) / 3600);
  const m = Math.floor((v % 3600) / 60);
  const s = Math.floor(v % 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m ${s}s`;
}

// Bytes → GiB with one decimal (RAM/VRAM columns).
export function fmtGiB(v: number | null | undefined): string {
  if (v == null || Number.isNaN(v)) return "—";
  return (v / 1024 ** 3).toFixed(1);
}
