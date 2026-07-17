import { useQuery } from "@tanstack/react-query";
import { ChevronLeft, ChevronRight, CircleAlert, FolderPlus } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";

import { formatAmountCents } from "../../lib/amount";
import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import type { AppError, CursorDto } from "../../types";
import "./Batches.css";

const PAGE_SIZE = 50;

function errorMessage(error: unknown) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return (error as AppError).message;
  }
  return "批次列表暂时无法加载";
}

function formatExportTime(value: string | null) {
  if (!value) return "尚未导出";
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? "导出时间未知"
    : new Intl.DateTimeFormat("zh-CN", {
        month: "numeric",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      }).format(date);
}

export function BatchListPage() {
  const [cursor, setCursor] = useState<CursorDto | undefined>();
  const [history, setHistory] = useState<Array<CursorDto | undefined>>([]);
  const page = { cursor, pageSize: PAGE_SIZE };
  const batchesQuery = useQuery({
    queryKey: queryKeys.batches(page),
    queryFn: () => api.listBatches(page),
  });

  return (
    <section className="batch-list-page" aria-labelledby="batch-list-title">
      <header className="batch-page-heading">
        <div>
          <h1 id="batch-list-title">报销批次</h1>
          <p>按批次核对归属、金额和导出状态。</p>
        </div>
        <Link className="button button-primary" to="/batches/new">
          <FolderPlus size={16} strokeWidth={1.7} aria-hidden="true" />
          新建批次
        </Link>
      </header>

      <div className="batch-list-region">
        <div className="batch-list-columns" aria-hidden="true">
          <span>批次</span>
          <span>日期范围</span>
          <span>状态</span>
          <span>票据</span>
          <span>金额</span>
          <span>待确认</span>
          <span>最近导出</span>
        </div>

        {batchesQuery.isPending ? (
          <div className="batch-list-state" role="status" aria-label="正在加载批次">
            <span>正在加载批次</span>
          </div>
        ) : batchesQuery.isError ? (
          <div className="batch-list-state is-error" role="alert">
            <CircleAlert size={18} strokeWidth={1.7} aria-hidden="true" />
            <span>{errorMessage(batchesQuery.error)}</span>
            <button
              type="button"
              className="button button-secondary"
              onClick={() => void batchesQuery.refetch()}
            >
              重试
            </button>
          </div>
        ) : batchesQuery.data.items.length === 0 ? (
          <div className="batch-list-state">
            <strong>{history.length ? "没有更多批次" : "尚未创建批次"}</strong>
            {!history.length ? <Link to="/batches/new">创建第一个批次</Link> : null}
          </div>
        ) : (
          <div className="batch-list-items">
            {batchesQuery.data.items.map((batch) => (
              <Link className="batch-workspace-row" to={`/batches/${batch.id}`} key={batch.id}>
                <strong title={batch.name}>{batch.name}</strong>
                <span>{batch.startDate} 至 {batch.endDate}</span>
                <span className="batch-status" data-status={batch.status}>
                  {batch.status === "exported" ? "已导出" : "草稿"}
                </span>
                <span>{batch.itemCount} 张</span>
                <span className="batch-amount">¥{formatAmountCents(batch.totalAmountCents)}</span>
                <span className={batch.unconfirmedCount ? "is-warning" : undefined}>
                  {batch.unconfirmedCount}
                </span>
                <span>{formatExportTime(batch.lastExportedAt)}</span>
              </Link>
            ))}
          </div>
        )}
      </div>

      <nav className="batch-pagination" aria-label="批次分页">
        <button
          type="button"
          className="button button-secondary"
          disabled={history.length === 0 || batchesQuery.isFetching}
          onClick={() => {
            const previous = history.at(-1);
            setHistory((value) => value.slice(0, -1));
            setCursor(previous);
          }}
        >
          <ChevronLeft size={15} strokeWidth={1.7} aria-hidden="true" />
          上一页
        </button>
        <span>第 {history.length + 1} 页</span>
        <button
          type="button"
          className="button button-secondary"
          disabled={!batchesQuery.data?.nextCursor || batchesQuery.isFetching}
          onClick={() => {
            const nextCursor = batchesQuery.data?.nextCursor;
            if (!nextCursor) return;
            setHistory((value) => [...value, cursor]);
            setCursor(nextCursor);
          }}
        >
          下一页
          <ChevronRight size={15} strokeWidth={1.7} aria-hidden="true" />
        </button>
      </nav>
    </section>
  );
}
