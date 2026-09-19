import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowRight,
  CalendarPlus,
  CircleAlert,
  Clock3,
  FilePlus2,
  Mail,
  RefreshCw,
  ShieldCheck,
} from "lucide-react";
import { useState } from "react";
import { Link, useNavigate } from "react-router-dom";

import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import type { AppError, DashboardDto } from "../../types";

function describeError(error: unknown) {
  if (error instanceof Error) {
    return error.message;
  }
  return typeof error === "string" && error ? error : "巡检失败";
}

const queueDefinitions = [
  {
    label: "最近新增",
    count: (dashboard: DashboardDto) => dashboard.recentlyAddedCount,
    to: "/inbox?scope=recent",
  },
  {
    label: "待确认",
    count: (dashboard: DashboardDto) => dashboard.pendingConfirmationCount,
    to: "/inbox?status=pending_confirmation",
  },
  {
    label: "识别失败",
    count: (dashboard: DashboardDto) => dashboard.recognitionFailedCount,
    to: "/inbox?status=recognition_failed",
  },
  {
    label: "疑似重复",
    count: (dashboard: DashboardDto) => dashboard.suspectedDuplicateCount,
    to: "/inbox?status=suspected_duplicate",
  },
  {
    label: "邮件待关注",
    count: (dashboard: DashboardDto) => dashboard.mailNeedsAttentionCount,
    to: "/mail",
  },
] as const;

const dateTimeFormatter = new Intl.DateTimeFormat("zh-CN", {
  month: "numeric",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
});

const amountFormatter = new Intl.NumberFormat("zh-CN", {
  style: "currency",
  currency: "CNY",
});

function formatSyncTime(value: string | null) {
  if (!value) return "尚未同步";
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? "同步时间未知"
    : dateTimeFormatter.format(date);
}

function errorMessage(error: unknown) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof (error as { message?: unknown }).message === "string"
  ) {
    return (error as AppError).message;
  }
  return "控制台暂时无法加载";
}

function DashboardSkeleton() {
  return (
    <div
      className="dashboard-page dashboard-skeleton"
      role="status"
      aria-label="正在加载控制台"
    >
      <span className="sr-only">正在加载控制台</span>
      <div className="skeleton-line skeleton-title" />
      <div className="skeleton-metrics">
        {Array.from({ length: 4 }, (_, index) => (
          <div className="skeleton-metric" key={index} />
        ))}
      </div>
      <div className="skeleton-panel" />
      <div className="skeleton-panel skeleton-panel-short" />
    </div>
  );
}

export function DashboardPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [syncFeedback, setSyncFeedback] = useState<string | null>(null);
  const dashboardQuery = useQuery({
    queryKey: queryKeys.dashboard,
    queryFn: api.getDashboard,
  });

  const consistencyQuery = useQuery({
    queryKey: queryKeys.consistencyReport,
    queryFn: api.getConsistencyReport,
  });
  const enabledAccounts =
    dashboardQuery.data?.mailboxAccounts.filter((account) => account.enabled) ??
    [];
  const syncMutation = useMutation({
    mutationKey: ["sync-accounts"],
    mutationFn: async () => {
      const outcomes = await Promise.allSettled(
        enabledAccounts.map((account) => api.syncAccountNow(account.id)),
      );
      await queryClient.invalidateQueries({ queryKey: queryKeys.dashboard });
      const failed = outcomes.find(
        (outcome): outcome is PromiseRejectedResult =>
          outcome.status === "rejected",
      );
      if (failed) throw failed.reason;
    },
    onMutate: () => setSyncFeedback(null),
    onSuccess: () => setSyncFeedback("同步完成"),
    onError: (error) => setSyncFeedback(`同步失败：${errorMessage(error)}`),
  });

  if (dashboardQuery.isPending) return <DashboardSkeleton />;

  if (dashboardQuery.isError) {
    return (
      <section className="dashboard-page dashboard-error" role="alert">
        <CircleAlert size={22} strokeWidth={1.7} aria-hidden="true" />
        <div>
          <h1>控制台无法加载</h1>
          <p>{errorMessage(dashboardQuery.error)}</p>
        </div>
        <button
          className="button button-secondary"
          type="button"
          onClick={() => void dashboardQuery.refetch()}
        >
          <RefreshCw size={16} strokeWidth={1.7} aria-hidden="true" />
          重试
        </button>
      </section>
    );
  }

  const dashboard = dashboardQuery.data;

  return (
    <div className="dashboard-page">
      <header className="dashboard-heading">
        <div>
          <h1>控制台</h1>
          <p>票据状态、邮箱同步和报销批次</p>
        </div>
        <div className="dashboard-actions">
          <button
            className="button button-secondary"
            type="button"
            disabled={enabledAccounts.length === 0 || syncMutation.isPending}
            onClick={() => syncMutation.mutate()}
          >
            <RefreshCw size={16} strokeWidth={1.7} aria-hidden="true" />
            {syncMutation.isPending ? "同步中" : "立即同步"}
          </button>
          <button
            className="button button-primary"
            type="button"
            onClick={() => navigate("/batches/new?type=month")}
          >
            <FilePlus2 size={16} strokeWidth={1.7} aria-hidden="true" />
            新建批次
          </button>
        </div>
      </header>

      <div className="dashboard-feedback" aria-live="polite">
        {syncFeedback}
      </div>

      <section className="dashboard-section queue-section" aria-labelledby="queue-title">
        <div className="section-heading">
          <h2 id="queue-title">今日工作队列</h2>
          <span>点击计数进入对应筛选</span>
        </div>
        <div className="queue-grid">
          {queueDefinitions.map((queue) => {
            const count = queue.count(dashboard);
            return (
              <Link
                className="queue-metric"
                to={queue.to}
                key={queue.label}
                aria-label={`${queue.label} ${count}`}
              >
                <span>{queue.label}</span>
                <strong>{count}</strong>
                <ArrowRight size={15} strokeWidth={1.7} aria-hidden="true" />
              </Link>
            );
          })}
        </div>
      </section>

      <section className="dashboard-section" aria-labelledby="consistency-title">
        <div className="section-heading">
          <h2 id="consistency-title">数据一致性巡检</h2>
          <button
            className="button button-secondary"
            type="button"
            disabled={consistencyQuery.isFetching}
            onClick={() => void consistencyQuery.refetch()}
          >
            <RefreshCw size={15} aria-hidden="true" />
            {consistencyQuery.isFetching ? "正在巡检" : "重新巡检"}
          </button>
        </div>
        {consistencyQuery.isPending ? (
          <p className="dashboard-empty">正在检查原件、归一化 PDF 与批次状态…</p>
        ) : consistencyQuery.isError ? (
          <p className="dashboard-empty" role="alert">
            一致性巡检失败：{describeError(consistencyQuery.error)}
          </p>
        ) : consistencyQuery.data.issues.length === 0 ? (
          <p className="consistency-ok" role="status">
            <ShieldCheck size={16} aria-hidden="true" />
            {`已检查 ${consistencyQuery.data.itemsChecked} 张票据和 ${consistencyQuery.data.batchesChecked} 个批次：原件、归一化 PDF 与批次状态都一致。`}
          </p>
        ) : (
          <ul className="consistency-issues">
            {consistencyQuery.data.issues.map((issue) => (
              <li key={issue.key} data-issue={issue.key}>
                <div className="consistency-issue-heading">
                  <CircleAlert size={16} aria-hidden="true" />
                  <strong>{issue.label}</strong>
                  <span>{issue.count}</span>
                </div>
                <p>{issue.hint}</p>
                {issue.samples.length > 0 ? (
                  <p className="consistency-samples">
                    {issue.samples.join("、")}
                  </p>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      <div className="dashboard-columns">
        <section className="dashboard-section" aria-labelledby="mailbox-title">
          <div className="section-heading">
            <h2 id="mailbox-title">邮箱同步</h2>
            <span>{dashboard.mailboxAccounts.length} 个账户</span>
          </div>
          {dashboard.mailboxAccounts.length === 0 ? (
            <div className="dashboard-empty">
              <Mail size={21} strokeWidth={1.6} aria-hidden="true" />
              <strong>尚未连接邮箱</strong>
              <Link to="/settings">前往设置</Link>
            </div>
          ) : (
            <div className="mailbox-list">
              {dashboard.mailboxAccounts.map((account) => (
                <div className="mailbox-row" key={account.id}>
                  <span className="mailbox-icon" aria-hidden="true">
                    <Mail size={16} strokeWidth={1.6} />
                  </span>
                  <div className="mailbox-copy">
                    <strong>{account.email}</strong>
                    <span>
                      {account.provider === "gmail" ? "Gmail" : "QQ 邮箱"}
                    </span>
                  </div>
                  <div className="mailbox-meta">
                    <span
                      className={`status-label${account.lastError ? " is-error" : ""}`}
                    >
                      {account.lastError
                        ? "需处理"
                        : account.enabled
                          ? "已启用"
                          : "已暂停"}
                    </span>
                    <span>
                      <Clock3 size={13} strokeWidth={1.6} aria-hidden="true" />
                      {formatSyncTime(account.lastSyncedAt)}
                    </span>
                  </div>
                </div>
              ))}
            </div>
          )}
        </section>

        <section className="dashboard-section" aria-labelledby="batch-title">
          <div className="section-heading">
            <h2 id="batch-title">最近批次</h2>
            <Link to="/batches">查看全部</Link>
          </div>
          {dashboard.recentBatches.length === 0 ? (
            <div className="dashboard-empty">
              <CalendarPlus size={21} strokeWidth={1.6} aria-hidden="true" />
              <strong>还没有报销批次</strong>
              <Link to="/batches/new?type=month">新建月度批次</Link>
            </div>
          ) : (
            <div className="batch-list">
              {dashboard.recentBatches.map((batch) => (
                <Link
                  className="batch-row"
                  to={`/batches/${batch.id}`}
                  key={batch.id}
                >
                  <div>
                    <strong>{batch.name}</strong>
                    <span>
                      {batch.itemCount} 张票据，{batch.unconfirmedCount} 张待确认
                    </span>
                  </div>
                  <div>
                    <strong>
                      {amountFormatter.format(batch.totalAmountCents / 100)}
                    </strong>
                    <span>{batch.status === "draft" ? "草稿" : "已导出"}</span>
                  </div>
                  <ArrowRight size={15} strokeWidth={1.7} aria-hidden="true" />
                </Link>
              ))}
            </div>
          )}
          <div className="batch-entry-actions" aria-label="新建批次方式">
            <Link to="/batches/new?type=month">月度批次</Link>
            <Link to="/batches/new?type=custom">自定义批次</Link>
          </div>
        </section>
      </div>
    </div>
  );
}
