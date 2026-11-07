import { clsx } from "clsx";
import {
  Boxes,
  Cpu,
  Gauge,
  Home,
  Key,
  Layers,
  LogOut,
  MonitorCog,
  Projector,
  ScrollText,
  Settings as SettingsIcon,
  Share2,
  Users,
} from "lucide-react";
import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { api, APP_VARIANT } from "../lib/api";
import { useBranding } from "../lib/branding";
import { Logo } from "./ui";

interface NavItem {
  to: string;
  label: string;
  icon: React.ReactNode;
}

const SERVER_NAV: NavItem[] = [
  { to: "/", label: "Overview", icon: <Home size={16} /> },
  { to: "/clients", label: "Clients", icon: <Cpu size={16} /> },
  { to: "/models", label: "Models", icon: <Boxes size={16} /> },
  { to: "/deployments", label: "Deployments", icon: <Layers size={16} /> },
  { to: "/scheduler", label: "Scheduler", icon: <Gauge size={16} /> },
  { to: "/projects", label: "Projects", icon: <Projector size={16} /> },
  { to: "/keys", label: "API Keys", icon: <Key size={16} /> },
  { to: "/users", label: "Users", icon: <Users size={16} /> },
  { to: "/monitoring", label: "Monitoring", icon: <ScrollText size={16} /> },
  { to: "/settings", label: "Settings", icon: <SettingsIcon size={16} /> },
];

const CLIENT_NAV: NavItem[] = [
  { to: "/", label: "Status", icon: <Home size={16} /> },
  { to: "/backends", label: "Backends", icon: <Cpu size={16} /> },
  { to: "/sharing", label: "Sharing", icon: <Share2 size={16} /> },
  { to: "/requests", label: "Requests", icon: <MonitorCog size={16} /> },
  { to: "/connection", label: "Connection", icon: <Gauge size={16} /> },
];

export default function Layout() {
  const brand = useBranding();
  const nav = useNavigate();
  const items = APP_VARIANT === "client" ? CLIENT_NAV : SERVER_NAV;

  const logout = async () => {
    try {
      await api.post("/api/auth/logout");
    } catch {
      /* ignore */
    }
    nav("/login");
  };

  return (
    <div className="flex min-h-screen">
      <aside className="flex w-56 shrink-0 flex-col border-r border-border bg-panel/60 backdrop-blur">
        <div className="grad-line" />
        <div className="flex items-center gap-2.5 px-4 pb-4 pt-4">
          <Logo size={30} />
          <div className="min-w-0">
            <div className="grad-text text-sm font-bold leading-tight tracking-wide">
              {brand.app_name}
            </div>
            <div className="truncate text-[10px] leading-tight text-muted">
              {APP_VARIANT === "client" ? "Workstation Client" : brand.tagline}
            </div>
          </div>
        </div>
        <nav className="flex-1 space-y-0.5 overflow-y-auto px-2.5">
          {items.map((it) => (
            <NavLink
              key={it.to}
              to={it.to}
              end={it.to === "/"}
              className={({ isActive }) =>
                clsx(
                  "group relative flex items-center gap-2.5 rounded-lg px-3 py-2 text-sm transition-all",
                  isActive
                    ? "bg-accent/12 text-text"
                    : "text-muted hover:bg-panel2 hover:text-text",
                )
              }
            >
              {({ isActive }) => (
                <>
                  <span
                    className={clsx(
                      "absolute left-0 top-1/2 h-4 w-0.5 -translate-y-1/2 rounded-full transition-all",
                      isActive ? "bg-accent opacity-100" : "opacity-0",
                    )}
                    style={isActive ? { background: "var(--grad)" } : undefined}
                  />
                  <span className={isActive ? "text-accent" : "text-muted/70 group-hover:text-muted"}>
                    {it.icon}
                  </span>
                  {it.label}
                </>
              )}
            </NavLink>
          ))}
        </nav>
        <div className="space-y-1 border-t border-border p-2.5">
          {APP_VARIANT === "server" && (
            <button onClick={logout} className="btn-ghost w-full justify-start px-3">
              <LogOut size={16} /> Sign out
            </button>
          )}
          <div className="px-3 pb-1 pt-1 text-[10px] text-muted/50">
            {APP_VARIANT === "client" ? "loom-client" : "loom-server"} · dark
          </div>
        </div>
      </aside>
      <main className="min-w-0 flex-1 p-6 lg:p-8">
        <div className="page-enter mx-auto max-w-6xl">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
