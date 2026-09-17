import type { InvoiceItemDto } from "../../types";
import { formatAmountCents } from "../../lib/amount";
import { formatLocalDateTime } from "../../lib/datetime";
import { systemNote } from "../../lib/systemNote";

interface InboxTableProps {
  items: InvoiceItemDto[];
  selectedItemId: string | null;
  state: "loading" | "ready" | "error";
  errorMessage?: string;
  onSelect: (item: InvoiceItemDto, opener: HTMLButtonElement) => void;
  onRetry: () => void;
}

const categoryLabels = {
  transport: "交通",
  dining: "餐饮",
  accommodation: "住宿",
  hospitality: "招待",
} as const;

const sourceLabels = {
  email: "邮箱",
  manual_upload: "手动上传",
} as const;

const statusLabels = {
  pending_recognition: "待识别",
  pending_confirmation: "待确认",
  recognition_failed: "识别失败",
  suspected_duplicate: "疑似重复",
  ready: "可纳入批次",
} as const;

/** The machine reason behind a failed import, shown next to the status. */
function reasonChip(note: string | null) {
  const reason = systemNote(note);
  if (!reason) {
    return null;
  }
  return (
    <span className="item-status-note" data-reason="true" title={reason}>
      {reason.length > 14 ? `${reason.slice(0, 14)}…` : reason}
    </span>
  );
}

export function InboxTable({
  items,
  selectedItemId,
  state,
  errorMessage,
  onSelect,
  onRetry,
}: InboxTableProps) {
  return (
    <table className="inbox-table">
      <colgroup>
        <col className="inbox-col-name" />
        <col className="inbox-col-date" />
        <col className="inbox-col-period" />
        <col className="inbox-col-category" />
        <col className="inbox-col-amount" />
        <col className="inbox-col-source" />
        <col className="inbox-col-status" />
        <col className="inbox-col-batch" />
      </colgroup>
      <thead>
        <tr>
          <th scope="col" className="inbox-col-name">原件</th>
          <th scope="col" className="inbox-col-date">开票日期</th>
          <th scope="col" className="inbox-col-mail-date">邮件收到</th>
          <th scope="col">建议月份</th>
          <th scope="col">分类</th>
          <th scope="col">金额</th>
          <th scope="col">来源</th>
          <th scope="col">状态</th>
          <th scope="col">批次</th>
        </tr>
      </thead>
      <tbody>
        {state === "loading" ? (
          <tr className="inbox-state-row">
            <td colSpan={9}>
              <div role="status" aria-label="正在加载待处理池">
                正在加载票据
              </div>
            </td>
          </tr>
        ) : null}
        {state === "error" ? (
          <tr className="inbox-state-row">
            <td colSpan={9}>
              <div role="alert">
                <span>{errorMessage ?? "待处理池暂时无法加载"}</span>
                <button type="button" onClick={onRetry}>
                  重试
                </button>
              </div>
            </td>
          </tr>
        ) : null}
        {state === "ready" && items.length === 0 ? (
          <tr className="inbox-state-row">
            <td colSpan={9}>当前筛选下没有票据</td>
          </tr>
        ) : null}
        {items.map((item) => (
          <tr
            key={item.id}
            className={selectedItemId === item.id ? "is-selected" : undefined}
          >
            <td className="inbox-name-cell">
              <button
                type="button"
                data-inbox-item-id={item.id}
                onClick={(event) => onSelect(item, event.currentTarget)}
              >
                {item.originalName}
              </button>
            </td>
            <td>{item.invoiceDate ?? "未识别"}</td>
            <td title={item.fetchedAt}>
              {item.sourceType === "email"
                ? formatLocalDateTime(item.fetchedAt)
                : "手动导入"}
            </td>
            <td>{item.suggestedPeriod ?? "待确认"}</td>
            <td>
              {item.finalCategory || item.suggestedCategory
                ? categoryLabels[item.finalCategory ?? item.suggestedCategory!]
                : "待确认"}
            </td>
            <td>
              {item.amountCents === null
                ? "待确认"
                : `¥${formatAmountCents(item.amountCents)}`}
            </td>
            <td>{sourceLabels[item.sourceType]}</td>
            <td>
              <span className="item-status" data-status={item.status}>
                {statusLabels[item.status]}
              </span>
              {reasonChip(item.note)}
              {!item.hasNormalizedPdf ? (
                <span className="item-status-note" title="导出前需要归一化 PDF">
                  缺归一化 PDF
                </span>
              ) : null}
            </td>
            <td>{item.batchId ?? "未分配"}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
