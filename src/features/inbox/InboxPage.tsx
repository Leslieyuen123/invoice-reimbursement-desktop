import {
  type InfiniteData,
  useInfiniteQuery,
  useQueryClient,
} from "@tanstack/react-query";
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
  PageDto,
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

const INBOX_PAGE_SIZE = 50;

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
  fallbackItemIds: string[];
  item: InvoiceItemDto;
  opener: HTMLButtonElement;
  sessionId: number;
}

type ItemListData = InfiniteData<PageDto<InvoiceItemDto>>;

function matchesItemFilter(item: InvoiceItemDto, filter: ItemFilter) {
  if (filter.status && item.status !== filter.status) return false;
  if (filter.suggestedPeriod && item.suggestedPeriod !== filter.suggestedPeriod) {
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
}

function reconcileItemList(
  data: ItemListData,
  filter: ItemFilter,
  item: InvoiceItemDto,
  insert: boolean,
  pageSize: number,
) {
  const exists = data.pages.some((page) =>
    page.items.some((candidate) => candidate.id === item.id),
  );
  const matches = matchesItemFilter(item, filter);
  const pages = data.pages.map((page) => ({
    ...page,
    items: page.items.flatMap((candidate) =>
      candidate.id === item.id ? (matches ? [item] : []) : [candidate],
    ).slice(0, pageSize),
  }));
  if (insert && !exists && matches && pages[0]) {
    // Keep cursor boundaries intact; a refetch restores any evicted page-one tail.
    pages[0] = {
      ...pages[0],
      items: [item, ...pages[0].items].slice(0, pageSize),
    };
  }
  return { ...data, pages };
}

function removeItemFromList(data: ItemListData, itemId: string) {
  return {
    ...data,
    pages: data.pages.map((page) => ({
      ...page,
      items: page.items.filter((item) => item.id !== itemId),
    })),
  };
}

export function InboxPage() {
  const queryClient = useQueryClient();
  const [searchParams, setSearchParams] = useSearchParams();
  const [selection, setSelection] = useState<DrawerSelection | null>(null);
  const nextSessionId = useRef(0);
  const pageRef = useRef<HTMLDivElement>(null);
  const pendingFocusRef = useRef<DrawerSelection | null>(null);
  const selectionRef = useRef(selection);
  const statusTabsRef = useRef<HTMLDivElement>(null);
  selectionRef.current = selection;
  const [suggestedPeriod, setSuggestedPeriod] = useState("");
  const [category, setCategory] = useState<Category | "">("");
  const [sourceType, setSourceType] = useState<SourceType | "">("");
  const [query, setQuery] = useState("");
  const [importOutcomes, setImportOutcomes] = useState<
    ManualImportOutcomeDto[]
  >([]);
  const [importingPaths, setImportingPaths] = useState<string[]>([]);
  const status = statusFromSearch(searchParams.get("status"));
  const batchId = searchParams.get("batchId")?.trim() || undefined;
  const filter = useMemo<ItemFilter>(
    () => ({
      ...(status ? { status } : {}),
      ...(suggestedPeriod ? { suggestedPeriod } : {}),
      ...(category ? { category } : {}),
      ...(sourceType ? { sourceType } : {}),
      ...(query.trim() ? { query: query.trim() } : {}),
      ...(batchId ? { batchId } : {}),
    }),
    [batchId, category, query, sourceType, status, suggestedPeriod],
  );
  const itemsQuery = useInfiniteQuery({
    queryKey: queryKeys.items(filter),
    initialPageParam: undefined as
      | { sortValue: string; id: string }
      | undefined,
    queryFn: ({ pageParam }) =>
      api.listItems(filter, { cursor: pageParam, pageSize: INBOX_PAGE_SIZE }),
    getNextPageParam: (lastPage) => lastPage.nextCursor ?? undefined,
  });
  const queriedItems = itemsQuery.data?.pages.flatMap((page) => page.items) ?? [];
  const items = queriedItems.filter((item) => matchesItemFilter(item, filter));

  useEffect(() => {
    setSelection((current) => {
      if (!current) return current;
      const serverItem = itemsQuery.data?.pages
        .flatMap((page) => page.items)
        .find((item) => item.id === current.item.id);
      if (!serverItem || serverItem === current.item) return current;
      const serverUpdatedAt = Date.parse(serverItem.updatedAt);
      const currentUpdatedAt = Date.parse(current.item.updatedAt);
      return serverUpdatedAt >= currentUpdatedAt
        ? { ...current, item: serverItem }
        : current;
    });
  }, [itemsQuery.data]);

  function reconcileCachedItem(item: InvoiceItemDto, insert: boolean) {
    for (const [queryKey, data] of queryClient.getQueriesData<ItemListData>({
      queryKey: queryKeys.itemLists,
    })) {
      if (!data) continue;
      const cachedFilter = queryKey[3] as ItemFilter;
      queryClient.setQueryData(
        queryKey,
        reconcileItemList(data, cachedFilter, item, insert, INBOX_PAGE_SIZE),
      );
    }
  }

  function invalidateItemLists() {
    void queryClient.invalidateQueries({ queryKey: queryKeys.itemLists });
  }

  function updateItem(item: InvoiceItemDto, sessionId: number) {
    reconcileCachedItem(item, false);
    setSelection((current) =>
      current?.sessionId === sessionId ? { ...current, item } : current,
    );
    invalidateItemLists();
  }

  function deleteItem(itemId: string, sessionId: number) {
    for (const [queryKey, data] of queryClient.getQueriesData<ItemListData>({
      queryKey: queryKeys.itemLists,
    })) {
      if (data) queryClient.setQueryData(queryKey, removeItemFromList(data, itemId));
    }
    dismissDrawer(sessionId);
    invalidateItemLists();
  }

  function openItem(item: InvoiceItemDto, opener: HTMLButtonElement) {
    const itemIndex = items.findIndex((candidate) => candidate.id === item.id);
    const followingIds = items.slice(itemIndex + 1).map((candidate) => candidate.id);
    const precedingIds = items
      .slice(0, itemIndex)
      .reverse()
      .map((candidate) => candidate.id);
    nextSessionId.current += 1;
    setSelection({
      fallbackItemIds: [...followingIds, ...precedingIds],
      item,
      opener,
      sessionId: nextSessionId.current,
    });
  }

  function dismissDrawer(sessionId: number) {
    const current = selectionRef.current;
    if (current?.sessionId !== sessionId) return;
    pendingFocusRef.current = current;
    setSelection(null);
  }

  useEffect(() => {
    if (selection) return;
    const dismissed = pendingFocusRef.current;
    if (!dismissed) return;
    pendingFocusRef.current = null;

    const canFocus = (element: HTMLElement | null): element is HTMLElement =>
      Boolean(element?.isConnected && !element.closest("[hidden]"));
    let target: HTMLElement | null = canFocus(dismissed.opener)
      ? dismissed.opener
      : null;
    if (!target) {
      const rowButtons = [
        ...(pageRef.current?.querySelectorAll<HTMLButtonElement>(
          "[data-inbox-item-id]",
        ) ?? []),
      ];
      target =
        dismissed.fallbackItemIds
          .map((itemId) =>
            rowButtons.find((button) => button.dataset.inboxItemId === itemId),
          )
          .find((button): button is HTMLButtonElement => canFocus(button ?? null)) ??
        null;
    }
    const selectedStatusTab =
      statusTabsRef.current?.querySelector<HTMLButtonElement>(
        '[role="tab"][aria-selected="true"]',
      ) ?? null;
    if (!target && canFocus(selectedStatusTab)) target = selectedStatusTab;
    target?.focus();
  }, [items, selection]);

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
    async (paths: string[]) => {
      setImportingPaths((current) => [...current, ...paths]);
      try {
        const outcomes = await api.importManualFiles(paths);
        for (const outcome of outcomes) {
          if (outcome.status === "imported") reconcileCachedItem(outcome.item, true);
        }
        setImportOutcomes((current) => {
          const byPath = new Map(current.map((outcome) => [outcome.path, outcome]));
          for (const outcome of outcomes) byPath.set(outcome.path, outcome);
          return [...byPath.values()];
        });
        invalidateItemLists();
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
    <div className="inbox-page" ref={pageRef}>
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
                    onClick={() => void importPaths([outcome.path])}
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

      <div
        className="inbox-status-tabs"
        role="tablist"
        aria-label="票据状态"
        ref={statusTabsRef}
      >
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
          onClose={() => dismissDrawer(selection.sessionId)}
          onDeleted={(itemId) => deleteItem(itemId, selection.sessionId)}
          onSaved={(item) => updateItem(item, selection.sessionId)}
        />
      ) : null}
    </div>
  );
}
