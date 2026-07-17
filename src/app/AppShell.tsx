import { useIsMutating, useQuery } from "@tanstack/react-query";
import {
  FolderKanban,
  Inbox,
  LayoutDashboard,
  ReceiptText,
  Settings2,
} from "lucide-react";
import { NavLink, Outlet, useLocation } from "react-router-dom";

import { api } from "../lib/api";
import { queryKeys } from "../lib/queryKeys";
import { AppErrorBanner } from "../components/AppErrorBanner";
import "./AppShell.css";

const DASHBOARD_REFRESH_INTERVAL_MS = 15_000;

const navigation = [
  { to: "/", label: "控制台", icon: LayoutDashboard, end: true },
  { to: "/inbox", label: "待处理池", icon: Inbox, end: false },
  { to: "/batches", label: "报销批次", icon: FolderKanban, end: false },
  { to: "/settings", label: "设置", icon: Settings2, end: false },
] as const;

function pageTitle(pathname: string) {
  if (pathname.startsWith("/inbox")) return "待处理池";
  if (pathname.startsWith("/batches/new")) return "新建批次";
  if (pathname.startsWith("/batches/")) return "批次详情";
  if (pathname.startsWith("/batches")) return "报销批次";
  if (pathname.startsWith("/settings")) return "设置";
  return "控制台";
}

export function AppShell() {
  const location = useLocation();
  const dashboardQuery = useQuery({
    queryKey: queryKeys.dashboard,
    queryFn: api.getDashboard,
    refetchInterval: DASHBOARD_REFRESH_INTERVAL_MS,
    refetchIntervalInBackground: true,
  });
  const activeSyncs = useIsMutating({ mutationKey: ["sync-accounts"] });
  const requiresAttention =
    dashboardQuery.isError ||
    (dashboardQuery.data?.recognitionFailedCount ?? 0) > 0 ||
    dashboardQuery.data?.mailboxAccounts.some((account) => account.lastError) ===
      true;
  const synchronizationStatus =
    activeSyncs > 0 || dashboardQuery.isPending
      ? { label: "同步中", state: "syncing" }
      : requiresAttention
        ? { label: "需处理", state: "attention" }
        : { label: "正常", state: "healthy" };

  return (
    <div className="app-shell">
      <aside className="app-sidebar">
        <div className="app-identity" aria-label="发票报销">
          <span className="app-identity-mark" aria-hidden="true">
            <ReceiptText size={19} strokeWidth={1.7} />
          </span>
          <span className="app-identity-copy">
            <strong>发票报销</strong>
            <small>本地工作台</small>
          </span>
        </div>

        <nav className="app-navigation" aria-label="主导航">
          {navigation.map(({ to, label, icon: Icon, end }) => (
            <NavLink
              key={to}
              to={to}
              end={end}
              className={({ isActive }) =>
                `app-nav-link${isActive ? " is-active" : ""}`
              }
              aria-label={label}
              title={label}
            >
              <Icon size={18} strokeWidth={1.7} aria-hidden="true" />
              <span>{label}</span>
            </NavLink>
          ))}
        </nav>

        <div className="app-sidebar-footer">
          <span className="app-local-indicator" aria-hidden="true" />
          <span>仅存储于此 Mac</span>
        </div>
      </aside>

      <div className="app-workspace">
        <header className="app-topbar">
          <div>
            <span className="app-topbar-context">工作区</span>
            <span className="app-topbar-title">{pageTitle(location.pathname)}</span>
          </div>
          <div
            className="app-sync-health"
            data-state={synchronizationStatus.state}
            aria-label={`同步状态：${synchronizationStatus.label}`}
          >
            <span aria-hidden="true" />
            {synchronizationStatus.label}
          </div>
        </header>
        <div className="app-error-slot">
          <AppErrorBanner accounts={dashboardQuery.data?.mailboxAccounts ?? []} />
        </div>
        <main className="app-main">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
