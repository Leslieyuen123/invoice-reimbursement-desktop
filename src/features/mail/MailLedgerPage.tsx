import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronLeft, ChevronRight, MailSearch, RefreshCw } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";

import { formatAmountCents } from "../../lib/amount";
import { api } from "../../lib/api";
import { formatLocalDateTime } from "../../lib/datetime";
import { queryKeys } from "../../lib/queryKeys";
import type {
  MailLedgerCursorDto,
  MailLedgerEntryDto,
  MailLedgerFilter,
  MailOutcome,
} from "../../types";
import "./MailLedger.css";

const PAGE_SIZE = 50;

const outcomeLabels: Record<MailOutcome, string> = {
  imported: "已提取",
  partial: "部分提取",
  failed: "未提取",
  ignored: "无需处理",
};

const tabs = [
  { value: "attention", label: "需关注" },
  { value: "failed", label: "未提取" },
  { value: "partial", label: "部分提取" },
  { value: "imported", label: "已提取" },
  { value: "ignored", label: "无需处理" },
  { value: "all", label: "全部" },
] as const;

type TabValue = (typeof tabs)[number]["value"];

function filterFor(tab: TabValue, query: string): MailLedgerFilter {
  const filter: MailLedgerFilter = {};
  if (tab === "attention") filter.needsAttention = true;
  if (tab === "failed" || tab === "partial" || tab === "imported" || tab === "ignored") {
    filter.outcome = tab;
  }
  const trimmed = query.trim();
  if (trimmed) filter.query = trimmed;
  return filter;
}

function errorMessage(error: unknown) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return error.message;
  }
  return "邮件台账暂时无法加载";
}

/** One scanned mail: what it carried and what the app did with it. */
function MailRow({ entry }: { entry: MailLedgerEntryDto }) {
  const [expanded, setExpanded] = useState(false);
  const itemsQuery = useQuery({
    queryKey: queryKeys.items({
      sourceAccountId: entry.accountId,
      sourceUid: entry.uid,
    }),
    enabled: expanded,
    queryFn: () =>
      api.listItems(
        { sourceAccountId: entry.accountId, sourceUid: entry.uid },
        { pageSize: 50 },
      ),
  });
  const invoices = itemsQuery.data?.items ?? [];

  return (
    <>
      <tr>
        <td className="mail-subject-cell">
          <button
            type="button"
            className="mail-subject-button"
            aria-expanded={expanded}
            onClick={() => setExpanded((value) => !value)}
          >
            <ChevronDown
              size={14}
              strokeWidth={1.7}
              aria-hidden="true"
              className={expanded ? "is-expanded" : undefined}
            />
            <span>{entry.subject ?? "（无主题）"}</span>
          </button>
          {entry.reason ? <small className="mail-reason">{entry.reason}</small> : null}
        </td>
        <td title={entry.sender ?? undefined}>{entry.sender ?? "-"}</td>
        <td>{formatLocalDateTime(entry.receivedAt)}</td>
        <td className="mail-counts">
          {entry.candidateCount} 个
        </td>
        <td className="mail-counts">
          导入 {entry.importedCount} · 已有 {entry.existingCount} · 失败{" "}
          {entry.failedCount}
        </td>
        <td>
          <span className="mail-outcome" data-outcome={entry.outcome}>
            {outcomeLabels[entry.outcome]}
          </span>
        </td>
        <td>
          {entry.markedSeen ? (
            <span className="mail-seen">已读</span>
          ) : entry.seenMismatch ? (
            <span className="mail-seen-mismatch" title="发票已入库，但邮箱仍是未读">
              标记失败
            </span>
          ) : (
            <span className="mail-unseen">未读</span>
          )}
        </td>
      </tr>
      {expanded ? (
        <tr className="mail-detail-row">
          <td colSpan={7}>
            {itemsQuery.isPending ? (
              <p role="status">正在加载这封邮件的票据…</p>
            ) : itemsQuery.isError ? (
              <p role="alert">{errorMessage(itemsQuery.error)}</p>
            ) : invoices.length === 0 ? (
              <p>这封邮件当前没有对应的票据记录。</p>
            ) : (
              <ul className="mail-invoice-list">
                {invoices.map((item) => (
                  <li key={item.id}>
                    <span className="mail-invoice-name">{item.originalName}</span>
                    <span>{item.invoiceDate ?? "未识别日期"}</span>
                    <span>
                      {item.amountCents === null
                        ? "金额待补充"
                        : `¥${formatAmountCents(item.amountCents)}`}
                    </span>
                    <span className="mail-invoice-status">
                      {item.status === "ready"
                        ? "可纳入批次"
                        : item.status === "pending_confirmation"
                          ? "待确认"
                          : item.status === "pending_recognition"
                            ? "待识别"
                            : item.status === "recognition_failed"
                              ? "识别失败"
                              : "疑似重复"}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            <p className="mail-detail-hint">
              在待处理池按状态筛选即可打开这些票据
              <Link to="/inbox?status=pending_confirmation">待确认</Link>
              <Link to="/inbox?status=recognition_failed">识别失败</Link>
              <Link to="/inbox?status=suspected_duplicate">疑似重复</Link>
            </p>
          </td>
        </tr>
      ) : null}
    </>
  );
}

export function MailLedgerPage() {
  const [tab, setTab] = useState<TabValue>("attention");
  const [searchInput, setSearchInput] = useState("");
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState<MailLedgerCursorDto | undefined>();
  const [history, setHistory] = useState<Array<MailLedgerCursorDto | undefined>>([]);
  const filter = filterFor(tab, query);
  const page = { cursor, pageSize: PAGE_SIZE };
  const ledgerQuery = useQuery({
    queryKey: queryKeys.mailLedger(filter, page),
    queryFn: () => api.listMailLedger(filter, page),
  });
  const countsQuery = useQuery({
    queryKey: queryKeys.mailLedgerCounts,
    queryFn: api.getMailLedgerCounts,
  });

  return (
    <section className="mail-ledger-page" aria-labelledby="mail-ledger-title">
      <header className="batch-page-heading">
        <div>
          <p className="mail-ledger-breadcrumb">工作区 邮件台账</p>
          <h1 id="mail-ledger-title">邮件台账</h1>
          <p>每封扫描过的邮件、提取结果，以及是否已在邮箱标记为已读。</p>
        </div>
        <button
          type="button"
          className="button button-secondary"
          onClick={() => void ledgerQuery.refetch()}
        >
          <RefreshCw size={16} strokeWidth={1.7} aria-hidden="true" />
          刷新
        </button>
      </header>

      <div className="mail-ledger-summary">
        <div>
          <span>需关注</span>
          <strong className={countsQuery.data?.needsAttention ? "is-warning" : undefined}>
            {countsQuery.data?.needsAttention ?? 0} 封
          </strong>
        </div>
        <div>
          <span>已提取</span>
          <strong>{countsQuery.data?.imported ?? 0} 封</strong>
        </div>
        <div>
          <span>无需处理</span>
          <strong>{countsQuery.data?.ignored ?? 0} 封</strong>
        </div>
      </div>

      <div className="mail-ledger-toolbar">
        <div className="mail-ledger-tabs" role="tablist" aria-label="邮件状态筛选">
          {tabs.map(({ value, label }) => (
            <button
              key={value}
              type="button"
              role="tab"
              aria-selected={tab === value}
              className={tab === value ? "is-active" : undefined}
              onClick={() => {
                setTab(value);
                setCursor(undefined);
                setHistory([]);
              }}
            >
              {label}
            </button>
          ))}
        </div>
        <form
          className="mail-ledger-search"
          onSubmit={(event) => {
            event.preventDefault();
            setQuery(searchInput.trim());
            setCursor(undefined);
            setHistory([]);
          }}
        >
          <label>
            <span>搜索主题或发件人</span>
            <input
              type="search"
              value={searchInput}
              placeholder="例如 发票 或 billing@example.com"
              onChange={(event) => setSearchInput(event.target.value)}
            />
          </label>
          <button type="submit" className="button button-secondary">
            搜索
          </button>
        </form>
      </div>

      <div className="mail-ledger-table-region">
        <table className="mail-ledger-table">
          <thead>
            <tr>
              <th scope="col">邮件主题</th>
              <th scope="col">发件人</th>
              <th scope="col">邮件收到</th>
              <th scope="col">线索</th>
              <th scope="col">结果</th>
              <th scope="col">状态</th>
              <th scope="col">邮箱</th>
            </tr>
          </thead>
          <tbody>
            {ledgerQuery.isPending ? (
              <tr className="mail-ledger-state-row">
                <td colSpan={7}>
                  <div role="status" aria-label="正在加载邮件台账">
                    正在加载邮件台账
                  </div>
                </td>
              </tr>
            ) : null}
            {ledgerQuery.isError ? (
              <tr className="mail-ledger-state-row">
                <td colSpan={7}>
                  <div role="alert">
                    <span>{errorMessage(ledgerQuery.error)}</span>
                    <button
                      type="button"
                      onClick={() => void ledgerQuery.refetch()}
                    >
                      重试
                    </button>
                  </div>
                </td>
              </tr>
            ) : null}
            {ledgerQuery.data?.items.length === 0 ? (
              <tr className="mail-ledger-state-row">
                <td colSpan={7}>
                  <MailSearch size={18} strokeWidth={1.7} aria-hidden="true" />
                  <span>当前筛选下没有邮件记录</span>
                </td>
              </tr>
            ) : null}
            {ledgerQuery.data?.items.map((entry) => (
              <MailRow
                key={`${entry.accountId}-${entry.mailbox}-${entry.uid}`}
                entry={entry}
              />
            ))}
          </tbody>
        </table>
      </div>

      <div className="mail-ledger-pagination">
        <button
          type="button"
          className="button button-secondary"
          disabled={history.length === 0 || ledgerQuery.isFetching}
          onClick={() => {
            const previous = history.at(-1);
            setHistory((value) => value.slice(0, -1));
            setCursor(previous);
          }}
        >
          <ChevronLeft size={15} strokeWidth={1.7} aria-hidden="true" />
          上一页
        </button>
        <button
          type="button"
          className="button button-secondary"
          disabled={!ledgerQuery.data?.nextCursor || ledgerQuery.isFetching}
          onClick={() => {
            const next = ledgerQuery.data?.nextCursor;
            if (!next) return;
            setHistory((value) => [...value, cursor]);
            setCursor(next);
          }}
        >
          下一页
          <ChevronRight size={15} strokeWidth={1.7} aria-hidden="true" />
        </button>
      </div>
    </section>
  );
}
