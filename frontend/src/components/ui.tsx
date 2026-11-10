import { clsx } from "clsx";
import { Inbox, X } from "lucide-react";
import { useEffect, useRef, type ReactNode } from "react";
import { createPortal } from "react-dom";

/* ---------- layout primitives ---------- */

export function PageHeader({
  title,
  desc,
  actions,
}: {
  title: ReactNode;
  desc?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="mb-5 flex flex-wrap items-end justify-between gap-3">
      <div>
        <h1 className="text-lg font-semibold tracking-tight">{title}</h1>
        {desc && <p className="mt-0.5 text-sm text-muted">{desc}</p>}
      </div>
      {actions && <div className="flex items-center gap-2">{actions}</div>}
    </div>
  );
}

export function Card({
  className,
  children,
  hover,
}: {
  className?: string;
  children: ReactNode;
  hover?: boolean;
}) {
  return <div className={clsx("card p-4", hover && "card-hover", className)}>{children}</div>;
}

export function Stat({
  label,
  value,
  sub,
  icon,
}: {
  label: string;
  value: ReactNode;
  sub?: string;
  icon?: ReactNode;
}) {
  return (
    <Card hover>
      <div className="flex items-center justify-between">
        <div className="text-xs text-muted">{label}</div>
        {icon && <div className="text-muted/60">{icon}</div>}
      </div>
      <div className="mt-1 text-2xl font-semibold tracking-tight">{value}</div>
      {sub && <div className="mt-0.5 text-xs text-muted">{sub}</div>}
    </Card>
  );
}

/* ---------- status ---------- */

const TONES = {
  ok: "bg-ok/12 text-ok",
  warn: "bg-warn/12 text-warn",
  err: "bg-err/12 text-err",
  muted: "bg-panel2 text-muted",
  accent: "bg-accent/12 text-accent",
} as const;

export function Badge({
  tone,
  children,
  dot,
}: {
  tone: keyof typeof TONES;
  children: ReactNode;
  dot?: boolean;
}) {
  return (
    <span className={clsx("badge", TONES[tone])}>
      {dot && <span className="live-dot" />}
      {children}
    </span>
  );
}

export function LiveBadge({ live }: { live: boolean }) {
  return live ? (
    <Badge tone="ok" dot>
      online
    </Badge>
  ) : (
    <Badge tone="muted">offline</Badge>
  );
}

/* ---------- feedback ---------- */

export function Modal({
  open,
  title,
  onClose,
  children,
  wide,
}: {
  open: boolean;
  title: string;
  onClose: () => void;
  children: ReactNode;
  wide?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const h = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [open, onClose]);
  if (!open) return null;
  // Portal to body: page-enter's persisted transform would otherwise make
  // this `fixed` overlay position relative to the content container.
  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-4 backdrop-blur-sm"
      onMouseDown={(e) => e.target === ref.current && onClose()}
    >
      <div
        ref={ref}
        className={clsx(
          "card page-enter max-h-[90vh] w-full overflow-y-auto p-5",
          wide ? "max-w-3xl" : "max-w-lg",
        )}
      >
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-base font-semibold tracking-tight">{title}</h2>
          <button className="btn-ghost p-1" onClick={onClose} aria-label="close">
            <X size={16} />
          </button>
        </div>
        {children}
      </div>
    </div>,
    document.body,
  );
}

export function ConfirmModal({
  open,
  title,
  body,
  confirmLabel = "Confirm",
  danger,
  alert,
  busy,
  onConfirm,
  onClose,
}: {
  open: boolean;
  title: string;
  body: ReactNode;
  confirmLabel?: string;
  danger?: boolean;
  /** alert mode: single dismiss button, no cancel */
  alert?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onClose: () => void;
}) {
  return (
    <Modal open={open} title={title} onClose={onClose}>
      <div className="text-sm text-muted">{body}</div>
      <div className="mt-5 flex justify-end gap-2">
        {!alert && (
          <button className="btn-ghost px-3 py-1.5 text-sm" onClick={onClose} disabled={busy}>
            Cancel
          </button>
        )}
        <button
          className={clsx("px-3 py-1.5 text-sm", danger && !alert ? "btn-danger" : "btn")}
          onClick={onConfirm}
          disabled={busy}
        >
          {busy ? "…" : confirmLabel}
        </button>
      </div>
    </Modal>
  );
}

export function Field({
  label,
  children,
  hint,
}: {
  label: string;
  children: ReactNode;
  hint?: string;
}) {
  return (
    <div>
      <label className="label">{label}</label>
      {children}
      {hint && <div className="mt-1 text-xs text-muted">{hint}</div>}
    </div>
  );
}

export function Select({
  value,
  onChange,
  options,
  className,
}: {
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
  className?: string;
}) {
  return (
    <select className={clsx("input", className)} value={value} onChange={(e) => onChange(e.target.value)}>
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label?: string;
}) {
  return (
    <button type="button" onClick={() => onChange(!checked)} className="flex items-center gap-2 text-sm">
      <span
        className={clsx(
          "relative inline-flex h-5 w-9 items-center rounded-full transition-colors",
          checked ? "bg-accent shadow-glow" : "bg-border",
        )}
      >
        <span
          className={clsx(
            "inline-block h-3.5 w-3.5 transform rounded-full bg-white transition-transform",
            checked ? "translate-x-5" : "translate-x-0.5",
          )}
        />
      </span>
      {label && <span className="text-muted">{label}</span>}
    </button>
  );
}

export function Empty({ msg }: { msg: string }) {
  return (
    <div className="flex flex-col items-center gap-2 py-10 text-sm text-muted">
      <Inbox size={22} className="text-muted/50" />
      {msg}
    </div>
  );
}

export function Spinner() {
  return (
    <div className="flex justify-center py-10">
      <div className="h-6 w-6 animate-spin rounded-full border-2 border-border border-t-accent" />
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={clsx("skeleton h-4 w-full", className)} />;
}

/* ---------- logo ---------- */

export function Logo({ size = 28, className }: { size?: number; className?: string }) {
  return (
    <img
      src="/ui/logo.png"
      alt="LOOM"
      width={size}
      height={size}
      className={clsx("object-contain drop-shadow-[0_0_12px_rgba(77,141,255,.35)]", className)}
      style={{ width: size, height: size }}
    />
  );
}
