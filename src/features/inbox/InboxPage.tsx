import { useInfiniteQuery } from "@tanstack/react-query";
import { Check, RotateCcw, Search, TriangleAlert } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";

import { FileDropZone } from "../../components/FileDropZone";
import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import type {
  Category,
  InvoiceItemDto,
  ItemFilter,
  ItemStatus,
  ManualImportOutcomeDto,
  SourceType,
} from "../../types";
import { InboxTable } from "./InboxTable";
import { ItemDrawer } from "./ItemDrawer";

const itemStatuses: ItemStatus[] = [
  "pending_recognition",
  "pending_confirmation",
  "recognition_failed",
  "suspected_duplicate",
  "ready",
];

const statusTabs: Array<{ value?: ItemStatus; label: string }> = [
  { label: "全部" },
  { value: "pending_recognition", label: "待识别" },
  { value: "pending_confirmation", label: "待确认" },
  { value: "recognition_failed", label: "识别失败" },
  { value: "suspected_duplicate", label: "疑似重复" },
  { value: "ready", label: "可纳入批次" },
];

function errorMessage(error: unknown) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return error.message;
  }
  return "待处理池暂时无法加载";
}

function statusFromSearch(value: string | null): ItemStatus | undefined {
  return itemStatuses.find((status) => status === value);
}

interface DrawerSelection {
  item: InvoiceItemDto;
  opener: HTMLButtonElement;
  sessionId: number;
}

export function InboxPage() {
  const [searchParams, setSearchParams] = useSearchParams();
  const [selection, setSelection] = useState<DrawerSelection | null>(null);
  const nextSessionId = useRef(0);
  const [suggestedPeriod, setSuggestedPeriod] = useState("");
  const [category, setCategory] = useState<Category | "">("");
  const [sourceType, setSourceType] = useState<SourceType | "">("");
  const [query, setQuery] = useState("");
  const [importedItems, setImportedItems] = useState<InvoiceItemDto[]>([]);
  const [updatedItems, setUpdatedItems] = useState<InvoiceItemDto[]>([]);
  const [deletedItemIds, setDeletedItemIds] = useState<string[]>([]);
  const [importOutcomes, setImportOutcomes] = useState<
    ManualImportOutcomeDto[]
  >([]);
  const [importingPaths, setImportingPaths] = useState<string[]>([]);
  const status = statusFromSearch(searchParams.get("status"));
  const filter = useMemo<ItemFilter>(
    () => ({
      ...(status ? { status } : {}),
      ...(suggestedPeriod ? { suggestedPeriod } : {}),
      ...(category ? { category } : {}),
      ...(sourceType ? { sourceType } : {}),
      ...(query.trim() ? { query: query.trim() } : {}),
    }),
    [category, query, sourceType, status, suggestedPeriod],
  );
  const itemsQuery = useInfiniteQuery({
    queryKey: queryKeys.items(filter),
    initialPageParam: undefined as
      | { sortValue: string; id: string }
      | undefined,
    queryFn: ({ pageParam }) =>
      api.listItems(filter, { cursor: pageParam, pageSize: 50 }),
    getNextPageParam: (lastPage) => lastPage.nextCursor ?? undefined,
  });
  const queriedItems = itemsQuery.data?.pages.flatMap((page) => page.items) ?? [];
  const items = useMemo(() => {
    const byId = new Map<string, InvoiceItemDto>();
    for (const item of [...queriedItems, ...importedItems, ...updatedItems]) {
      byId.set(item.id, item);
    }
    return [...byId.values()].filter((item) => {
      if (deletedItemIds.includes(item.id)) return false;
      if (filter.status && item.status !== filter.status) return false;
      if (
        filter.suggestedPeriod &&
        item.suggestedPeriod !== filter.suggestedPeriod
      ) {
        return false;
      }
      if (
        filter.category &&
        (item.finalCategory ?? item.suggestedCategory) !== filter.category
      ) {
        return false;
      }
      if (filter.sourceType && item.sourceType !== filter.sourceType) return false;
      if (filter.query) {
        const haystack = [item.originalName, item.city, item.company]
          .filter(Boolean)
          .join(" ")
          .toLocaleLowerCase("zh-CN");
        if (!haystack.includes(filter.query.toLocaleLowerCase("zh-CN"))) return false;
      }
      return true;
    });
  }, [deletedItemIds, filter, importedItems, queriedItems, updatedItems]);

  function updateItem(item: InvoiceItemDto, sessionId: number) {
    setUpdatedItems((current) => [
      ...current.filter((candidate) => candidate.id !== item.id),
      item,
    ]);
    setSelection((current) =>
      current?.sessionId === sessionId ? { ...current, item } : current,
    );
  }

  function deleteItem(itemId: string, sessionId: number) {
    setDeletedItemIds((current) =>
      current.includes(itemId) ? current : [...current, itemId],
    );
    setSelection((current) =>
      current?.sessionId === sessionId ? null : current,
    );
  }

  function openItem(item: InvoiceItemDto, opener: HTMLButtonElement) {
    nextSessionId.current += 1;
    setSelection({ item, opener, sessionId: nextSessionId.current });
  }

  function closeDrawer(sessionId: number, opener: HTMLButtonElement) {
    if (selection?.sessionId !== sessionId) return;
    setSelection(null);
    opener.focus();
  }

  useEffect(() => {
    setSelection(null);
  }, [filter]);

  function chooseStatus(nextStatus?: ItemStatus) {
    const next = new URLSearchParams(searchParams);
    if (nextStatus) next.set("status", nextStatus);
    else next.delete("status");
    setSearchParams(next);
  }

  const importPaths = useCallback(
    async (paths: string[], retryPath?: string) => {
      setImportingPaths((current) => [...current, ...paths]);
      try {
        const outcomes = await api.importManualFiles(paths);
        setImportedItems((current) => {
          const byId = new Map(current.map((item) => [item.id, item]));
          for (const outcome of outcomes) {
            if (outcome.status === "imported") byId.set(outcome.item.id, outcome.item);
          }
          return [...byId.values()];
        });
        setImportOutcomes((current) => {
          if (!retryPath) return [...current, ...outcomes];
          const replacement = outcomes[0];
          return current.map((outcome) =>
            outcome.path === retryPath && replacement ? replacement : outcome,
          );
        });
        if (
          outcomes.some(
            (outcome) =>
              outcome.status === "imported" &&
              outcome.item.status === "suspected_duplicate",
          )
        ) {
          chooseStatus("suspected_duplicate");
        }
      } finally {
        setImportingPaths((current) =>
          current.filter((path) => !paths.includes(path)),
        );
      }
    },
    [searchParams],
  );

  function fileName(path: string) {
    return path.split(/[\\/]/).pop() ?? path;
  }

  return (
    <div className="inbox-page">
      <header className="inbox-heading">
        <div>
          <h1>待处理池</h1>
          <p>集中核对自动抓取和手动补充的票据</p>
        </div>
        <FileDropZone
          disabled={importingPaths.length > 0}
          onPaths={(paths) => void importPaths(paths)}
        />
      </header>

      {importOutcomes.length > 0 ? (
        <ol className="import-outcomes" aria-label="导入结果">
          {importOutcomes.map((outcome) => (
            <li key={outcome.path} data-status={outcome.status}>
              {outcome.status === "imported" ? (
                <>
                  <Check size={15} strokeWidth={1.8} aria-hidden="true" />
                  <span>已导入：{fileName(outcome.path)}</span>
                </>
              ) : (
                <div role="alert">
                  <TriangleAlert size={15} strokeWidth={1.8} aria-hidden="true" />
                  <span>
                    {fileName(outcome.path)}：{outcome.error.message}
                  </span>
                  <button
                    type="button"
                    aria-label={`重试 ${fileName(outcome.path)}`}
                    disabled={importingPaths.includes(outcome.path)}
                    onClick={() =>
                      void importPaths([outcome.path], outcome.path)
                    }
                  >
                    <RotateCcw size={14} strokeWidth={1.8} aria-hidden="true" />
                    重试
                  </button>
                </div>
              )}
            </li>
          ))}
        </ol>
      ) : null}

      <div className="inbox-status-tabs" role="tablist" aria-label="票据状态">
        {statusTabs.map((tab) => (
          <button
            type="button"
            role="tab"
            aria-selected={status === tab.value}
            key={tab.label}
            onClick={() => chooseStatus(tab.value)}
          >
            {tab.label}
          </button>
        ))}
      </div>

      <div className="inbox-filters" aria-label="票据筛选">
        <label>
          <span>建议月份</span>
          <input
            type="month"
            value={suggestedPeriod}
            onChange={(event) => setSuggestedPeriod(event.target.value)}
          />
        </label>
        <label>
          <span>分类</span>
          <select
            aria-label="分类筛选"
            value={category}
            onChange={(event) => setCategory(event.target.value as Category | "")}
          >
            <option value="">全部分类</option>
            <option value="transport">交通</option>
            <option value="dining">餐饮</option>
            <option value="accommodation">住宿</option>
            <option value="hospitality">招待</option>
          </select>
        </label>
        <label>
          <span>来源</span>
          <select
            value={sourceType}
            onChange={(event) =>
              setSourceType(event.target.value as SourceType | "")
            }
          >
            <option value="">全部来源</option>
            <option value="email">邮箱</option>
            <option value="manual_upload">手动上传</option>
          </select>
        </label>
        <label className="inbox-search">
          <span>搜索</span>
          <div>
            <Search size={15} strokeWidth={1.7} aria-hidden="true" />
            <input
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="文件名、城市、主体"
            />
          </div>
        </label>
      </div>

      <div className="inbox-table-region">
        <InboxTable
          items={items}
          selectedItemId={selection?.item.id ?? null}
          state={
            itemsQuery.isPending
              ? "loading"
              : itemsQuery.isError
                ? "error"
                : "ready"
          }
          errorMessage={errorMessage(itemsQuery.error)}
          onSelect={openItem}
          onRetry={() => void itemsQuery.refetch()}
        />
      </div>
      {itemsQuery.hasNextPage ? (
        <button
          className="button button-secondary inbox-load-more"
          type="button"
          disabled={itemsQuery.isFetchingNextPage}
          onClick={() => void itemsQuery.fetchNextPage()}
        >
          {itemsQuery.isFetchingNextPage ? "加载中" : "加载更多"}
        </button>
      ) : null}
      {selection ? (
        <ItemDrawer
          key={`${selection.item.id}:${selection.sessionId}`}
          item={selection.item}
          onClose={() => closeDrawer(selection.sessionId, selection.opener)}
          onDeleted={(itemId) => deleteItem(itemId, selection.sessionId)}
          onSaved={(item) => updateItem(item, selection.sessionId)}
        />
      ) : null}
    </div>
  );
}
