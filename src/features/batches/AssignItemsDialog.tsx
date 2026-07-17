import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronLeft, ChevronRight, CircleAlert, Search, X } from "lucide-react";
import {
  type FormEvent,
  type KeyboardEvent,
  useEffect,
  useRef,
  useState,
} from "react";

import { formatAmountCents } from "../../lib/amount";
import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import type {
  AppError,
  BatchDetailDto,
  CursorDto,
} from "../../types";

const PAGE_SIZE = 50;

interface AssignItemsDialogProps {
  batchId: string;
  sessionId: number;
  routeSessionId: number;
  onContentCommitted: (routeSessionId: number) => void;
  onAssigned: (detail: BatchDetailDto, sessionId: number) => void;
  onClose: () => void;
}

function errorMessage(error: unknown, fallback: string) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return (error as AppError).message;
  }
  return fallback;
}

function disabledCopy(reason: "recognition_failed" | "suspected_duplicate" | null) {
  if (reason === "recognition_failed") return "识别失败，不能归属";
  if (reason === "suspected_duplicate") return "疑似重复，不能归属";
  return null;
}

export function AssignItemsDialog({
  batchId,
  sessionId,
  routeSessionId,
  onContentCommitted,
  onAssigned,
  onClose,
}: AssignItemsDialogProps) {
  const queryClient = useQueryClient();
  const dialogRef = useRef<HTMLDivElement>(null);
  const mounted = useRef(true);
  const [searchInput, setSearchInput] = useState("");
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState<CursorDto | undefined>();
  const [history, setHistory] = useState<Array<CursorDto | undefined>>([]);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [assigning, setAssigning] = useState(false);
  const [assignError, setAssignError] = useState<string | null>(null);
  const page = { cursor, pageSize: PAGE_SIZE };
  const candidatesQuery = useQuery({
    queryKey: [...queryKeys.batchCandidates(batchId, query, page), sessionId],
    queryFn: () => api.listBatchCandidates(batchId, query || undefined, page),
  });

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  function submitSearch(event: FormEvent) {
    event.preventDefault();
    setQuery(searchInput.trim());
    setCursor(undefined);
    setHistory([]);
    setSelectedIds(new Set());
    setAssignError(null);
  }

  function handleDialogKeys(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
      return;
    }
    if (event.key !== "Tab" || !dialogRef.current) return;
    const focusable = Array.from(
      dialogRef.current.querySelectorAll<HTMLElement>(
        'button:not(:disabled), input:not(:disabled), [href], select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])',
      ),
    );
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last?.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  async function assignSelected() {
    if (selectedIds.size === 0) return;
    setAssigning(true);
    setAssignError(null);
    try {
      const detail = await api.assignItemsToBatch(batchId, [...selectedIds]);
      void queryClient.invalidateQueries({
        queryKey: queryKeys.batch(batchId),
        exact: true,
      });
      void queryClient.invalidateQueries({
        queryKey: queryKeys.batchCandidateLists(batchId),
      });
      void queryClient.invalidateQueries({ queryKey: queryKeys.batchLists });
      void queryClient.invalidateQueries({ queryKey: queryKeys.dashboard });
      void queryClient.invalidateQueries({ queryKey: queryKeys.itemLists });
      onContentCommitted(routeSessionId);
      if (!mounted.current) return;
      onAssigned(detail, sessionId);
    } catch (error) {
      if (mounted.current) {
        setAssignError(errorMessage(error, "票据归属失败"));
      }
    } finally {
      if (mounted.current) setAssigning(false);
    }
  }

  return (
    <div className="batch-dialog-backdrop" role="presentation">
      <div
        ref={dialogRef}
        className="batch-assign-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="assign-dialog-title"
        onKeyDown={handleDialogKeys}
      >
        <header className="batch-dialog-header">
          <div>
            <h2 id="assign-dialog-title">调整票据归属</h2>
            <p>默认显示批次日期范围内尚未归属的票据。</p>
          </div>
          <button
            type="button"
            className="icon-button"
            aria-label="关闭调整票据"
            onClick={onClose}
          >
            <X size={17} strokeWidth={1.7} aria-hidden="true" />
          </button>
        </header>

        <form className="batch-candidate-search" onSubmit={submitSearch}>
          <label>
            <span>搜索未归属票据</span>
            <div>
              <Search size={15} strokeWidth={1.7} aria-hidden="true" />
              <input
                autoFocus
                value={searchInput}
                onChange={(event) => setSearchInput(event.target.value)}
              />
            </div>
          </label>
          <button type="submit" className="button button-secondary">搜索</button>
        </form>

        <div className="batch-candidate-list" aria-live="polite">
          {candidatesQuery.isPending ? (
            <div className="batch-candidate-state" role="status">正在加载候选票据</div>
          ) : candidatesQuery.isError ? (
            <div className="batch-candidate-state is-error" role="alert">
              <CircleAlert size={17} strokeWidth={1.7} aria-hidden="true" />
              <span>{errorMessage(candidatesQuery.error, "候选票据暂时无法加载")}</span>
              <button
                type="button"
                className="button button-secondary"
                onClick={() => void candidatesQuery.refetch()}
              >
                重试
              </button>
            </div>
          ) : candidatesQuery.data.items.length === 0 ? (
            <div className="batch-candidate-state">
              {query ? "没有匹配的未归属票据" : "当前日期范围内没有未归属票据"}
            </div>
          ) : (
            candidatesQuery.data.items.map((candidate) => {
              const item = candidate.item;
              const reason = disabledCopy(candidate.disabledReason);
              const warnings = [
                candidate.outsideBatchRange ? "日期超出批次范围" : null,
                reason,
              ].filter((warning): warning is string => warning !== null);
              const warningId = warnings.length
                ? `batch-candidate-warning-${item.id}`
                : undefined;
              return (
                <label className="batch-candidate-row" key={item.id}>
                  <input
                    type="checkbox"
                    aria-label={item.originalName}
                    aria-describedby={warningId}
                    disabled={!candidate.eligible}
                    checked={selectedIds.has(item.id)}
                    onChange={(event) => {
                      setSelectedIds((current) => {
                        const next = new Set(current);
                        if (event.target.checked) next.add(item.id);
                        else next.delete(item.id);
                        return next;
                      });
                    }}
                  />
                  <span className="batch-candidate-copy">
                    <strong>{item.originalName}</strong>
                    <span>{item.invoiceDate ?? "日期待补充"} · {item.company ?? "公司待补充"}</span>
                  </span>
                  <span className="batch-candidate-amount">
                    {item.amountCents === null
                      ? "金额待补充"
                      : `¥${formatAmountCents(item.amountCents)}`}
                  </span>
                  <span id={warningId} className="batch-candidate-warning">
                    {warnings.join("；")}
                  </span>
                </label>
              );
            })
          )}
        </div>

        <footer className="batch-dialog-footer">
          <div className="batch-candidate-pagination">
            <button
              type="button"
              className="icon-button"
              aria-label="候选票据上一页"
              disabled={history.length === 0 || candidatesQuery.isFetching}
              onClick={() => {
                const previous = history.at(-1);
                setHistory((value) => value.slice(0, -1));
                setCursor(previous);
              }}
            >
              <ChevronLeft size={16} strokeWidth={1.7} aria-hidden="true" />
            </button>
            <span>第 {history.length + 1} 页</span>
            <button
              type="button"
              className="icon-button"
              aria-label="候选票据下一页"
              disabled={!candidatesQuery.data?.nextCursor || candidatesQuery.isFetching}
              onClick={() => {
                const nextCursor = candidatesQuery.data?.nextCursor;
                if (!nextCursor) return;
                setHistory((value) => [...value, cursor]);
                setCursor(nextCursor);
              }}
            >
              <ChevronRight size={16} strokeWidth={1.7} aria-hidden="true" />
            </button>
          </div>
          <div className="batch-dialog-submit">
            <span role={assignError ? "alert" : undefined}>{assignError}</span>
            <button type="button" className="button button-secondary" onClick={onClose}>
              取消
            </button>
            <button
              type="button"
              className="button button-primary"
              disabled={selectedIds.size === 0 || assigning}
              onClick={() => void assignSelected()}
            >
              {assigning ? "正在归属" : "归属所选票据"}
            </button>
          </div>
        </footer>
      </div>
    </div>
  );
}
