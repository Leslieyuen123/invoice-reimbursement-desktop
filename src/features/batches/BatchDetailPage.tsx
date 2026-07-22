import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { join } from "@tauri-apps/api/path";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  CircleAlert,
  CircleCheck,
  FileArchive,
  FolderOpen,
  LoaderCircle,
  PackagePlus,
  RefreshCw,
  Sparkles,
  SlidersHorizontal,
  Trash2,
} from "lucide-react";
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { Link, useLocation, useNavigate, useParams } from "react-router-dom";

import { formatAmountCents } from "../../lib/amount";
import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import type {
  AppError,
  BatchAutomationResultDto,
  BatchDetailDto,
  Category,
  CursorDto,
  ExportResultDto,
  InvoiceItemDto,
  ItemStatus,
} from "../../types";
import { AssignItemsDialog } from "./AssignItemsDialog";
import "./Batches.css";

const exportFiles = [
  "merged.pdf",
  "reimbursement.xlsx",
  "originals.zip",
  "manifest.json",
] as const;

const RECOMMEND_PAGE_SIZE = 200;
const MAX_RECOMMEND_PAGES = 5;
const MAX_AUTOMATION_ERROR_MESSAGE_LENGTH = 240;
const AUTOMATION_ERROR_FALLBACK = "自动处理失败，请稍后重试";

const categories = [
  ["交通", "transport"],
  ["餐饮", "dining"],
  ["住宿", "accommodation"],
  ["招待", "hospitality"],
] as const;

const categoryLabels: Record<Category, string> = {
  transport: "交通",
  dining: "餐饮",
  accommodation: "住宿",
  hospitality: "招待",
};

const statusLabels: Record<ItemStatus, string> = {
  pending_recognition: "待识别",
  pending_confirmation: "待确认",
  recognition_failed: "识别失败",
  suspected_duplicate: "疑似重复",
  ready: "可纳入批次",
};

type AutomationFeedback =
  | { status: "idle" }
  | { status: "pending" }
  | {
      status: "success";
      result: BatchAutomationResultDto;
      contentRevision: number;
    }
  | { status: "error"; message: string };

function categoryLabel(category: Category | null) {
  return category ? categoryLabels[category] : "待分类";
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

function isAppError(error: unknown): error is AppError {
  if (typeof error !== "object" || error === null) return false;
  const candidate = error as Record<string, unknown>;
  if (typeof candidate.message !== "string") return false;

  switch (candidate.code) {
    case "validation":
      return typeof candidate.field === "string";
    case "not_found":
      return typeof candidate.entity === "string";
    case "conflict":
    case "internal":
      return true;
    case "external":
      return (
        typeof candidate.service === "string" &&
        typeof candidate.retryable === "boolean"
      );
    default:
      return false;
  }
}

function sanitizeAutomationErrorMessage(message: string) {
  const withoutControls = Array.from(message, (character) => {
    const codePoint = character.codePointAt(0) ?? 0;
    return codePoint <= 0x1f || (codePoint >= 0x7f && codePoint <= 0x9f)
      ? " "
      : character;
  }).join("");
  const normalized = withoutControls.replace(/\s+/gu, " ").trim();
  return Array.from(normalized)
    .slice(0, MAX_AUTOMATION_ERROR_MESSAGE_LENGTH)
    .join("")
    .trimEnd();
}

function automationErrorMessage(error: unknown) {
  if (!isAppError(error)) return AUTOMATION_ERROR_FALLBACK;
  return sanitizeAutomationErrorMessage(error.message) || AUTOMATION_ERROR_FALLBACK;
}

export function BatchDetailPage() {
  const { batchId = "" } = useParams();
  const location = useLocation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const routeSession = useRef(0);
  const viewActive = useRef(false);
  const activeBatchId = useRef(batchId);
  const consumedAutomationLocations = useRef(new Set<string>());
  const runAutomationRef = useRef<(operationBatchId: string) => void>(() => undefined);
  const [automationFeedback, setAutomationFeedback] =
    useState<AutomationFeedback>({ status: "idle" });
  const [recommendPending, setRecommendPending] = useState(false);
  const [recommendError, setRecommendError] = useState<string | null>(null);
  const [exportPending, setExportPending] = useState(false);
  const [exportResult, setExportResult] = useState<ExportResultDto | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const [revealError, setRevealError] = useState<string | null>(null);
  const [assignDialogOpen, setAssignDialogOpen] = useState(false);
  const [assignDialogSession, setAssignDialogSession] = useState(0);
  const nextDialogSession = useRef(0);
  const assignDialogOpener = useRef<HTMLButtonElement | null>(null);
  const removeFocusFallbackRef = useRef<HTMLButtonElement | null>(null);
  const removeDialogRef = useRef<HTMLDivElement | null>(null);
  const removeCancelRef = useRef<HTMLButtonElement | null>(null);
  const nextRemoveDialogSession = useRef(0);
  const activeRemoveDialogSession = useRef<number | null>(null);
  const [removeTarget, setRemoveTarget] = useState<{
    item: InvoiceItemDto;
    opener: HTMLButtonElement;
    sessionId: number;
  } | null>(null);
  const [removePending, setRemovePending] = useState(false);
  const [removeError, setRemoveError] = useState<string | null>(null);
  const automationMutation = useMutation({
    mutationFn: (operationBatchId: string) =>
      api.runBatchAutomation(operationBatchId),
  });
  const batchQuery = useQuery({
    queryKey: queryKeys.batch(batchId),
    queryFn: () => api.getBatch(batchId),
    enabled: Boolean(batchId),
  });
  const { data: batchContentRevision } = useQuery({
    queryKey: queryKeys.batchContentRevision(batchId),
    queryFn: () => 0,
    initialData: 0,
    enabled: false,
  });

  useEffect(() => {
    viewActive.current = true;
    activeBatchId.current = batchId;
    routeSession.current += 1;
    setAutomationFeedback({ status: "idle" });
    setRecommendPending(false);
    setRecommendError(null);
    setExportPending(false);
    setExportResult(null);
    setExportError(null);
    setRevealError(null);
    setAssignDialogOpen(false);
    activeRemoveDialogSession.current = null;
    setRemoveTarget(null);
    setRemovePending(false);
    setRemoveError(null);
    return () => {
      viewActive.current = false;
      activeBatchId.current = "";
      routeSession.current += 1;
    };
  }, [batchId]);

  useEffect(() => {
    setExportPending(false);
    setExportResult(null);
    setExportError(null);
    setRevealError(null);
    setAutomationFeedback((feedback) =>
      feedback.status === "success" &&
      feedback.contentRevision !== batchContentRevision
        ? { status: "idle" }
        : feedback,
    );
  }, [batchContentRevision]);

  useEffect(() => {
    if (removePending) removeCancelRef.current?.focus();
  }, [removePending]);

  function reconcileBatchDetail(operationBatchId: string) {
    void queryClient.invalidateQueries({
      queryKey: queryKeys.batch(operationBatchId),
      exact: true,
    });
    void queryClient.invalidateQueries({
      queryKey: queryKeys.batchCandidateLists(operationBatchId),
    });
    void queryClient.invalidateQueries({ queryKey: queryKeys.batchLists });
    void queryClient.invalidateQueries({ queryKey: queryKeys.dashboard });
    void queryClient.invalidateQueries({ queryKey: queryKeys.itemLists });
  }

  function getBatchContentRevision(operationBatchId: string) {
    return queryClient.getQueryData<number>(
      queryKeys.batchContentRevision(operationBatchId),
    ) ?? 0;
  }

  function advanceBatchContentRevision(operationBatchId: string) {
    queryClient.setQueryData<number>(
      queryKeys.batchContentRevision(operationBatchId),
      (revision = 0) => revision + 1,
    );
  }

  async function runAutomation(operationBatchId: string) {
    const session = routeSession.current;
    setAutomationFeedback({ status: "pending" });
    try {
      const result = await automationMutation.mutateAsync(operationBatchId);
      reconcileBatchDetail(operationBatchId);
      advanceBatchContentRevision(operationBatchId);
      const contentRevision = getBatchContentRevision(operationBatchId);
      if (
        viewActive.current &&
        activeBatchId.current === operationBatchId &&
        session === routeSession.current
      ) {
        setAutomationFeedback({ status: "success", result, contentRevision });
      }
    } catch (error) {
      if (
        viewActive.current &&
        activeBatchId.current === operationBatchId &&
        session === routeSession.current
      ) {
        setAutomationFeedback({
          status: "error",
          message: automationErrorMessage(error),
        });
      }
    }
  }

  runAutomationRef.current = (operationBatchId: string) => {
    void runAutomation(operationBatchId);
  };

  useEffect(() => {
    const routeState = location.state as { runAutomation?: unknown } | null;
    if (routeState?.runAutomation !== true) return;
    if (consumedAutomationLocations.current.has(location.key)) return;
    consumedAutomationLocations.current.add(location.key);
    const operationBatchId = batchId;
    navigate(location.pathname, { replace: true, state: null });
    queueMicrotask(() => {
      if (
        viewActive.current &&
        activeBatchId.current === operationBatchId
      ) {
        runAutomationRef.current(operationBatchId);
      }
    });
  }, [batchId, location.key, location.pathname, location.state, navigate]);

  async function assignRecommendations() {
    const session = routeSession.current;
    const operationBatchId = batchId;
    setRecommendPending(true);
    setRecommendError(null);
    try {
      const itemIds: string[] = [];
      let cursor: CursorDto | undefined;
      for (let pageIndex = 0; pageIndex < MAX_RECOMMEND_PAGES; pageIndex += 1) {
        const page = await api.listBatchCandidates(operationBatchId, undefined, {
          cursor,
          pageSize: RECOMMEND_PAGE_SIZE,
        });
        if (session !== routeSession.current) return;
        itemIds.push(
          ...page.items
            .filter((candidate) => candidate.eligible)
            .map((candidate) => candidate.item.id),
        );
        if (!page.nextCursor) break;
        if (pageIndex === MAX_RECOMMEND_PAGES - 1) {
          setRecommendError(
            "候选票据超过 1000 张，请使用“调整票据”分批归属",
          );
          return;
        }
        cursor = page.nextCursor;
      }
      if (itemIds.length === 0) {
        setRecommendError("当前日期范围内没有可加入的推荐票据");
        return;
      }
      await api.assignItemsToBatch(operationBatchId, itemIds);
      reconcileBatchDetail(operationBatchId);
      advanceBatchContentRevision(operationBatchId);
      if (session !== routeSession.current) return;
    } catch (error) {
      if (session === routeSession.current) {
        setRecommendError(errorMessage(error, "推荐票据加入失败"));
      }
    } finally {
      if (session === routeSession.current) setRecommendPending(false);
    }
  }

  async function runExport() {
    const session = routeSession.current;
    const operationBatchId = batchId;
    const operationContentRevision = getBatchContentRevision(operationBatchId);
    setExportPending(true);
    setExportError(null);
    setRevealError(null);
    setExportResult(null);
    try {
      const result = await api.exportBatch(operationBatchId);
      void queryClient.invalidateQueries({
        queryKey: queryKeys.batch(operationBatchId),
      });
      void queryClient.invalidateQueries({ queryKey: queryKeys.batchLists });
      void queryClient.invalidateQueries({ queryKey: queryKeys.dashboard });
      void queryClient.invalidateQueries({ queryKey: queryKeys.itemLists });
      if (
        session !== routeSession.current ||
        operationContentRevision !== getBatchContentRevision(operationBatchId)
      ) return;
      setExportResult(result);
    } catch (error) {
      if (
        session === routeSession.current &&
        operationContentRevision === getBatchContentRevision(operationBatchId)
      ) {
        setExportError(errorMessage(error, "报销包导出失败"));
      }
    } finally {
      if (
        session === routeSession.current &&
        operationContentRevision === getBatchContentRevision(operationBatchId)
      ) {
        setExportPending(false);
      }
    }
  }

  async function revealExport() {
    if (!exportResult) return;
    const session = routeSession.current;
    const operationBatchId = batchId;
    const operationContentRevision = getBatchContentRevision(operationBatchId);
    setRevealError(null);
    try {
      const mergedPdf = await join(exportResult.directory, "merged.pdf");
      await revealItemInDir(mergedPdf);
    } catch {
      if (
        session === routeSession.current &&
        operationContentRevision === getBatchContentRevision(operationBatchId)
      ) {
        setRevealError("无法在文件夹中显示，请从导出目录手动打开报销包");
      }
    }
  }

  function closeAssignDialog() {
    setAssignDialogOpen(false);
    const opener = assignDialogOpener.current;
    queueMicrotask(() => opener?.focus());
  }

  function acceptAssignedDetail(_detail: BatchDetailDto, sessionId: number) {
    if (sessionId !== assignDialogSession) return;
    closeAssignDialog();
  }

  function closeRemoveDialog(preferFallback = false) {
    if (!removeTarget) return;
    const { opener, sessionId } = removeTarget;
    if (activeRemoveDialogSession.current !== sessionId) return;
    activeRemoveDialogSession.current = null;
    setRemoveTarget(null);
    queueMicrotask(() => {
      if (!preferFallback && opener.isConnected) {
        opener.focus();
      } else {
        removeFocusFallbackRef.current?.focus();
      }
    });
  }

  function handleRemoveDialogKeys(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === "Escape") {
      event.preventDefault();
      if (!removePending) closeRemoveDialog();
      return;
    }
    if (event.key !== "Tab" || !removeDialogRef.current) return;
    const focusable = Array.from(
      removeDialogRef.current.querySelectorAll<HTMLElement>(
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

  async function confirmRemoval() {
    if (!removeTarget) return;
    const session = routeSession.current;
    const operationBatchId = batchId;
    const { item, sessionId: removeDialogSession } = removeTarget;
    setRemovePending(true);
    setRemoveError(null);
    try {
      await api.removeItemFromBatch(operationBatchId, item.id);
      reconcileBatchDetail(operationBatchId);
      advanceBatchContentRevision(operationBatchId);
      if (session !== routeSession.current) return;
      closeRemoveDialog(true);
    } catch (error) {
      if (
        session === routeSession.current &&
        removeDialogSession === activeRemoveDialogSession.current
      ) {
        setRemoveError(errorMessage(error, "票据移出失败"));
      }
    } finally {
      if (session === routeSession.current) setRemovePending(false);
    }
  }

  if (batchQuery.isPending) {
    return (
      <div className="batch-detail-page batch-detail-loading" role="status">
        正在加载批次详情
      </div>
    );
  }

  if (batchQuery.isError) {
    return (
      <section className="batch-detail-page batch-detail-error" role="alert">
        <CircleAlert size={20} strokeWidth={1.7} aria-hidden="true" />
        <div>
          <h1>批次详情无法加载</h1>
          <p>{errorMessage(batchQuery.error, "批次详情暂时无法加载")}</p>
        </div>
        <button
          type="button"
          className="button button-secondary"
          onClick={() => void batchQuery.refetch()}
        >
          <RefreshCw size={15} strokeWidth={1.7} aria-hidden="true" />
          重试
        </button>
      </section>
    );
  }

  const detail = batchQuery.data;
  const exportBlocked = detail.summary.unconfirmedCount > 0;

  return (
    <section className="batch-detail-page" aria-labelledby="batch-detail-title">
      <header className="batch-page-heading batch-detail-heading">
        <div>
          <Link className="batch-back-link" to="/batches">报销批次</Link>
          <h1 id="batch-detail-title">{detail.batch.name}</h1>
          <p>{detail.batch.startDate} 至 {detail.batch.endDate}</p>
        </div>
        <div className="batch-detail-actions">
          <button
            ref={removeFocusFallbackRef}
            type="button"
            className="button button-secondary"
            onClick={(event) => {
              assignDialogOpener.current = event.currentTarget;
              nextDialogSession.current += 1;
              setAssignDialogSession(nextDialogSession.current);
              setAssignDialogOpen(true);
            }}
          >
            <SlidersHorizontal size={16} strokeWidth={1.7} aria-hidden="true" />
            调整票据
          </button>
          <button
            type="button"
            className="button button-secondary"
            disabled={recommendPending}
            onClick={() => void assignRecommendations()}
          >
            <PackagePlus size={16} strokeWidth={1.7} aria-hidden="true" />
            {recommendPending ? "正在加入" : "加入推荐票据"}
          </button>
          <button
            type="button"
            className="button button-primary"
            disabled={exportBlocked || exportPending}
            onClick={() => void runExport()}
          >
            <FileArchive size={16} strokeWidth={1.7} aria-hidden="true" />
            {exportPending ? "正在生成报销文件" : "导出报销包"}
          </button>
        </div>
      </header>

      {recommendError ? (
        <div className="batch-inline-error" role="alert">{recommendError}</div>
      ) : null}

      <section
        className={`batch-automation-strip is-${automationFeedback.status}`}
        aria-labelledby="batch-automation-title"
      >
        {automationFeedback.status === "idle" ? (
          <div className="batch-automation-copy">
            <div className="batch-automation-title">
              <Sparkles size={17} strokeWidth={1.7} aria-hidden="true" />
              <h2 id="batch-automation-title">自动处理</h2>
            </div>
            <p>同步该日期范围的邮箱票据，纳入安全项并生成报销包。</p>
          </div>
        ) : automationFeedback.status === "pending" ? (
          <div
            className="batch-automation-copy"
            role="status"
            aria-label="正在自动处理批次"
          >
            <div className="batch-automation-title">
              <LoaderCircle
                className="batch-automation-spinner"
                size={17}
                strokeWidth={1.7}
                aria-hidden="true"
              />
              <h2 id="batch-automation-title">正在自动处理</h2>
            </div>
            <p>正在同步邮箱、整理票据并准备导出。</p>
          </div>
        ) : automationFeedback.status === "success" ? (
          <div
            className="batch-automation-copy"
            role="status"
            aria-live="polite"
            aria-label="批次自动处理完成"
          >
            <div className="batch-automation-title">
              <CircleCheck size={17} strokeWidth={1.7} aria-hidden="true" />
              <h2 id="batch-automation-title">自动处理完成</h2>
            </div>
            <ul className="batch-automation-counts">
              <li>{automationFeedback.result.importedCount} 张新导入</li>
              <li>{automationFeedback.result.assignedCount} 张已自动纳入</li>
              <li>{automationFeedback.result.exceptionCount} 个异常项</li>
              <li>{automationFeedback.result.failedAccounts.length} 个邮箱失败</li>
            </ul>
            {automationFeedback.result.export ? (
              <p className="batch-automation-directory">
                <span>导出目录</span>
                <code>{automationFeedback.result.export.directory}</code>
              </p>
            ) : (
              <p className="batch-automation-empty">
                没有可导出的票据，本次未生成报销包
              </p>
            )}
          </div>
        ) : (
          <div className="batch-automation-copy" role="alert">
            <div className="batch-automation-title">
              <CircleAlert size={17} strokeWidth={1.7} aria-hidden="true" />
              <h2 id="batch-automation-title">自动处理未完成</h2>
            </div>
            <p>{automationFeedback.message}</p>
          </div>
        )}
        <div className="batch-automation-action">
          <button
            type="button"
            className="button button-secondary"
            disabled={automationFeedback.status === "pending"}
            onClick={() => void runAutomation(batchId)}
          >
            <Sparkles size={15} strokeWidth={1.7} aria-hidden="true" />
            {automationFeedback.status === "pending"
              ? "正在自动处理"
              : automationFeedback.status === "error"
                ? "重试自动处理"
                : automationFeedback.status === "success"
                  ? "再次自动处理"
                  : "一键自动处理"}
          </button>
        </div>
      </section>

      <div className="batch-summary-strip">
        <div>
          <span>票据</span>
          <strong>{detail.summary.itemCount} 张</strong>
        </div>
        <div>
          <span>合计</span>
          <strong>¥{formatAmountCents(detail.summary.totalAmountCents)}</strong>
        </div>
        <div>
          <span>待确认</span>
          <strong className={exportBlocked ? "is-warning" : undefined}>
            {detail.summary.unconfirmedCount}
          </strong>
        </div>
        <div>
          <span>导出状态</span>
          <strong>{detail.batch.status === "exported" ? "已导出" : "未导出"}</strong>
        </div>
      </div>

      <section className="batch-category-section" aria-labelledby="category-summary-title">
        <h2 id="category-summary-title">分类汇总</h2>
        <div className="batch-category-grid">
          {categories.map(([label, key]) => (
            <div key={key}>
              <span>{label}</span>
              <strong>¥{formatAmountCents(detail.summary[key].amountCents)}</strong>
              <small>{detail.summary[key].itemCount} 张</small>
            </div>
          ))}
        </div>
      </section>

      <section className="batch-items-section" aria-labelledby="assigned-items-title">
        <div className="batch-section-heading">
          <h2 id="assigned-items-title">已归属票据</h2>
          <span>{detail.items.length} 张</span>
        </div>
        <div className="batch-items-table-region">
          <table className="batch-items-table">
            <thead>
              <tr>
                <th>文件</th>
                <th>日期</th>
                <th>分类</th>
                <th>状态</th>
                <th>公司</th>
                <th>金额</th>
                <th aria-label="操作" />
              </tr>
            </thead>
            <tbody>
              {detail.items.length === 0 ? (
                <tr>
                  <td colSpan={7} className="batch-empty-row">尚未归属票据</td>
                </tr>
              ) : (
                detail.items.map((item) => (
                  <tr key={item.id}>
                    <td title={item.originalName}>{item.originalName}</td>
                    <td>{item.invoiceDate ?? "日期待补充"}</td>
                    <td>{categoryLabel(item.finalCategory ?? item.suggestedCategory)}</td>
                    <td>{statusLabels[item.status]}</td>
                    <td title={item.company ?? undefined}>{item.company ?? "-"}</td>
                    <td>
                      {item.amountCents === null
                        ? "金额待补充"
                        : `¥${formatAmountCents(item.amountCents)}`}
                    </td>
                    <td>
                      <button
                        type="button"
                        className="icon-button batch-remove-button"
                        aria-label={`移出 ${item.originalName}`}
                        title="移出票据"
                        onClick={(event) => {
                          nextRemoveDialogSession.current += 1;
                          activeRemoveDialogSession.current =
                            nextRemoveDialogSession.current;
                          setRemoveError(null);
                          setRemoveTarget({
                            item,
                            opener: event.currentTarget,
                            sessionId: nextRemoveDialogSession.current,
                          });
                        }}
                      >
                        <Trash2 size={15} strokeWidth={1.7} aria-hidden="true" />
                      </button>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </section>

      <section className="batch-export-section" aria-labelledby="batch-export-title">
        <div className="batch-section-heading">
          <h2 id="batch-export-title">导出</h2>
        </div>
        {exportBlocked ? (
          <div className="batch-export-blocked">
            <CircleAlert size={17} strokeWidth={1.7} aria-hidden="true" />
            <span>还有 {detail.summary.unconfirmedCount} 张票据待确认，确认后才能导出。</span>
            <Link to={`/inbox?status=pending_confirmation&batchId=${batchId}`}>
              查看待确认票据
            </Link>
          </div>
        ) : null}
        {exportError ? (
          <div className="batch-export-error" role="alert">
            <span>{exportError}</span>
            <button type="button" className="button button-secondary" onClick={() => void runExport()}>
              重试导出
            </button>
          </div>
        ) : null}
        {exportResult ? (
          <div className="batch-export-result" role="status">
            <div className="batch-export-result-heading">
              <div>
                <strong>报销包已生成</strong>
                <span>{exportResult.itemCount} 张，合计 ¥{formatAmountCents(exportResult.totalAmountCents)}</span>
              </div>
              <button
                type="button"
                className="button button-secondary"
                onClick={() => void revealExport()}
              >
                <FolderOpen size={16} strokeWidth={1.7} aria-hidden="true" />
                在文件夹中显示
              </button>
            </div>
            <ul>
              {exportFiles.map((file) => <li key={file}>{file}</li>)}
            </ul>
            {revealError ? (
              <div className="batch-inline-error batch-reveal-error" role="alert">
                {revealError}
              </div>
            ) : null}
          </div>
        ) : null}
      </section>

      {assignDialogOpen ? (
        <AssignItemsDialog
          batchId={batchId}
          sessionId={assignDialogSession}
          onAssigned={acceptAssignedDetail}
          onClose={closeAssignDialog}
        />
      ) : null}

      {removeTarget ? (
        <div className="batch-dialog-backdrop" role="presentation">
          <div
            ref={removeDialogRef}
            className="batch-confirm-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="remove-confirm-title"
            onKeyDown={handleRemoveDialogKeys}
          >
            <h2 id="remove-confirm-title">确认移出票据</h2>
            <p>“{removeTarget.item.originalName}”将回到未归属票据池。</p>
            {removeError ? <div className="batch-inline-error" role="alert">{removeError}</div> : null}
            <div className="batch-form-actions">
              <button
                ref={removeCancelRef}
                type="button"
                className="button button-secondary"
                aria-disabled={removePending || undefined}
                autoFocus
                onClick={() => {
                  if (!removePending) closeRemoveDialog();
                }}
              >
                {removePending ? "正在移除" : "取消"}
              </button>
              <button
                type="button"
                className="button button-danger"
                disabled={removePending}
                onClick={() => void confirmRemoval()}
              >
                确认移出
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </section>
  );
}
