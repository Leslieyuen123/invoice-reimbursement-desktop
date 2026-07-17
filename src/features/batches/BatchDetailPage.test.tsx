import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ revealItemInDir: vi.fn() }));
vi.mock("@tauri-apps/api/path", () => ({
  join: vi.fn(async (...parts: string[]) => parts.join("/")),
}));

import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { App } from "../../app/App";
import {
  invokeMock,
  mockCommand,
  resetMockApi,
} from "../../test/mockApi";
import type {
  BatchCandidateDto,
  BatchDetailDto,
  BatchDto,
  InvoiceItemDto,
} from "../../types";

const revealMock = vi.mocked(revealItemInDir);

function batchFixture(overrides: Partial<BatchDto> = {}): BatchDto {
  return {
    id: "batch-summer",
    name: "6-8 月整理批次",
    startDate: "2026-06-01",
    endDate: "2026-08-31",
    status: "draft",
    itemCount: 0,
    totalAmountCents: 0,
    unconfirmedCount: 0,
    note: null,
    createdAt: "2026-07-17T09:00:00+08:00",
    updatedAt: "2026-07-17T09:00:00+08:00",
    lastExportedAt: null,
    ...overrides,
  };
}

function itemFixture(overrides: Partial<InvoiceItemDto> = {}): InvoiceItemDto {
  return {
    id: "invoice-train",
    originalName: "高铁电子发票.pdf",
    previewUrl: "invoice-file://item/invoice-train?variant=normalized",
    sourceType: "email",
    sourceAccountId: "account-finance",
    fetchedAt: "2026-07-15T09:45:00+08:00",
    invoiceDate: "2026-07-14",
    suggestedPeriod: "2026-07",
    batchId: null,
    suggestedCategory: "transport",
    finalCategory: "transport",
    amountCents: 12_850,
    currency: "CNY",
    city: "上海",
    company: "中国铁路上海局集团有限公司",
    status: "ready",
    recognitionStatus: "succeeded",
    confirmationStatus: "confirmed",
    dedupeStatus: "unique",
    note: null,
    eventTag: null,
    projectTag: null,
    createdAt: "2026-07-15T09:45:00+08:00",
    updatedAt: "2026-07-15T09:45:00+08:00",
    ...overrides,
  };
}

function detailFixture(items: InvoiceItemDto[] = []): BatchDetailDto {
  const totalAmountCents = items.reduce(
    (total, item) => total + (item.amountCents ?? 0),
    0,
  );
  return {
    batch: batchFixture({
      itemCount: items.length,
      totalAmountCents,
    }),
    items,
    summary: {
      itemCount: items.length,
      totalAmountCents,
      transport: { itemCount: items.length, amountCents: totalAmountCents },
      dining: { itemCount: 0, amountCents: 0 },
      accommodation: { itemCount: 0, amountCents: 0 },
      hospitality: { itemCount: 0, amountCents: 0 },
      unconfirmedCount: 0,
    },
    warnings: [],
  };
}

function candidateFixture(
  item: InvoiceItemDto,
  overrides: Partial<BatchCandidateDto> = {},
): BatchCandidateDto {
  return {
    item,
    outsideBatchRange: false,
    eligible: true,
    disabledReason: null,
    ...overrides,
  };
}

function mockBatchCommands() {
  const recommendedItem = itemFixture();
  let detail = detailFixture();
  mockCommand("get_dashboard", new Promise(() => undefined));
  mockCommand("create_custom_batch", batchFixture());
  mockCommand("get_batch", () => detail);
  mockCommand("list_batch_candidates", {
    items: [candidateFixture(recommendedItem)],
    nextCursor: null,
  });
  mockCommand("assign_items_to_batch", () => {
    detail = detailFixture([{ ...recommendedItem, batchId: "batch-summer" }]);
    return detail;
  });
  mockCommand("export_batch", {
    directory: "/Users/finance/6-8 月整理批次-20260717",
    itemCount: 1,
    totalAmountCents: 12_850,
  });
}

function renderAppAt(path: string) {
  window.history.replaceState({}, "", path);
  return render(<App />);
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

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function mockDetail(detail: BatchDetailDto, candidates: BatchCandidateDto[] = []) {
  let currentDetail = detail;
  mockCommand("get_dashboard", new Promise(() => undefined));
  mockCommand("get_batch", () => currentDetail);
  mockCommand("list_batch_candidates", {
    items: candidates,
    nextCursor: null,
  });
  mockCommand("assign_items_to_batch", (arguments_) => {
    const ids = (arguments_ as { itemIds: string[] }).itemIds;
    const assigned = candidates
      .filter((candidate) => ids.includes(candidate.item.id))
      .map((candidate) => ({ ...candidate.item, batchId: detail.batch.id }));
    currentDetail = detailFixture([...detail.items, ...assigned]);
    return currentDetail;
  });
  mockCommand("remove_item_from_batch", () => {
    currentDetail = detailFixture([]);
    return currentDetail;
  });
}

describe("Batch workspace", () => {
  beforeEach(() => {
    cleanup();
    resetMockApi();
    revealMock.mockReset();
    revealMock.mockResolvedValue(undefined);
  });

  it("creates a custom range, assigns recommendations and exports", async () => {
    const user = userEvent.setup();
    mockBatchCommands();
    renderAppAt("/batches/new");
    await user.click(screen.getByRole("radio", { name: "自定义范围" }));
    await user.type(screen.getByLabelText("批次名称"), "6-8 月整理批次");
    await user.type(screen.getByLabelText("开始日期"), "2026-06-01");
    await user.type(screen.getByLabelText("结束日期"), "2026-08-31");
    await user.click(screen.getByRole("button", { name: "创建批次" }));
    expect(commandCalls("create_custom_batch")[0].endDate).toBe("2026-08-31");
    await screen.findByRole("button", { name: "加入推荐票据" });
    await user.click(screen.getByRole("button", { name: "加入推荐票据" }));
    await user.click(screen.getByRole("button", { name: "导出报销包" }));
    expect(await screen.findByText(/merged\.pdf/)).toBeInTheDocument();
  });

  it("defaults monthly creation to the current year and month", async () => {
    const user = userEvent.setup();
    const now = new Date();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("create_month_batch", batchFixture({ name: "本月报销" }));

    renderAppAt("/batches/new");

    expect(screen.getByRole("radio", { name: "按月" })).toBeChecked();
    expect(screen.getByLabelText("年份")).toHaveValue(now.getFullYear());
    expect(screen.getByLabelText("月份")).toHaveValue(String(now.getMonth() + 1));
    await user.click(screen.getByRole("button", { name: "创建批次" }));

    await waitFor(() => {
      expect(commandCalls("create_month_batch")).toEqual([
        { year: now.getFullYear(), month: now.getMonth() + 1 },
      ]);
    });
  });

  it("keeps batch history bounded to one stable cursor page", async () => {
    const user = userEvent.setup();
    const nextCursor = {
      sortValue: "2026-07-16T09:00:00+08:00",
      id: "batch-page-one",
    };
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_batches", (arguments_) => {
      const page = (arguments_ as { page: { cursor?: typeof nextCursor } }).page;
      return page.cursor
        ? {
            items: [
              batchFixture({
                id: "batch-page-two",
                name: "第二页批次",
                totalAmountCents: Number.MAX_SAFE_INTEGER,
              }),
            ],
            nextCursor: null,
          }
        : {
            items: [batchFixture({ id: "batch-page-one", name: "第一页批次" })],
            nextCursor,
          };
    });

    renderAppAt("/batches");
    expect(await screen.findByText("第一页批次")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "下一页" }));
    expect(await screen.findByText("第二页批次")).toBeInTheDocument();
    expect(screen.getByText("¥90071992547409.91")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "下一页" })).toBeDisabled();
    expect(commandCalls("list_batches")).toHaveLength(2);
    expect(commandCalls("list_batches")[0]).toMatchObject({ page: { pageSize: 50 } });
    expect(commandCalls("list_batches")[1]).toMatchObject({
      page: { cursor: nextCursor, pageSize: 50 },
    });
  });

  it("retains the batch list frame while loading and then shows empty state", async () => {
    const response = deferred<{ items: BatchDto[]; nextCursor: null }>();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_batches", response.promise);

    renderAppAt("/batches");
    expect(screen.getByText("日期范围")).toBeInTheDocument();
    expect(screen.getByRole("status", { name: "正在加载批次" })).toBeInTheDocument();
    await act(async () => response.resolve({ items: [], nextCursor: null }));
    expect(await screen.findByText("尚未创建批次")).toBeInTheDocument();
    expect(screen.getByText("日期范围")).toBeInTheDocument();
  });

  it("retries a failed batch list without changing its table frame", async () => {
    const user = userEvent.setup();
    let attempt = 0;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_batches", () => {
      attempt += 1;
      return attempt === 1
        ? Promise.reject({ code: "internal", message: "批次数据库忙" })
        : { items: [], nextCursor: null };
    });

    renderAppAt("/batches");
    expect(await screen.findByRole("alert")).toHaveTextContent("批次数据库忙");
    expect(screen.getByText("日期范围")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "重试" }));
    expect(await screen.findByText("尚未创建批次")).toBeInTheDocument();
    expect(attempt).toBe(2);
  });

  it("loads recommended candidates, marks disabled and outside-range items, then assigns", async () => {
    const user = userEvent.setup();
    const eligible = itemFixture();
    const duplicate = itemFixture({
      id: "invoice-duplicate",
      originalName: "疑似重复发票.pdf",
      status: "suspected_duplicate",
      dedupeStatus: "suspected_duplicate",
    });
    const failed = itemFixture({
      id: "invoice-failed",
      originalName: "识别失败发票.pdf",
      status: "recognition_failed",
      recognitionStatus: "failed",
    });
    const outside = itemFixture({
      id: "invoice-outside",
      originalName: "苏州酒店发票.pdf",
      invoiceDate: "2026-09-01",
    });
    mockDetail(detailFixture(), [
      candidateFixture(eligible),
      candidateFixture(duplicate, {
        eligible: false,
        disabledReason: "suspected_duplicate",
      }),
      candidateFixture(failed, {
        eligible: false,
        disabledReason: "recognition_failed",
      }),
    ]);
    mockCommand("list_batch_candidates", (arguments_) => {
      const query = (arguments_ as { query?: string }).query;
      return {
        items: query
          ? [candidateFixture(outside, { outsideBatchRange: true })]
          : [
              candidateFixture(eligible),
              candidateFixture(duplicate, {
                eligible: false,
                disabledReason: "suspected_duplicate",
              }),
              candidateFixture(failed, {
                eligible: false,
                disabledReason: "recognition_failed",
              }),
            ],
        nextCursor: null,
      };
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const dialog = screen.getByRole("dialog", { name: "调整票据归属" });
    expect(await within(dialog).findByText("高铁电子发票.pdf")).toBeInTheDocument();
    expect(within(dialog).getByRole("checkbox", { name: "疑似重复发票.pdf" })).toBeDisabled();
    expect(within(dialog).getByText("疑似重复，不能归属")).toBeInTheDocument();
    expect(within(dialog).getByRole("checkbox", { name: "识别失败发票.pdf" })).toBeDisabled();
    expect(within(dialog).getByText("识别失败，不能归属")).toBeInTheDocument();

    await user.type(within(dialog).getByLabelText("搜索未归属票据"), "苏州");
    await user.click(within(dialog).getByRole("button", { name: "搜索" }));
    expect(await within(dialog).findByText("苏州酒店发票.pdf")).toBeInTheDocument();
    expect(within(dialog).getByText("日期超出批次范围")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("checkbox", { name: "苏州酒店发票.pdf" }));
    await user.click(within(dialog).getByRole("button", { name: "归属所选票据" }));

    await waitFor(() => {
      expect(commandCalls("assign_items_to_batch")[0]).toEqual({
        batchId: "batch-summer",
        itemIds: ["invoice-outside"],
      });
    });
  });

  it("does not let an earlier dialog session replace reopened candidate results", async () => {
    const user = userEvent.setup();
    const first = deferred<{ items: BatchCandidateDto[]; nextCursor: null }>();
    const second = deferred<{ items: BatchCandidateDto[]; nextCursor: null }>();
    let attempt = 0;
    mockDetail(detailFixture());
    mockCommand("list_batch_candidates", () => {
      attempt += 1;
      return attempt === 1 ? first.promise : second.promise;
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    await user.click(screen.getByRole("button", { name: "关闭调整票据" }));
    await user.click(screen.getByRole("button", { name: "调整票据" }));

    await act(async () => {
      first.resolve({
        items: [candidateFixture(itemFixture({ originalName: "旧会话票据.pdf" }))],
        nextCursor: null,
      });
    });
    expect(screen.queryByText("旧会话票据.pdf")).not.toBeInTheDocument();
    await act(async () => {
      second.resolve({
        items: [candidateFixture(itemFixture({ originalName: "新会话票据.pdf" }))],
        nextCursor: null,
      });
    });
    expect(await screen.findByText("新会话票据.pdf")).toBeInTheDocument();
  });

  it("requires an accessible confirmation before removing an assigned item", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture([{ ...itemFixture(), batchId: "batch-summer" }]));

    renderAppAt("/batches/batch-summer");
    const assignedItems = await screen.findByRole("region", { name: "已归属票据" });
    expect(within(assignedItems).getByText("交通")).toBeInTheDocument();
    await user.click(
      await screen.findByRole("button", { name: "移出 高铁电子发票.pdf" }),
    );
    const confirmation = screen.getByRole("dialog", { name: "确认移出票据" });
    await user.click(within(confirmation).getByRole("button", { name: "确认移出" }));

    await waitFor(() => {
      expect(commandCalls("remove_item_from_batch")[0]).toEqual({
        batchId: "batch-summer",
        itemId: "invoice-train",
      });
    });
  });

  it("blocks export until every assigned item is confirmed", async () => {
    mockDetail({
      ...detailFixture([itemFixture({ confirmationStatus: "pending" })]),
      batch: batchFixture({ itemCount: 1, unconfirmedCount: 1 }),
      summary: {
        ...detailFixture([itemFixture()]).summary,
        unconfirmedCount: 1,
      },
    });

    renderAppAt("/batches/batch-summer");

    expect(await screen.findByRole("button", { name: "导出报销包" })).toBeDisabled();
    expect(screen.getByRole("link", { name: "查看待确认票据" })).toHaveAttribute(
      "href",
      "/inbox?status=pending_confirmation&batchId=batch-summer",
    );
  });

  it("shows a retryable export failure and succeeds on retry", async () => {
    const user = userEvent.setup();
    let attempt = 0;
    mockDetail(detailFixture());
    mockCommand("export_batch", () => {
      attempt += 1;
      if (attempt === 1) {
        return Promise.reject({
          code: "external",
          service: "filesystem",
          retryable: true,
          message: "目标文件夹暂时不可写",
        });
      }
      return {
        directory: "/Users/finance/retry",
        itemCount: 0,
        totalAmountCents: 0,
      };
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("目标文件夹暂时不可写");
    await user.click(screen.getByRole("button", { name: "重试导出" }));
    expect(await screen.findByText("manifest.json")).toBeInTheDocument();
    expect(commandCalls("export_batch")).toHaveLength(2);
  });

  it("shows a deterministic export stage and reports reveal failures safely", async () => {
    const user = userEvent.setup();
    const exportRequest = deferred<{
      directory: string;
      itemCount: number;
      totalAmountCents: number;
    }>();
    mockDetail(detailFixture());
    mockCommand("export_batch", exportRequest.promise);
    revealMock.mockRejectedValue(new Error("Finder unavailable"));

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    expect(screen.getByRole("button", { name: "正在生成报销文件" })).toBeDisabled();
    expect(screen.queryByText(/%/)).not.toBeInTheDocument();
    await act(async () => {
      exportRequest.resolve({
        directory: "/Users/finance/export",
        itemCount: 1,
        totalAmountCents: Number.MAX_SAFE_INTEGER,
      });
    });
    expect(await screen.findByText(/¥90071992547409\.91/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "在文件夹中显示" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("无法在文件夹中显示");
  });

  it("ignores an export result after navigating to another route", async () => {
    const user = userEvent.setup();
    const exportRequest = deferred<{
      directory: string;
      itemCount: number;
      totalAmountCents: number;
    }>();
    mockDetail(detailFixture());
    mockCommand("export_batch", exportRequest.promise);
    mockCommand("list_batches", { items: [], nextCursor: null });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    await user.click(screen.getByText("报销批次", { selector: ".batch-back-link" }));
    expect(await screen.findByText("尚未创建批次")).toBeInTheDocument();
    await act(async () => {
      exportRequest.resolve({
        directory: "/Users/finance/stale",
        itemCount: 0,
        totalAmountCents: 0,
      });
    });
    expect(screen.queryByText("merged.pdf")).not.toBeInTheDocument();
  });
});
