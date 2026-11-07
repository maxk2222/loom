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
    } catch { /* ignore */ }
    nav("/login");
  };

  return (
    <div className="flex min-h-screen">
      <aside className="w-52 shrink-0 border-r border-border bg-panel p-3">
        <div className="mb-6 flex items-center gap-2 px-1 pt-1">
          {brand.logo_path ? (
            <img src={brand.logo_path} alt="" className="h-7 w-7 rounded object-contain" />
          ) : (
            <div className="flex h-7 w-7 items-center justify-center rounded bg-accent text-sm font-bold text-white">
              {brand.app_name.slice(0, 1)}
            </div>
          )}
          <div>
            <div className="text-sm font-semibold leading-tight">{brand.app_name}</div>
            <div className="text-[10px] text-muted leading-tight">{brand.tagline}</div>
          </div>
        </div>
        <nav className="space-y-0.5">
          {items.map((it) => (
            <NavLink
              key={it.to}
              to={it.to}
              end={it.to === "/"}
              className={({ isActive }) =>
                clsx(
                  "flex items-center gap-2.5 rounded-md px-3 py-2 text-sm transition-colors",
                  isActive ? "bg-accent/15 text-accent" : "text-muted hover:bg-panel2 hover:text-text",
                )
              }
            >
              {it.icon}
              {it.label}
            </NavLink>
          ))}
        </nav>
        {APP_VARIANT === "server" && (
          <button onClick={logout} className="btn-ghost mt-6 w-full justify-start px-3">
            <LogOut size={16} /> Sign out
          </button>
        )}
      </aside>
      <main className="min-w-0 flex-1 p-6">
        <Outlet />
      </main>
    </div>
  );
}
