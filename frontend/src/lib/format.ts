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
