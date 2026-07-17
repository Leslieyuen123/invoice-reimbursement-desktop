import { useMutation, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, KeyRound, RefreshCw } from "lucide-react";
import { Link } from "react-router-dom";

import { api } from "../lib/api";
import { queryKeys } from "../lib/queryKeys";
import type { MailboxAccountDto } from "../types";

function formatFailureTime(value: string | null) {
  if (!value) return "时间未知";
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return "时间未知";
  return new Intl.DateTimeFormat("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(date);
}

export function AppErrorBanner({ accounts }: { accounts: MailboxAccountDto[] }) {
  const queryClient = useQueryClient();
  const retryMutation = useMutation({
    mutationKey: ["sync-accounts", "manual-retry"],
    mutationFn: (accountId: string) => api.syncAccountNow(accountId),
    onSettled: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: queryKeys.dashboard }),
        queryClient.invalidateQueries({ queryKey: queryKeys.mailboxAccounts }),
      ]);
    },
  });
  const failedAccounts = accounts.filter(
    (account) => account.lastError && account.lastErrorKind,
  );

  if (failedAccounts.length === 0) return null;

  return (
    <div className="app-error-banners">
      {failedAccounts.map((account) => {
        const isNetwork = account.lastErrorKind === "network";
        const isRetrying =
          retryMutation.isPending && retryMutation.variables === account.id;
        return (
          <div
            className="app-error-banner"
            data-kind={account.lastErrorKind}
            role="alert"
            aria-label={isNetwork ? "邮箱网络连接失败" : "邮箱授权失效"}
            key={account.id}
          >
            {isNetwork ? (
              <AlertTriangle size={17} aria-hidden="true" />
            ) : (
              <KeyRound size={17} aria-hidden="true" />
            )}
            <div className="app-error-copy">
              <strong>{isNetwork ? "邮箱网络连接失败" : "邮箱授权失效"}</strong>
              <span>
                {account.email}
                {isNetwork
                  ? `，上次失败 ${formatFailureTime(account.lastErrorAt)}`
                  : "，请更新授权信息后恢复同步"}
              </span>
            </div>
            {isNetwork ? (
              <button
                className="button button-secondary"
                type="button"
                disabled={isRetrying}
                onClick={() => retryMutation.mutate(account.id)}
              >
                <RefreshCw size={15} aria-hidden="true" />
                {isRetrying ? "正在重试" : "立即重试"}
              </button>
            ) : (
              <Link
                className="button button-secondary"
                to={`/settings?account=${encodeURIComponent(account.id)}`}
              >
                前往账号设置
              </Link>
            )}
          </div>
        );
      })}
    </div>
  );
}
