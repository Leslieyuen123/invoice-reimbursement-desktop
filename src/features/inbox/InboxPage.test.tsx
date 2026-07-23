import {
  act,
  cleanup,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import {
  type InfiniteData,
  QueryClient,
  QueryClientProvider,
} from "@tanstack/react-query";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

import { open } from "@tauri-apps/plugin-dialog";
import { App } from "../../app/App";
import { queryKeys } from "../../lib/queryKeys";
import {
  invokeMock,
  mockCommand,
  resetMockApi,
} from "../../test/mockApi";
import type { InvoiceItemDto, PageDto } from "../../types";
import { InboxPage } from "./InboxPage";

const openDialogMock = vi.mocked(open);
const fetchMock = vi.fn<typeof fetch>();

function pendingInvoiceFixture(
  overrides: Partial<InvoiceItemDto> = {},
): InvoiceItemDto {
  return {
    id: "invoice-taxi",
    originalName: "出租车电子发票.pdf",
    previewUrl: "invoice-file://item/invoice-taxi?variant=normalized",
    sourceType: "email",
    sourceAccountId: "account-finance",
    fetchedAt: "2026-07-15T09:45:00+08:00",
    invoiceDate: "2026-07-14",
    suggestedPeriod: "2026-07",
    batchId: null,
    suggestedCategory: "dining",
    finalCategory: null,
    amountCents: 8_800,
    currency: "CNY",
    city: "上海",
    company: "上海出租车有限公司",
    status: "pending_confirmation",
    recognitionStatus: "succeeded",
    confirmationStatus: "pending",
    dedupeStatus: "unique",
    note: null,
    eventTag: null,
    projectTag: null,
    createdAt: "2026-07-15T09:45:00+08:00",
    updatedAt: "2026-07-15T09:45:00+08:00",
    ...overrides,
  };
}

function renderAppAt(path: string) {
  window.history.replaceState({}, "", path);
  return render(<App />);
}

function renderInboxAt(path: string, queryClient: QueryClient) {
  return render(
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[path]}>
        <InboxPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function commandCalls(command: string) {
  return invokeMock.mock.calls
    .filter(([wireCommand]) => wireCommand === command)
    .map(([, arguments_]) => {
      if (
        typeof arguments_ === "object" &&
        arguments_ !== null &&
        "input" in arguments_
      ) {
        return arguments_.input as Record<string, unknown>;
      }
      return arguments_ as Record<string, unknown>;
    });
}

describe("InboxPage", () => {
  beforeEach(() => {
    cleanup();
    resetMockApi();
    openDialogMock.mockReset();
    fetchMock.mockReset();
    fetchMock.mockResolvedValue(
      new Response(new Uint8Array(), {
        status: 200,
        headers: { "Content-Type": "application/pdf" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => vi.unstubAllGlobals());

  it("filters pending items and saves a manual correction", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });
    mockCommand("review_item", (arguments_) => {
      const input = (arguments_ as { input: Record<string, unknown> }).input;
      return pendingInvoiceFixture({
        finalCategory: input.finalCategory as InvoiceItemDto["finalCategory"],
        amountCents: input.amountCents as number,
        status: "ready",
        confirmationStatus: "confirmed",
      });
    });

    renderAppAt("/inbox?status=pending_confirmation");
    await user.click(await screen.findByText("出租车电子发票.pdf"));
    await user.click(screen.getByRole("radio", { name: "交通" }));
    await user.clear(screen.getByLabelText("金额"));
    await user.type(screen.getByLabelText("金额"), "128.50");
    await user.click(screen.getByRole("button", { name: "保存并确认" }));
    expect(commandCalls("review_item")[0].amountCents).toBe(12850);
  });

  it("closes the item drawer after a manual correction is saved", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });
    mockCommand(
      "review_item",
      pendingInvoiceFixture({ status: "ready", confirmationStatus: "confirmed" }),
    );

    renderAppAt("/inbox");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "保存并确认" }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog", { name: "票据详情" })).not.toBeInTheDocument();
    });
  });

  it("invalidates the dashboard after reviewing an item", async () => {
    const user = userEvent.setup();
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const invalidate = vi.spyOn(queryClient, "invalidateQueries");
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });
    mockCommand(
      "review_item",
      pendingInvoiceFixture({ status: "ready", confirmationStatus: "confirmed" }),
    );

    renderInboxAt("/inbox?status=pending_confirmation", queryClient);
    await user.click(await screen.findByRole("button", { name: "出租车电子发票.pdf" }));
    await user.click(screen.getByRole("button", { name: "保存并确认" }));

    await waitFor(() => expect(commandCalls("review_item")).toHaveLength(1));
    expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.dashboard });
  });

  it("invalidates the dashboard after importing an item", async () => {
    const user = userEvent.setup();
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const invalidate = vi.spyOn(queryClient, "invalidateQueries");
    const path = "/Users/finance/新发票.pdf";
    mockCommand("list_items", { items: [], nextCursor: null });
    openDialogMock.mockResolvedValue([path]);
    mockCommand("import_manual_files", [
      {
        status: "imported" as const,
        path,
        item: pendingInvoiceFixture({ sourceType: "manual_upload" }),
      },
    ]);

    renderInboxAt("/inbox", queryClient);
    await screen.findByText("当前筛选下没有票据");
    await user.click(screen.getByRole("button", { name: "选择文件" }));

    await screen.findByText("已导入：新发票.pdf");
    expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.dashboard });
  });

  it("does not reconcile an old imported item into a recent cached list", async () => {
    const user = userEvent.setup();
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false, staleTime: Infinity } },
    });
    const recentKey = queryKeys.items({ recent: true });
    queryClient.setQueryData(recentKey, {
      pages: [{ items: [], nextCursor: null }],
      pageParams: [undefined],
    });
    mockCommand("list_items", new Promise(() => undefined));
    openDialogMock.mockResolvedValue(["/Users/finance/旧票据.pdf"]);
    mockCommand("import_manual_files", [
      {
        status: "imported" as const,
        path: "/Users/finance/旧票据.pdf",
        item: pendingInvoiceFixture({
          id: "old-import",
          originalName: "旧票据.pdf",
          createdAt: "2000-01-01T00:00:00Z",
        }),
      },
    ]);

    renderInboxAt("/inbox?scope=recent", queryClient);
    await user.click(screen.getByRole("button", { name: "选择文件" }));
    await screen.findByText("已导入：旧票据.pdf");

    const cached = queryClient.getQueryData<
      InfiniteData<PageDto<InvoiceItemDto>>
    >(recentKey);
    expect(cached?.pages[0].items).toEqual([]);
  });

  it("shows and clears only the recent scope from the inbox URL", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", { items: [], nextCursor: null });

    renderAppAt(
      "/inbox?scope=recent&status=pending_confirmation&batchId=batch-1&foo=bar",
    );

    const clearRecent = await screen.findByRole("button", {
      name: "清除最近 7 天筛选",
    });
    expect(clearRecent).toHaveTextContent("最近 7 天");
    await waitFor(() => {
      expect(commandCalls("list_items")[0]).toMatchObject({
        filter: {
          recent: true,
          status: "pending_confirmation",
          batchId: "batch-1",
        },
      });
    });

    await user.click(clearRecent);

    await waitFor(() => {
      const params = new URLSearchParams(window.location.search);
      expect(params.has("scope")).toBe(false);
      expect(params.get("status")).toBe("pending_confirmation");
      expect(params.get("batchId")).toBe("batch-1");
      expect(params.get("foo")).toBe("bar");
      expect(commandCalls("list_items")).toHaveLength(2);
    });
    const latestFilter = commandCalls("list_items").at(-1)?.filter as Record<
      string,
      unknown
    >;
    expect(latestFilter).toMatchObject({
      status: "pending_confirmation",
      batchId: "batch-1",
    });
    expect(latestFilter).not.toHaveProperty("recent");
  });

  it("resets bounded pagination when a filter changes", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", (arguments_) => {
      const request = arguments_ as {
        filter: Record<string, unknown>;
        page: { cursor?: { sortValue: string; id: string }; pageSize: number };
      };
      if (request.filter.category === "transport") {
        return { items: [pendingInvoiceFixture()], nextCursor: null };
      }
      if (request.page.cursor) {
        return {
          items: [
            pendingInvoiceFixture({
              id: "invoice-hotel",
              originalName: "酒店住宿发票.pdf",
            }),
          ],
          nextCursor: null,
        };
      }
      return {
        items: [pendingInvoiceFixture()],
        nextCursor: { sortValue: "2026-07-15T09:45:00+08:00", id: "invoice-taxi" },
      };
    });

    renderAppAt("/inbox?status=pending_confirmation");
    expect(await screen.findByText("出租车电子发票.pdf")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "加载更多" }));
    expect(await screen.findByText("酒店住宿发票.pdf")).toBeInTheDocument();

    await user.selectOptions(screen.getByLabelText("分类筛选"), "transport");

    await waitFor(() => {
      expect(screen.queryByText("酒店住宿发票.pdf")).not.toBeInTheDocument();
    });
    const listCalls = invokeMock.mock.calls.filter(
      ([command]) => command === "list_items",
    );
    expect(listCalls).toHaveLength(3);
    expect(listCalls[0][1]).toMatchObject({
      filter: { status: "pending_confirmation" },
      page: { pageSize: 50 },
    });
    expect(listCalls[1][1]).toMatchObject({
      page: {
        cursor: {
          sortValue: "2026-07-15T09:45:00+08:00",
          id: "invoice-taxi",
        },
        pageSize: 50,
      },
    });
    expect(listCalls[2][1]).toMatchObject({
      filter: {
        status: "pending_confirmation",
        category: "transport",
      },
      page: { pageSize: 50 },
    });
  });

  it("keeps per-file import outcomes and retries only the failed file", async () => {
    const user = userEvent.setup();
    const serverItems: InvoiceItemDto[] = [];
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => ({ items: serverItems, nextCursor: null }));
    openDialogMock.mockResolvedValue([
      "/Users/finance/出租车电子发票.pdf",
      "/Users/finance/损坏附件.pdf",
    ]);
    let importAttempt = 0;
    mockCommand("import_manual_files", (arguments_) => {
      importAttempt += 1;
      const paths = (arguments_ as { paths: string[] }).paths;
      if (importAttempt === 1) {
        const imported = pendingInvoiceFixture({ sourceType: "manual_upload" });
        serverItems.push(imported);
        return [
          {
            status: "imported" as const,
            path: paths[0],
            item: imported,
          },
          {
            status: "failed" as const,
            path: paths[1],
            error: {
              code: "validation" as const,
              field: "file",
              message: "文件已损坏",
            },
          },
        ];
      }
      const recovered = pendingInvoiceFixture({
        id: "invoice-recovered",
        originalName: "损坏附件.pdf",
        sourceType: "manual_upload",
      });
      serverItems.push(recovered);
      return [
        {
          status: "imported" as const,
          path: paths[0],
          item: recovered,
        },
      ];
    });

    renderAppAt("/inbox");
    await screen.findByText("当前筛选下没有票据");
    await user.click(screen.getByRole("button", { name: "选择文件" }));

    expect(await screen.findByText("出租车电子发票.pdf")).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("文件已损坏");
    await user.click(
      screen.getByRole("button", { name: "重试 损坏附件.pdf" }),
    );
    expect(await screen.findByText("损坏附件.pdf")).toBeInTheDocument();

    const importCalls = invokeMock.mock.calls
      .filter(([command]) => command === "import_manual_files")
      .map(([, arguments_]) => arguments_);
    expect(importCalls).toEqual([
      {
        paths: [
          "/Users/finance/出租车电子发票.pdf",
          "/Users/finance/损坏附件.pdf",
        ],
      },
      { paths: ["/Users/finance/损坏附件.pdf"] },
    ]);
  });

  it("retires a local save when an equal-or-newer server record is refetched", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const serverItem = pendingInvoiceFixture({
      originalName: "后台同步结果.pdf",
      updatedAt: "2026-07-15T10:01:00+08:00",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return {
        items: [listAttempt === 1 ? pendingInvoiceFixture() : serverItem],
        nextCursor: null,
      };
    });
    mockCommand(
      "review_item",
      pendingInvoiceFixture({
        originalName: "本地保存响应.pdf",
        updatedAt: "2026-07-15T10:00:00+08:00",
      }),
    );

    renderAppAt("/inbox");
    await user.click(await screen.findByText("出租车电子发票.pdf"));
    await user.click(screen.getByRole("button", { name: "保存并确认" }));

    expect(await screen.findAllByText("后台同步结果.pdf")).not.toHaveLength(0);
    expect(screen.queryByText("本地保存响应.pdf")).not.toBeInTheDocument();
    expect(listAttempt).toBe(2);
  });

  it("retires an imported cache record when the server refetches the same id", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const imported = pendingInvoiceFixture({
      id: "invoice-imported",
      originalName: "本地导入记录.pdf",
      sourceType: "manual_upload",
      updatedAt: "2026-07-15T10:00:00+08:00",
    });
    const serverItem = {
      ...imported,
      originalName: "服务器导入记录.pdf",
      updatedAt: "2026-07-15T10:01:00+08:00",
    };
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return {
        items: listAttempt === 1 ? [] : [serverItem],
        nextCursor: null,
      };
    });
    openDialogMock.mockResolvedValue(["/Users/finance/导入票据.pdf"]);
    mockCommand("import_manual_files", [
      {
        status: "imported" as const,
        path: "/Users/finance/导入票据.pdf",
        item: imported,
      },
    ]);

    renderAppAt("/inbox");
    await screen.findByText("当前筛选下没有票据");
    await user.click(screen.getByRole("button", { name: "选择文件" }));

    expect(await screen.findByText("服务器导入记录.pdf")).toBeInTheDocument();
    expect(screen.queryByText("本地导入记录.pdf")).not.toBeInTheDocument();
    expect(listAttempt).toBe(2);
  });

  it("does not retain a deletion tombstone over a newer server record", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const duplicate = pendingInvoiceFixture({
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    const serverItem = {
      ...duplicate,
      originalName: "重新同步票据.pdf",
      updatedAt: "2026-07-15T10:01:00+08:00",
    };
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return {
        items: [listAttempt === 1 ? duplicate : serverItem],
        nextCursor: null,
      };
    });
    mockCommand("resolve_duplicate", null);

    renderAppAt("/inbox?status=suspected_duplicate");
    await user.click(await screen.findByText("出租车电子发票.pdf"));
    await user.click(screen.getByRole("button", { name: "删除重复" }));

    expect(await screen.findByText("重新同步票据.pdf")).toBeInTheDocument();
    expect(listAttempt).toBe(2);
  });

  it("replaces import outcomes for a path instead of accumulating duplicates", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", { items: [], nextCursor: null });
    openDialogMock.mockResolvedValue(["/Users/finance/同一票据.pdf"]);
    mockCommand("import_manual_files", [
      {
        status: "imported" as const,
        path: "/Users/finance/同一票据.pdf",
        item: pendingInvoiceFixture({ sourceType: "manual_upload" }),
      },
    ]);

    renderAppAt("/inbox");
    await screen.findByText("当前筛选下没有票据");
    await user.click(screen.getByRole("button", { name: "选择文件" }));
    await user.click(screen.getByRole("button", { name: "选择文件" }));

    expect(screen.getAllByText("已导入：同一票据.pdf")).toHaveLength(1);
  });

  it("bounds optimistic imports when the authoritative refetch fails", async () => {
    const user = userEvent.setup();
    const pageSize = 50;
    const firstCursor = {
      sortValue: "2026-07-15T09:00:00+08:00",
      id: "seed-050",
    };
    const secondCursor = {
      sortValue: "2026-07-15T08:00:00+08:00",
      id: "seed-100",
    };
    const seedItems = Array.from({ length: pageSize * 2 }, (_, index) =>
      pendingInvoiceFixture({
        id: `seed-${String(index + 1).padStart(3, "0")}`,
        originalName: `原有票据-${String(index + 1).padStart(3, "0")}.pdf`,
        sourceType: "manual_upload",
      }),
    );
    const activeKey = queryKeys.items({});
    const manualKey = queryKeys.items({ sourceType: "manual_upload" });
    const activeData: InfiniteData<PageDto<InvoiceItemDto>> = {
      pages: [{ items: seedItems.slice(0, pageSize), nextCursor: firstCursor }],
      pageParams: [undefined],
    };
    const manualData: InfiniteData<PageDto<InvoiceItemDto>> = {
      pages: [
        { items: seedItems.slice(0, pageSize), nextCursor: firstCursor },
        { items: seedItems.slice(pageSize), nextCursor: secondCursor },
      ],
      pageParams: [undefined, firstCursor],
    };
    const queryClient = new QueryClient({
      defaultOptions: {
        queries: { retry: false, staleTime: Number.POSITIVE_INFINITY },
      },
    });
    queryClient.setQueryData(activeKey, activeData);
    queryClient.setQueryData(manualKey, manualData);

    const paths = Array.from(
      { length: pageSize + 5 },
      (_, index) =>
        `/Users/finance/批量导入-${String(index + 1).padStart(3, "0")}.pdf`,
    );
    const outcomes = paths.map((path, index) => ({
      status: "imported" as const,
      path,
      item: pendingInvoiceFixture({
        id: `imported-${String(index + 1).padStart(3, "0")}`,
        originalName: path.split("/").at(-1),
        sourceType: "manual_upload",
      }),
    }));
    mockCommand("list_items", async () => {
      throw new Error("权威刷新失败");
    });
    mockCommand("import_manual_files", outcomes);
    openDialogMock.mockResolvedValue(paths);

    renderInboxAt("/inbox", queryClient);
    expect(
      screen.getByRole("table").querySelectorAll("[data-inbox-item-id]"),
    ).toHaveLength(pageSize);
    await user.click(screen.getByRole("button", { name: "选择文件" }));

    const outcomeList = await screen.findByRole("list", { name: "导入结果" });
    const outcomeRows = within(outcomeList).getAllByRole("listitem");
    expect(outcomeRows).toHaveLength(paths.length);
    expect(outcomeRows.map((row) => row.textContent)).toEqual(
      paths.map((path) => `已导入：${path.split("/").at(-1)}`),
    );
    expect(outcomeRows.every((row) => row.dataset.status === "imported")).toBe(
      true,
    );
    expect(within(outcomeList).queryByRole("alert")).not.toBeInTheDocument();
    await waitFor(() => expect(commandCalls("list_items")).toHaveLength(1));

    const cachedLists = queryClient.getQueriesData<
      InfiniteData<PageDto<InvoiceItemDto>>
    >({ queryKey: queryKeys.itemLists });
    expect(cachedLists).toHaveLength(2);
    for (const [, data] of cachedLists) {
      expect(data).toBeDefined();
      for (const page of data?.pages ?? []) {
        expect(page.items.length).toBeLessThanOrEqual(pageSize);
      }
    }
    expect(queryClient.getQueryData<typeof activeData>(activeKey)?.pages).toHaveLength(
      1,
    );
    expect(
      queryClient.getQueryData<typeof activeData>(activeKey)?.pages[0].nextCursor,
    ).toEqual(firstCursor);
    expect(queryClient.getQueryData<typeof manualData>(manualKey)?.pages).toHaveLength(
      2,
    );
    expect(
      queryClient.getQueryData<typeof manualData>(manualKey)?.pages.map(
        (page) => page.nextCursor,
      ),
    ).toEqual([firstCursor, secondCursor]);

    const table = screen.getByRole("table");
    expect(table.querySelectorAll("[data-inbox-item-id]")).toHaveLength(pageSize);
    expect(
      within(table).getByRole("button", { name: "批量导入-055.pdf" }),
    ).toBeInTheDocument();
    expect(
      within(table).queryByRole("button", { name: "批量导入-001.pdf" }),
    ).not.toBeInTheDocument();
    expect(
      within(outcomeList).getByText("已导入：批量导入-001.pdf"),
    ).toBeInTheDocument();
  });

  it("rejects amount input with more than two decimal places", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });

    renderAppAt("/inbox");
    await user.click(await screen.findByText("出租车电子发票.pdf"));
    await user.clear(screen.getByLabelText("金额"));
    await user.type(screen.getByLabelText("金额"), "12.345");
    await user.click(screen.getByRole("button", { name: "保存并确认" }));

    expect(await screen.findByText("金额最多保留两位小数")).toBeInTheDocument();
    expect(commandCalls("review_item")).toHaveLength(0);
  });

  it("submits an untouched safe-integer boundary amount without changing cents", async () => {
    const user = userEvent.setup();
    const boundaryCents = 9_007_199_254_740_990;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture({ amountCents: boundaryCents })],
      nextCursor: null,
    });
    mockCommand("review_item", pendingInvoiceFixture({ amountCents: boundaryCents }));

    renderAppAt("/inbox");
    await user.click(await screen.findByText("出租车电子发票.pdf"));
    await user.click(screen.getByRole("button", { name: "保存并确认" }));

    await waitFor(() => expect(commandCalls("review_item")).toHaveLength(1));
    expect(commandCalls("review_item")[0].amountCents).toBe(boundaryCents);
  });

  it("formats safe-integer boundary cents exactly in every inbox amount view", async () => {
    const user = userEvent.setup();
    const boundaryCents = 9_007_199_254_740_990;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture({ amountCents: boundaryCents })],
      nextCursor: null,
    });

    renderAppAt("/inbox");
    const opener = await screen.findByRole("button", {
      name: "出租车电子发票.pdf",
    });
    expect(within(opener.closest("tr")!).getByText("¥90071992547409.90")).toBeInTheDocument();
    await user.click(opener);

    const dialog = screen.getByRole("dialog", { name: "票据详情" });
    expect(within(dialog).getByLabelText("金额")).toHaveValue("90071992547409.90");
    expect(within(dialog).getByText("¥90071992547409.90")).toBeInTheDocument();
  });

  it("shows complete editable details and detects a failed preview response", async () => {
    const user = userEvent.setup();
    fetchMock.mockResolvedValueOnce(
      new Response("Preview unavailable", {
        status: 404,
        headers: { "Content-Type": "text/plain; charset=utf-8" },
      }),
    );
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });

    renderAppAt("/inbox");
    await user.click(await screen.findByText("出租车电子发票.pdf"));

    const drawer = screen.getByRole("dialog", { name: "票据详情" });
    expect(within(drawer).getByText("自动识别值")).toBeInTheDocument();
    expect(within(drawer).getByText("最终值")).toBeInTheDocument();
    expect(within(drawer).getByLabelText("开票日期")).toHaveValue(
      "2026-07-14",
    );
    expect(within(drawer).getByLabelText("建议月份")).toHaveValue("2026-07");
    expect(within(drawer).getByLabelText("城市")).toHaveValue("上海");
    expect(within(drawer).getByLabelText("主体")).toHaveValue(
      "上海出租车有限公司",
    );
    expect(within(drawer).getByLabelText("备注")).toBeInTheDocument();
    expect(within(drawer).getByLabelText("事项标签")).toBeInTheDocument();
    expect(within(drawer).getByLabelText("项目标签")).toBeInTheDocument();
    expect(within(drawer).getByText("未分配批次")).toBeInTheDocument();

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    expect(await within(drawer).findByRole("alert")).toHaveTextContent(
      "无法显示票据预览",
    );
    expect(within(drawer).queryByTitle("票据预览")).not.toBeInTheDocument();

    fetchMock.mockResolvedValueOnce(
      new Response(new Uint8Array(), {
        status: 200,
        headers: { "Content-Type": "application/pdf" },
      }),
    );
    fetchMock.mockImplementationOnce(() => new Promise(() => undefined));
    await user.click(within(drawer).getByRole("button", { name: "重新加载" }));
    expect(
      await within(drawer).findByRole("img", { name: "票据预览" }),
    ).toBeInTheDocument();
  });

  it("closes the review dialog with Escape and restores focus", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });

    renderAppAt("/inbox");
    const opener = await screen.findByRole("button", {
      name: "出租车电子发票.pdf",
    });
    await user.click(opener);

    const dialog = screen.getByRole("dialog", { name: "票据详情" });
    expect(
      within(dialog).getByRole("button", { name: "关闭票据详情" }),
    ).toHaveFocus();

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "票据详情" })).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
  });

  it("keeps focus inside the modal drawer and makes the background inert", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });

    renderAppAt("/inbox");
    const opener = await screen.findByRole("button", {
      name: "出租车电子发票.pdf",
    });
    await user.click(opener);

    const dialog = screen.getByRole("dialog", { name: "票据详情" });
    expect(dialog).toHaveAttribute("aria-modal", "true");
    const close = within(dialog).getByRole("button", { name: "关闭票据详情" });
    const save = within(dialog).getByRole("button", { name: "保存并确认" });
    expect(close).toHaveFocus();
    await user.tab({ shift: true });
    expect(save).toHaveFocus();
    await user.tab();
    expect(close).toHaveFocus();

    const background = dialog.parentElement?.children ?? [];
    for (const element of background) {
      if (element === dialog) continue;
      expect(element).toHaveAttribute("inert");
      expect(element).toHaveAttribute("aria-hidden", "true");
    }
    expect(document.querySelector(".app-sidebar")).toHaveAttribute("inert");
    expect(document.querySelector(".app-topbar")).toHaveAttribute("inert");

    await user.keyboard("{Escape}");
    expect(opener).toHaveFocus();
    expect(document.querySelector(".app-sidebar")).not.toHaveAttribute("inert");
    expect(document.querySelector(".app-topbar")).not.toHaveAttribute("inert");
  });

  it("restores focus to the latest opener after replacing the drawer", async () => {
    const user = userEvent.setup();
    const secondItem = pendingInvoiceFixture({
      id: "invoice-hotel",
      originalName: "酒店住宿发票.pdf",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture(), secondItem],
      nextCursor: null,
    });

    renderAppAt("/inbox");
    const firstOpener = await screen.findByRole("button", {
      name: "出租车电子发票.pdf",
    });
    const secondOpener = screen.getByRole("button", { name: "酒店住宿发票.pdf" });
    await user.click(firstOpener);
    await user.click(secondOpener);

    const dialog = screen.getByRole("dialog", { name: "票据详情" });
    expect(dialog).toHaveTextContent("酒店住宿发票.pdf");
    expect(
      within(dialog).getByRole("button", { name: "关闭票据详情" }),
    ).toHaveFocus();

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "票据详情" })).not.toBeInTheDocument();
    expect(secondOpener).toHaveFocus();
  });

  it("focuses the next visible row after deleting the active item", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const first = pendingInvoiceFixture({
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    const second = pendingInvoiceFixture({
      id: "invoice-duplicate-two",
      originalName: "出租车电子发票-副本.pdf",
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return { items: listAttempt === 1 ? [first, second] : [second], nextCursor: null };
    });
    mockCommand("resolve_duplicate", null);

    renderAppAt("/inbox?status=suspected_duplicate");
    await user.click(await screen.findByRole("button", { name: "出租车电子发票.pdf" }));
    await user.click(screen.getByRole("button", { name: "删除重复" }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog", { name: "票据详情" })).not.toBeInTheDocument();
    });
    expect(
      screen.getByRole("button", { name: "出租车电子发票-副本.pdf" }),
    ).toHaveFocus();
  });

  it("focuses the selected status tab after deleting the only visible row", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const duplicate = pendingInvoiceFixture({
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return { items: listAttempt === 1 ? [duplicate] : [], nextCursor: null };
    });
    mockCommand("resolve_duplicate", null);

    renderAppAt("/inbox?status=suspected_duplicate");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "删除重复" }));

    await waitFor(() => {
      expect(
        screen.queryByRole("dialog", { name: "票据详情" }),
      ).not.toBeInTheDocument();
    });
    const selectedTab = screen.getByRole("tab", { name: "疑似重复" });
    expect(selectedTab).toHaveAttribute("aria-selected", "true");
    expect(selectedTab).toHaveFocus();
  });

  it("focuses the previous visible row after save removes the last item", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const first = pendingInvoiceFixture();
    const second = pendingInvoiceFixture({
      id: "invoice-hotel",
      originalName: "酒店住宿发票.pdf",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return { items: listAttempt === 1 ? [first, second] : [first], nextCursor: null };
    });
    mockCommand(
      "review_item",
      pendingInvoiceFixture({
        ...second,
        status: "ready",
        confirmationStatus: "confirmed",
        updatedAt: "2026-07-15T10:00:00+08:00",
      }),
    );

    renderAppAt("/inbox?status=pending_confirmation");
    await user.click(await screen.findByRole("button", { name: "酒店住宿发票.pdf" }));
    await user.click(screen.getByRole("button", { name: "保存并确认" }));
    await waitFor(() => {
      expect(screen.queryByRole("button", { name: "酒店住宿发票.pdf" })).not.toBeInTheDocument();
    });
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "出租车电子发票.pdf" })).toHaveFocus();
    });
  });

  it("focuses the selected status tab after save removes the only visible row", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const pendingItem = pendingInvoiceFixture();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return { items: listAttempt === 1 ? [pendingItem] : [], nextCursor: null };
    });
    mockCommand(
      "review_item",
      pendingInvoiceFixture({
        status: "ready",
        confirmationStatus: "confirmed",
        updatedAt: "2026-07-15T10:00:00+08:00",
      }),
    );

    renderAppAt("/inbox?status=pending_confirmation");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "保存并确认" }));
    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: "出租车电子发票.pdf" }),
      ).not.toBeInTheDocument();
    });

    const selectedTab = screen.getByRole("tab", { name: "待确认" });
    expect(selectedTab).toHaveAttribute("aria-selected", "true");
    await waitFor(() => expect(selectedTab).toHaveFocus());
  });

  it("focuses the selected status tab when keep removes the only visible row", async () => {
    const user = userEvent.setup();
    let listAttempt = 0;
    const duplicate = pendingInvoiceFixture({
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      listAttempt += 1;
      return { items: listAttempt === 1 ? [duplicate] : [], nextCursor: null };
    });
    mockCommand(
      "resolve_duplicate",
      pendingInvoiceFixture({
        status: "pending_confirmation",
        dedupeStatus: "resolved",
        updatedAt: "2026-07-15T10:00:00+08:00",
      }),
    );

    renderAppAt("/inbox?status=suspected_duplicate");
    await user.click(await screen.findByRole("button", { name: "出租车电子发票.pdf" }));
    await user.click(screen.getByRole("button", { name: "确认保留" }));
    await waitFor(() => {
      expect(screen.queryByRole("button", { name: "出租车电子发票.pdf" })).not.toBeInTheDocument();
    });
    await user.click(screen.getByRole("button", { name: "关闭票据详情" }));

    const selectedTab = screen.getByRole("tab", { name: "疑似重复" });
    expect(selectedTab).toHaveAttribute("aria-selected", "true");
    expect(selectedTab).toHaveFocus();
  });

  it("does not reopen a closed drawer when a save response arrives", async () => {
    const user = userEvent.setup();
    let resolveSave: ((item: InvoiceItemDto) => void) | undefined;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });
    mockCommand(
      "review_item",
      new Promise<InvoiceItemDto>((resolve) => {
        resolveSave = resolve;
      }),
    );

    renderAppAt("/inbox");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "保存并确认" }));
    await user.click(screen.getByRole("button", { name: "关闭票据详情" }));

    await act(async () => {
      resolveSave?.(
        pendingInvoiceFixture({
          status: "ready",
          confirmationStatus: "confirmed",
        }),
      );
    });

    expect(screen.queryByRole("dialog", { name: "票据详情" })).not.toBeInTheDocument();
  });

  it("keeps a reopened drawer isolated from an older save response", async () => {
    const user = userEvent.setup();
    let resolveSave: ((item: InvoiceItemDto) => void) | undefined;
    let serverItem = pendingInvoiceFixture();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => ({ items: [serverItem], nextCursor: null }));
    mockCommand(
      "review_item",
      new Promise<InvoiceItemDto>((resolve) => {
        resolveSave = (item) => {
          serverItem = item;
          resolve(item);
        };
      }),
    );

    renderAppAt("/inbox");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "保存并确认" }));
    await user.click(screen.getByRole("button", { name: "关闭票据详情" }));
    await user.click(screen.getByRole("button", { name: "出租车电子发票.pdf" }));

    await act(async () => {
      resolveSave?.(
        pendingInvoiceFixture({
          originalName: "旧保存响应.pdf",
          status: "ready",
          confirmationStatus: "confirmed",
          updatedAt: "2026-07-15T09:44:00+08:00",
        }),
      );
    });

    expect(await screen.findByText("旧保存响应.pdf")).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "票据详情" })).toHaveTextContent(
      "出租车电子发票.pdf",
    );
  });

  it("keeps a reopened drawer isolated from an older recognition response", async () => {
    const user = userEvent.setup();
    let resolveRetry: ((item: InvoiceItemDto) => void) | undefined;
    let serverItem = pendingInvoiceFixture();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => ({ items: [serverItem], nextCursor: null }));
    mockCommand(
      "retry_recognition",
      new Promise<InvoiceItemDto>((resolve) => {
        resolveRetry = (item) => {
          serverItem = item;
          resolve(item);
        };
      }),
    );

    renderAppAt("/inbox");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "重新识别" }));
    await user.click(screen.getByRole("button", { name: "关闭票据详情" }));
    await user.click(screen.getByRole("button", { name: "出租车电子发票.pdf" }));

    await act(async () => {
      resolveRetry?.(
        pendingInvoiceFixture({
          originalName: "旧识别响应.pdf",
          status: "pending_recognition",
          recognitionStatus: "pending",
          updatedAt: "2026-07-15T09:44:00+08:00",
        }),
      );
    });

    expect(await screen.findByText("旧识别响应.pdf")).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "票据详情" })).toHaveTextContent(
      "出租车电子发票.pdf",
    );
  });

  it("keeps a reopened drawer open when an older deletion response arrives", async () => {
    const user = userEvent.setup();
    let resolveDeletion: ((item: null) => void) | undefined;
    const duplicate = pendingInvoiceFixture({
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    let serverItems = [duplicate];
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => ({ items: serverItems, nextCursor: null }));
    mockCommand(
      "resolve_duplicate",
      new Promise<null>((resolve) => {
        resolveDeletion = (item) => {
          serverItems = [];
          resolve(item);
        };
      }),
    );

    renderAppAt("/inbox?status=suspected_duplicate");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "删除重复" }));
    await user.click(screen.getByRole("button", { name: "关闭票据详情" }));
    await user.click(screen.getByRole("button", { name: "出租车电子发票.pdf" }));

    await act(async () => {
      resolveDeletion?.(null);
    });

    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: "出租车电子发票.pdf" }),
      ).not.toBeInTheDocument();
    });
    expect(screen.getByRole("dialog", { name: "票据详情" })).toHaveTextContent(
      "出租车电子发票.pdf",
    );
  });

  it("does not close a newer drawer when an older deletion response arrives", async () => {
    const user = userEvent.setup();
    let resolveDeletion: ((item: null) => void) | undefined;
    const firstDuplicate = pendingInvoiceFixture({
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    const secondDuplicate = pendingInvoiceFixture({
      id: "invoice-duplicate-two",
      originalName: "出租车电子发票-副本.pdf",
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [firstDuplicate, secondDuplicate],
      nextCursor: null,
    });
    mockCommand(
      "resolve_duplicate",
      new Promise<null>((resolve) => {
        resolveDeletion = resolve;
      }),
    );

    renderAppAt("/inbox?status=suspected_duplicate");
    await user.click(
      await screen.findByRole("button", { name: "出租车电子发票.pdf" }),
    );
    await user.click(screen.getByRole("button", { name: "删除重复" }));
    await user.click(screen.getByRole("button", { name: "关闭票据详情" }));
    await user.click(
      screen.getByRole("button", { name: "出租车电子发票-副本.pdf" }),
    );

    await act(async () => {
      resolveDeletion?.(null);
    });

    expect(screen.getByRole("dialog", { name: "票据详情" })).toHaveTextContent(
      "出租车电子发票-副本.pdf",
    );
  });

  it("retries recognition from the item drawer", async () => {
    const user = userEvent.setup();
    const failedItem = pendingInvoiceFixture({
      status: "recognition_failed",
      recognitionStatus: "failed",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", { items: [failedItem], nextCursor: null });
    mockCommand(
      "retry_recognition",
      pendingInvoiceFixture({
        status: "pending_recognition",
        recognitionStatus: "pending",
      }),
    );

    renderAppAt("/inbox?status=recognition_failed");
    await user.click(await screen.findByText("出租车电子发票.pdf"));
    await user.click(screen.getByRole("button", { name: "重新识别" }));

    await waitFor(() => {
      expect(commandCalls("retry_recognition")).toEqual([
        { itemId: "invoice-taxi" },
      ]);
    });
  });

  it("keeps or deletes suspected duplicates from the drawer", async () => {
    const user = userEvent.setup();
    const duplicateOne = pendingInvoiceFixture({
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    const duplicateTwo = pendingInvoiceFixture({
      id: "invoice-duplicate-two",
      originalName: "出租车电子发票-副本.pdf",
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    let serverItems = [duplicateOne, duplicateTwo];
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", (arguments_) => {
      const { status } = (arguments_ as { filter: { status?: string } }).filter;
      return {
        items: serverItems.filter((item) => !status || item.status === status),
        nextCursor: null,
      };
    });
    mockCommand("resolve_duplicate", (arguments_) => {
      const request = arguments_ as { itemId: string; keep: boolean };
      if (!request.keep) {
        serverItems = serverItems.filter((item) => item.id !== request.itemId);
        return null;
      }
      const kept = pendingInvoiceFixture({
        status: "pending_confirmation",
        dedupeStatus: "resolved",
      });
      serverItems = serverItems.map((item) =>
        item.id === request.itemId ? kept : item,
      );
      return kept;
    });

    renderAppAt("/inbox?status=suspected_duplicate");
    await user.click(await screen.findByText("出租车电子发票.pdf"));
    await user.click(screen.getByRole("button", { name: "确认保留" }));
    await waitFor(() => {
      expect(commandCalls("resolve_duplicate")[0]).toEqual({
        itemId: "invoice-taxi",
        keep: true,
      });
    });

    await user.click(screen.getByText("出租车电子发票-副本.pdf"));
    await user.click(screen.getByRole("button", { name: "删除重复" }));
    await waitFor(() => {
      expect(screen.queryByText("出租车电子发票-副本.pdf")).not.toBeInTheDocument();
    });
    expect(commandCalls("resolve_duplicate")[1]).toEqual({
      itemId: "invoice-duplicate-two",
      keep: false,
    });
  });

  it("does not append a stale response after filters change", async () => {
    const user = userEvent.setup();
    let resolveOldRequest:
      | ((value: {
          items: InvoiceItemDto[];
          nextCursor: null;
        }) => void)
      | undefined;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", (arguments_) => {
      const filter = (arguments_ as { filter: Record<string, unknown> }).filter;
      if (filter.category === "transport") {
        return {
          items: [pendingInvoiceFixture({ suggestedCategory: "transport" })],
          nextCursor: null,
        };
      }
      return new Promise<{ items: InvoiceItemDto[]; nextCursor: null }>((resolve) => {
        resolveOldRequest = resolve;
      });
    });

    renderAppAt("/inbox");
    expect(screen.getByRole("columnheader", { name: "原件" })).toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("分类筛选"), "transport");
    expect(await screen.findByText("出租车电子发票.pdf")).toBeInTheDocument();

    await act(async () => {
      resolveOldRequest?.({
        items: [
          pendingInvoiceFixture({
            id: "invoice-stale",
            originalName: "旧筛选结果.pdf",
          }),
        ],
        nextCursor: null,
      });
    });
    expect(screen.queryByText("旧筛选结果.pdf")).not.toBeInTheDocument();
  });

  it("keeps the table header stable through error and retry", async () => {
    const user = userEvent.setup();
    let attempts = 0;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", () => {
      attempts += 1;
      if (attempts === 1) {
        return Promise.reject({
          code: "external",
          service: "database",
          retryable: true,
          message: "待处理池暂时不可用",
        });
      }
      return { items: [], nextCursor: null };
    });

    renderAppAt("/inbox");
    expect(screen.getByRole("columnheader", { name: "原件" })).toBeInTheDocument();
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "待处理池暂时不可用",
    );
    await user.click(screen.getByRole("button", { name: "重试" }));

    expect(await screen.findByText("当前筛选下没有票据")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "批次" })).toBeInTheDocument();
  });

  it("opens the suspected-duplicate filter after importing a duplicate", async () => {
    const user = userEvent.setup();
    const serverItems: InvoiceItemDto[] = [];
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", (arguments_) => {
      const { status } = (arguments_ as { filter: { status?: string } }).filter;
      return {
        items: serverItems.filter((item) => !status || item.status === status),
        nextCursor: null,
      };
    });
    openDialogMock.mockResolvedValue(["/Users/finance/重复票据.pdf"]);
    mockCommand("import_manual_files", () => {
      const imported = pendingInvoiceFixture({
        id: "invoice-imported-duplicate",
        originalName: "重复票据.pdf",
        sourceType: "manual_upload",
        status: "suspected_duplicate",
        dedupeStatus: "suspected_duplicate",
      });
      serverItems.push(imported);
      return [{
        status: "imported" as const,
        path: "/Users/finance/重复票据.pdf",
        item: imported,
      }];
    });

    renderAppAt("/inbox");
    await screen.findByText("当前筛选下没有票据");
    await user.click(screen.getByRole("button", { name: "选择文件" }));

    await waitFor(() => {
      expect(window.location.search).toBe("?status=suspected_duplicate");
    });
    expect(screen.getByRole("tab", { name: "疑似重复" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(await screen.findByText("重复票据.pdf")).toBeInTheDocument();
  });
});
