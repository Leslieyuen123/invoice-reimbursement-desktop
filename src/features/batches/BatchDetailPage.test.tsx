import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { StrictMode } from "react";
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
  BatchAutomationResultDto,
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
    sourceReceivedDate: null,
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
    hasNormalizedPdf: true,
    note: null,
    eventTag: null,
    projectTag: null,
    createdAt: "2026-07-15T09:45:00+08:00",
    updatedAt: "2026-07-15T09:45:00+08:00",
    ...overrides,
  };
}

function detailFixture(
  items: InvoiceItemDto[] = [],
  summaryOverrides: Partial<BatchDetailDto["summary"]> = {},
): BatchDetailDto {
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
      ...summaryOverrides,
    },
    warnings: [],
    issues: [],
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

function automationFixture(
  overrides: Partial<BatchAutomationResultDto> = {},
): BatchAutomationResultDto {
  return {
    scannedAccountCount: 1,
    failedAccounts: [],
    importedCount: 1,
    assignedCount: 1,
    exceptionCount: 0,
    repairedCount: 0,
    export: {
      directory: "/Users/finance/2026-5",
      itemCount: 1,
      totalAmountCents: 12_850,
    },
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

function renderStrictAppAt(path: string) {
  window.history.replaceState({}, "", path);
  return render(
    <StrictMode>
      <App />
    </StrictMode>,
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
    await user.click(
      screen.getByRole("checkbox", { name: "创建后自动处理并导出" }),
    );
    await user.click(screen.getByRole("button", { name: "创建批次" }));
    expect(commandCalls("create_custom_batch")[0].endDate).toBe("2026-08-31");
    await screen.findByRole("button", { name: "加入推荐票据" });
    await user.click(screen.getByRole("button", { name: "加入推荐票据" }));
    await user.click(screen.getByRole("button", { name: "导出报销包" }));
    expect(await screen.findByText(/merged\.pdf/)).toBeInTheDocument();
  });

  it("moves the batch range without rebuilding the batch", async () => {
    const user = userEvent.setup();
    let detail = detailFixture([itemFixture()]);
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_batch", () => detail);
    mockCommand("update_batch_range", (arguments_) => {
      const input = (arguments_ as { input: { startDate: string; endDate: string } })
        .input;
      expect(input).toEqual({
        startDate: "2026-06-01",
        endDate: "2026-09-15",
      });
      detail = {
        ...detail,
        batch: {
          ...detail.batch,
          endDate: "2026-09-15",
          status: "draft",
        },
      };
      return detail;
    });

    renderAppAt("/batches/batch-summer");

    await user.click(
      await screen.findByRole("button", { name: "编辑批次范围" }),
    );
    const endDate = screen.getByLabelText("结束日期");
    await user.clear(endDate);
    await user.type(endDate, "2026-09-15");
    await user.click(screen.getByRole("button", { name: "保存范围" }));

    await waitFor(() =>
      expect(commandCalls("update_batch_range")).toHaveLength(1),
    );
    expect(
      await screen.findByText("2026-06-01 至 2026-09-15"),
    ).toBeInTheDocument();
  });

  it("removes the selected invoices in one bulk call", async () => {
    const user = userEvent.setup();
    const first = itemFixture({ id: "invoice-one", originalName: "一号.pdf" });
    const second = itemFixture({ id: "invoice-two", originalName: "二号.pdf" });
    mockCommand("get_dashboard", new Promise(() => undefined));
    let detail = detailFixture([first, second]);
    mockCommand("get_batch", () => detail);
    mockCommand("remove_batch_items", (arguments_) => {
      const removed = (arguments_ as { itemIds: string[] }).itemIds;
      expect(removed).toEqual(["invoice-one"]);
      detail = detailFixture([second]);
      return detail;
    });

    renderAppAt("/batches/batch-summer");
    await user.click(
      await screen.findByRole("checkbox", { name: "选择 一号.pdf" }),
    );

    expect(screen.getByText("已选 1 张")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "移出所选票据" }));

    await waitFor(() =>
      expect(commandCalls("remove_batch_items")).toHaveLength(1),
    );
    expect(
      await screen.findByRole("checkbox", { name: "选择 二号.pdf" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("checkbox", { name: "选择 一号.pdf" }),
    ).not.toBeInTheDocument();
  });

  it("settles derivable invoices and reports the ones that still need work", async () => {
    const user = userEvent.setup();
    const pending = itemFixture({
      id: "invoice-pending",
      originalName: "待确认.pdf",
      status: "pending_confirmation",
      confirmationStatus: "pending",
      finalCategory: null,
      suggestedCategory: "dining",
    });
    const noAmount = itemFixture({
      id: "invoice-no-amount",
      originalName: "缺金额.pdf",
      status: "pending_confirmation",
      confirmationStatus: "pending",
      amountCents: null,
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    let detail = detailFixture([pending, noAmount], { unconfirmedCount: 2 });
    mockCommand("get_batch", () => detail);
    mockCommand("settle_batch_items", () => {
      detail = detailFixture(
        [
          { ...pending, confirmationStatus: "confirmed", status: "ready" },
          noAmount,
        ],
        { unconfirmedCount: 1 },
      );
      return {
        confirmedCount: 1,
        filledInvoiceDateCount: 1,
        appliedCategoryCount: 1,
        repairedCount: 0,
        skipped: [
          {
            itemId: "invoice-no-amount",
            fileName: "缺金额.pdf",
            code: "missing_amount",
            message: "缺少金额，必须对照原件人工填写",
          },
        ],
      };
    });

    renderAppAt("/batches/batch-summer");
    await user.click(
      await screen.findByRole("button", { name: "清空待确认（2）" }),
    );
    await user.click(
      screen.getByRole("button", { name: "确认可推导的票据" }),
    );

    // commandCalls unwraps the `input` payload of wire commands.
    expect(commandCalls("settle_batch_items")[0]).toEqual({
      fillInvoiceDateFromReceived: true,
      applySuggestedCategory: true,
      defaultCategory: null,
    });
    const report = await screen.findByRole("status", {
      name: "已确认 1 张票据",
    });
    expect(within(report).getByText("1 张按邮件收到日期补齐开票日期")).toBeInTheDocument();
    expect(within(report).getByText("缺金额.pdf")).toBeInTheDocument();
    expect(
      within(report).getByText("缺少金额，必须对照原件人工填写"),
    ).toBeInTheDocument();
  });

  it("automatically processes and exports a newly created monthly batch", async () => {
    const user = userEvent.setup();
    const created = batchFixture({
      id: "batch-may",
      name: "2026 年 5 月报销",
      startDate: "2026-05-01",
      endDate: "2026-05-31",
    });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("create_month_batch", created);
    mockCommand("get_batch", { ...detailFixture(), batch: created });
    mockCommand("run_batch_automation", automationFixture());

    renderAppAt("/batches/new");

    expect(
      screen.getByRole("checkbox", { name: "创建后自动处理并导出" }),
    ).toBeChecked();
    await user.clear(screen.getByLabelText("年份"));
    await user.type(screen.getByLabelText("年份"), "2026");
    await user.selectOptions(screen.getByLabelText("月份"), "5");
    await user.click(screen.getByRole("button", { name: "创建并自动处理" }));

    expect(await screen.findByText("自动处理完成")).toBeInTheDocument();
    expect(commandCalls("create_month_batch")).toEqual([
      { year: 2026, month: 5 },
    ]);
    expect(commandCalls("run_batch_automation")).toEqual([
      { batchId: "batch-may" },
    ]);
    expect(screen.getByText("1 张已自动纳入")).toBeInTheDocument();
  });

  it("creates without automation when the create toggle is off", async () => {
    const user = userEvent.setup();
    const created = batchFixture({ id: "batch-manual" });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("create_month_batch", created);
    mockCommand("get_batch", { ...detailFixture(), batch: created });
    mockCommand("run_batch_automation", automationFixture());

    renderAppAt("/batches/new");
    await user.click(
      screen.getByRole("checkbox", { name: "创建后自动处理并导出" }),
    );
    await user.click(screen.getByRole("button", { name: "创建批次" }));

    expect(
      await screen.findByRole("button", { name: "一键自动处理" }),
    ).toBeEnabled();
    expect(commandCalls("create_month_batch")).toHaveLength(1);
    expect(commandCalls("run_batch_automation")).toHaveLength(0);
  });

  it("shows stable pending feedback and a complete automation summary", async () => {
    const user = userEvent.setup();
    const automation = deferred<BatchAutomationResultDto>();
    mockDetail(detailFixture());
    mockCommand("run_batch_automation", automation.promise);

    renderAppAt("/batches/batch-summer");

    const start = await screen.findByRole("button", { name: "一键自动处理" });
    expect(start).toBeEnabled();
    await user.click(start);
    expect(
      screen.getByRole("status", { name: "正在自动处理批次" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "正在自动处理" }),
    ).toBeDisabled();

    await act(async () => {
      automation.resolve(
        automationFixture({
          exceptionCount: 2,
          failedAccounts: [
            {
              accountId: "account-failed",
              email: "failed@example.com",
              message: "邮箱连接超时",
            },
          ],
          export: {
            directory: "/Users/finance/a/very/long/export/path/2026-05",
            itemCount: 1,
            totalAmountCents: 12_850,
          },
        }),
      );
    });

    const completionStatus = await screen.findByRole("status", {
      name: "批次自动处理完成",
    });
    expect(completionStatus).toHaveAttribute("aria-live", "polite");
    expect(screen.getByText("1 张新导入")).toBeInTheDocument();
    expect(screen.getByText("1 张已自动纳入")).toBeInTheDocument();
    expect(screen.getByText("2 个异常项")).toBeInTheDocument();
    expect(screen.getByText("1 个邮箱失败")).toBeInTheDocument();
    expect(
      screen.getByText("/Users/finance/a/very/long/export/path/2026-05"),
    ).toBeInTheDocument();
    await waitFor(() => expect(commandCalls("get_batch").length).toBeGreaterThan(1));
  });

  it("reveals the automation export from its completion summary", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture());
    mockCommand(
      "run_batch_automation",
      automationFixture({
        export: {
          directory: "/Users/finance/automated/2026-05",
          itemCount: 1,
          totalAmountCents: 12_850,
        },
      }),
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));

    const completion = await screen.findByRole("status", {
      name: "批次自动处理完成",
    });
    const reveal = within(completion).getByRole("button", {
      name: "在文件夹中显示",
    });
    expect(screen.queryByText("报销包已生成")).not.toBeInTheDocument();
    await user.click(reveal);

    expect(revealMock).toHaveBeenCalledWith(
      "/Users/finance/automated/2026-05/merged.pdf",
    );
  });

  it("reports automation export reveal failures in its completion summary", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture());
    mockCommand(
      "run_batch_automation",
      automationFixture({
        export: {
          directory: "/Users/finance/automated/2026-05",
          itemCount: 1,
          totalAmountCents: 12_850,
        },
      }),
    );
    revealMock.mockRejectedValue(new Error("Finder unavailable"));

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));

    const completion = await screen.findByRole("status", {
      name: "批次自动处理完成",
    });
    await user.click(
      within(completion).getByRole("button", { name: "在文件夹中显示" }),
    );

    expect(await within(completion).findByRole("alert")).toHaveTextContent(
      "无法在文件夹中显示",
    );
    expect(screen.queryByText("报销包已生成")).not.toBeInTheDocument();
  });

  it("keeps automation and manual reveal failures with their own export result", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture());
    mockCommand("run_batch_automation", automationFixture());
    mockCommand("export_batch", {
      directory: "/Users/finance/manual/2026-05",
      itemCount: 1,
      totalAmountCents: 12_850,
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));
    const automationResult = await screen.findByRole("status", {
      name: "批次自动处理完成",
    });
    await user.click(screen.getByRole("button", { name: "导出报销包" }));
    const manualResult = (await screen.findByText("报销包已生成")).closest<HTMLElement>(
      ".batch-export-result",
    );
    if (!manualResult) throw new Error("manual export result is missing");

    expect(automationResult).toBeVisible();
    expect(manualResult).toBeVisible();

    revealMock.mockRejectedValueOnce(new Error("automation Finder unavailable"));
    await user.click(
      within(automationResult).getByRole("button", { name: "在文件夹中显示" }),
    );
    expect(await within(automationResult).findByRole("alert")).toHaveTextContent(
      "无法在文件夹中显示",
    );
    expect(within(manualResult).queryByRole("alert")).not.toBeInTheDocument();

    revealMock.mockResolvedValueOnce(undefined);
    await user.click(
      within(automationResult).getByRole("button", { name: "在文件夹中显示" }),
    );
    await waitFor(() => {
      expect(within(automationResult).queryByRole("alert")).not.toBeInTheDocument();
    });

    revealMock.mockRejectedValueOnce(new Error("manual Finder unavailable"));
    await user.click(
      within(manualResult).getByRole("button", { name: "在文件夹中显示" }),
    );
    expect(await within(manualResult).findByRole("alert")).toHaveTextContent(
      "无法在文件夹中显示",
    );
    expect(within(automationResult).queryByRole("alert")).not.toBeInTheDocument();
  });

  it("clears automation success when manual assignment changes batch content", async () => {
    const user = userEvent.setup();
    const candidate = candidateFixture(itemFixture());
    mockDetail(detailFixture(), [candidate]);
    mockCommand(
      "run_batch_automation",
      automationFixture({
        importedCount: 4,
        assignedCount: 3,
        export: {
          directory: "/Users/finance/stale-automation-export",
          itemCount: 3,
          totalAmountCents: 12_850,
        },
      }),
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));
    expect(await screen.findByText("4 张新导入")).toBeInTheDocument();
    expect(screen.getByText("/Users/finance/stale-automation-export")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "加入推荐票据" }));

    expect(
      await screen.findByRole("button", { name: "一键自动处理" }),
    ).toBeEnabled();
    expect(screen.queryByText("自动处理完成")).not.toBeInTheDocument();
    expect(screen.queryByText("4 张新导入")).not.toBeInTheDocument();
    expect(
      screen.queryByText("/Users/finance/stale-automation-export"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "在文件夹中显示" }),
    ).not.toBeInTheDocument();
  });

  it("keeps a populated batch in failure state until automation is retried", async () => {
    const user = userEvent.setup();
    let attempt = 0;
    mockDetail(detailFixture([{ ...itemFixture(), batchId: "batch-summer" }]));
    mockCommand("run_batch_automation", () => {
      attempt += 1;
      return attempt === 1
        ? Promise.reject({
            code: "external",
            service: "mailbox",
            retryable: true,
            message: "邮箱同步暂时失败",
          })
        : automationFixture();
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "邮箱同步暂时失败",
    );
    expect(
      screen.getByRole("heading", { name: "6-8 月整理批次" }),
    ).toBeInTheDocument();
    expect(screen.getByText("高铁电子发票.pdf")).toBeInTheDocument();
    expect(screen.queryByText("自动处理完成")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "重试自动处理" }));

    expect(await screen.findByText("自动处理完成")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "在文件夹中显示" })).toBeInTheDocument();
    expect(commandCalls("run_batch_automation")).toHaveLength(2);
  });

  it("uses the fallback for a malformed automation error", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture());
    mockCommand("run_batch_automation", () =>
      Promise.reject({
        code: "external",
        message: "泄漏\n\u0000详情",
      }),
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("自动处理失败，请稍后重试");
    expect(alert).not.toHaveTextContent("泄漏");
  });

  it("sanitizes and caps a valid automation error message", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture());
    mockCommand("run_batch_automation", () =>
      Promise.reject({
        code: "external",
        service: "mailbox",
        retryable: true,
        message: `邮箱\u0000同步\n\u0085失败 ${"详".repeat(300)} 机密尾部`,
      }),
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));

    const message = (await screen.findByRole("alert")).querySelector("p");
    expect(message).not.toBeNull();
    expect(message?.textContent).toHaveLength(240);
    expect(message).toHaveTextContent(`邮箱 同步 失败 ${"详".repeat(231)}`);
    expect(message).not.toHaveTextContent("机密尾部");
    expect(
      Array.from(message?.textContent ?? "").some((character) => {
        const codePoint = character.codePointAt(0) ?? 0;
        return codePoint <= 0x1f || (codePoint >= 0x7f && codePoint <= 0x9f);
      }),
    ).toBe(false);
  });

  it("does not recreate a batch when its automatic start fails", async () => {
    const user = userEvent.setup();
    const created = batchFixture({ id: "batch-auto-failed" });
    let automationAttempt = 0;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("create_month_batch", created);
    mockCommand("get_batch", { ...detailFixture(), batch: created });
    mockCommand("run_batch_automation", () => {
      automationAttempt += 1;
      return automationAttempt === 1
        ? Promise.reject({ code: "conflict", message: "自动处理已在运行" })
        : automationFixture();
    });

    renderAppAt("/batches/new");
    await user.click(screen.getByRole("button", { name: "创建并自动处理" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("自动处理已在运行");
    expect(
      screen.getByRole("heading", { name: "6-8 月整理批次" }),
    ).toBeInTheDocument();
    expect(commandCalls("create_month_batch")).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "重试自动处理" }));
    expect(await screen.findByText("自动处理完成")).toBeInTheDocument();
    expect(commandCalls("create_month_batch")).toHaveLength(1);
    expect(commandCalls("run_batch_automation")).toHaveLength(2);
  });

  it("consumes automatic route state once under StrictMode", async () => {
    const user = userEvent.setup();
    const automation = deferred<BatchAutomationResultDto>();
    const created = batchFixture({ id: "batch-strict" });
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("create_month_batch", created);
    mockCommand("get_batch", { ...detailFixture(), batch: created });
    mockCommand("run_batch_automation", automation.promise);

    const view = renderStrictAppAt("/batches/new");
    await user.click(screen.getByRole("button", { name: "创建并自动处理" }));
    await waitFor(() => expect(commandCalls("run_batch_automation")).toHaveLength(1));
    expect(window.history.state.usr).toBeNull();

    view.rerender(
      <StrictMode>
        <App />
      </StrictMode>,
    );
    expect(commandCalls("run_batch_automation")).toHaveLength(1);
    await act(async () => automation.resolve(automationFixture()));
    expect(await screen.findByText("自动处理完成")).toBeInTheDocument();
  });

  it("ignores a late automation result after switching batch routes", async () => {
    const user = userEvent.setup();
    const first = deferred<BatchAutomationResultDto>();
    const second = deferred<BatchAutomationResultDto>();
    const otherDetail = {
      ...detailFixture(),
      batch: batchFixture({ id: "batch-other", name: "其他批次" }),
    };
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_batch", (arguments_) =>
      (arguments_ as { batchId: string }).batchId === "batch-other"
        ? otherDetail
        : detailFixture(),
    );
    mockCommand("run_batch_automation", (arguments_) =>
      (arguments_ as { batchId: string }).batchId === "batch-other"
        ? second.promise
        : first.promise,
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));
    await act(async () => {
      window.history.pushState({}, "", "/batches/batch-other");
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    expect(await screen.findByRole("heading", { name: "其他批次" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "一键自动处理" }));

    await act(async () => first.resolve(automationFixture({ importedCount: 77 })));
    expect(
      screen.getByRole("status", { name: "正在自动处理批次" }),
    ).toBeInTheDocument();
    expect(screen.queryByText("77 张新导入")).not.toBeInTheDocument();
    await act(async () => second.resolve(automationFixture({ importedCount: 2 })));
    expect(await screen.findByText("2 张新导入")).toBeInTheDocument();
  });

  it("ignores a late automation result after the detail page unmounts", async () => {
    const user = userEvent.setup();
    const automation = deferred<BatchAutomationResultDto>();
    const other = batchFixture({ id: "batch-other", name: "其他批次" });
    mockDetail(detailFixture());
    mockCommand("run_batch_automation", automation.promise);
    mockCommand("list_batches", { items: [other], nextCursor: null });
    mockCommand("get_batch", (arguments_) =>
      (arguments_ as { batchId: string }).batchId === "batch-other"
        ? { ...detailFixture(), batch: other }
        : detailFixture(),
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "一键自动处理" }));
    await user.click(screen.getByText("报销批次", { selector: ".batch-back-link" }));
    await user.click(await screen.findByRole("link", { name: /其他批次/ }));
    await act(async () => automation.resolve(automationFixture({ importedCount: 88 })));

    expect(screen.getByRole("heading", { name: "其他批次" })).toBeInTheDocument();
    expect(screen.queryByText("88 张新导入")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "一键自动处理" })).toBeEnabled();
  });

  it("continues recommendations after a disabled-only candidate page", async () => {
    const user = userEvent.setup();
    const nextCursor = {
      sortValue: "2026-07-15T09:45:00+08:00",
      id: "invoice-disabled",
    };
    const disabled = candidateFixture(
      itemFixture({ id: "invoice-disabled", originalName: "疑似重复发票.pdf" }),
      { eligible: false, disabledReason: "suspected_duplicate" },
    );
    const eligible = candidateFixture(
      itemFixture({ id: "invoice-page-two", originalName: "第二页车票.pdf" }),
    );
    mockDetail(detailFixture());
    mockCommand("list_batch_candidates", (arguments_) => {
      const page = (arguments_ as { page: { cursor?: typeof nextCursor } }).page;
      return page.cursor
        ? { items: [eligible], nextCursor: null }
        : { items: [disabled], nextCursor };
    });
    mockCommand(
      "assign_items_to_batch",
      detailFixture([{ ...eligible.item, batchId: "batch-summer" }]),
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "加入推荐票据" }));

    await waitFor(() => {
      expect(commandCalls("assign_items_to_batch")[0]).toEqual({
        batchId: "batch-summer",
        itemIds: ["invoice-page-two"],
      });
    });
    expect(commandCalls("list_batch_candidates")).toEqual([
      { batchId: "batch-summer", page: { pageSize: 200 } },
      { batchId: "batch-summer", page: { cursor: nextCursor, pageSize: 200 } },
    ]);
  });

  it("refuses automatic recommendations when more than five full pages exist", async () => {
    const user = userEvent.setup();
    let pageNumber = 0;
    mockDetail(detailFixture());
    mockCommand("list_batch_candidates", () => {
      pageNumber += 1;
      return {
        items: [candidateFixture(itemFixture({ id: `invoice-${pageNumber}` }))],
        nextCursor: {
          sortValue: `2026-07-${String(20 - pageNumber).padStart(2, "0")}T09:00:00+08:00`,
          id: `invoice-${pageNumber}`,
        },
      };
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "加入推荐票据" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "候选票据超过 1000 张，请使用“调整票据”分批归属",
    );
    expect(commandCalls("list_batch_candidates")).toHaveLength(5);
    expect(commandCalls("assign_items_to_batch")).toHaveLength(0);
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
    await user.click(
      screen.getByRole("checkbox", { name: "创建后自动处理并导出" }),
    );
    await user.click(screen.getByRole("button", { name: "创建批次" }));

    await waitFor(() => {
      expect(commandCalls("create_month_batch")).toEqual([
        { year: now.getFullYear(), month: now.getMonth() + 1 },
      ]);
    });
  });

  it("keeps the current route when creation succeeds after cancel", async () => {
    const user = userEvent.setup();
    const creation = deferred<BatchDto>();
    const created = batchFixture({ id: "batch-late", name: "延迟创建批次" });
    let serverBatches: BatchDto[] = [];
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("create_month_batch", creation.promise);
    mockCommand("list_batches", () => ({
      items: serverBatches,
      nextCursor: null,
    }));

    renderAppAt("/batches/new");
    await user.click(
      screen.getByRole("checkbox", { name: "创建后自动处理并导出" }),
    );
    await user.click(screen.getByRole("button", { name: "创建批次" }));
    await user.click(screen.getByRole("button", { name: "取消" }));
    expect(await screen.findByRole("heading", { name: "报销批次" })).toBeInTheDocument();

    serverBatches = [created];
    await act(async () => creation.resolve(created));

    expect(window.location.pathname).toBe("/batches");
    expect(
      await screen.findByRole("link", { name: /延迟创建批次/ }),
    ).toBeInTheDocument();
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
      hasNormalizedPdf: true,
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

  it("describes every warning on an outside-range disabled candidate", async () => {
    const user = userEvent.setup();
    const candidate = candidateFixture(
      itemFixture({
        id: "invoice-outside-duplicate",
        originalName: "范围外重复发票.pdf",
      }),
      {
        outsideBatchRange: true,
        eligible: false,
        disabledReason: "suspected_duplicate",
      },
    );
    mockDetail(detailFixture(), [candidate]);

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const checkbox = await screen.findByRole("checkbox", { name: "范围外重复发票.pdf" });

    expect(checkbox).toBeDisabled();
    expect(checkbox).toHaveAccessibleDescription(
      "日期超出批次范围；疑似重复，不能归属",
    );
    expect(screen.getByText("日期超出批次范围；疑似重复，不能归属")).toBeInTheDocument();
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

  it("accepts a successful candidate assignment under StrictMode", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture(), [candidateFixture(itemFixture())]);

    renderStrictAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const dialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(dialog).findByRole("checkbox", { name: "高铁电子发票.pdf" }),
    );
    await user.click(within(dialog).getByRole("button", { name: "归属所选票据" }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog", { name: "调整票据归属" })).not.toBeInTheDocument();
    });
    expect(
      within(screen.getByRole("region", { name: "已归属票据" })).getByText(
        "高铁电子发票.pdf",
      ),
    ).toBeInTheDocument();
  });

  it("reconciles a committed assignment after the dialog closes", async () => {
    const user = userEvent.setup();
    const candidate = candidateFixture(itemFixture());
    const assignment = deferred<BatchDetailDto>();
    const committed = detailFixture([
      { ...candidate.item, batchId: "batch-summer" },
    ]);
    let serverDetail = detailFixture();
    mockDetail(serverDetail, [candidate]);
    mockCommand("get_batch", () => serverDetail);
    mockCommand("assign_items_to_batch", assignment.promise);
    mockCommand("list_batches", {
      items: [committed.batch],
      nextCursor: null,
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const dialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(dialog).findByRole("checkbox", { name: "高铁电子发票.pdf" }),
    );
    await user.click(within(dialog).getByRole("button", { name: "归属所选票据" }));
    await user.click(within(dialog).getByRole("button", { name: "关闭调整票据" }));
    await user.click(screen.getByText("报销批次", { selector: ".batch-back-link" }));
    expect(await screen.findByRole("heading", { name: "报销批次" })).toBeInTheDocument();

    serverDetail = committed;
    await act(async () => assignment.resolve(committed));
    await user.click(
      await screen.findByRole("link", { name: /6-8 月整理批次/ }),
    );

    expect(
      within(await screen.findByRole("region", { name: "已归属票据" })).getByText(
        "高铁电子发票.pdf",
      ),
    ).toBeInTheDocument();
    expect(commandCalls("get_batch")).toHaveLength(2);
  });

  it("refreshes reopened candidates after a pending assignment commits", async () => {
    const user = userEvent.setup();
    const candidate = candidateFixture(itemFixture());
    const assignment = deferred<BatchDetailDto>();
    let serverCandidates = [candidate];
    mockDetail(detailFixture(), serverCandidates);
    mockCommand("list_batch_candidates", () => ({
      items: serverCandidates,
      nextCursor: null,
    }));
    mockCommand("assign_items_to_batch", assignment.promise);

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const firstDialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(firstDialog).findByRole("checkbox", { name: "高铁电子发票.pdf" }),
    );
    await user.click(within(firstDialog).getByRole("button", { name: "归属所选票据" }));
    await user.click(within(firstDialog).getByRole("button", { name: "关闭调整票据" }));
    await user.click(screen.getByRole("button", { name: "调整票据" }));
    const secondDialog = screen.getByRole("dialog", { name: "调整票据归属" });
    expect(
      await within(secondDialog).findByText("高铁电子发票.pdf"),
    ).toBeInTheDocument();

    serverCandidates = [];
    await act(async () => {
      assignment.resolve(
        detailFixture([{ ...candidate.item, batchId: "batch-summer" }]),
      );
    });

    expect(
      await within(secondDialog).findByText("当前日期范围内没有未归属票据"),
    ).toBeInTheDocument();
    expect(within(secondDialog).queryByText("高铁电子发票.pdf")).not.toBeInTheDocument();
    expect(commandCalls("list_batch_candidates")).toHaveLength(3);
  });

  it("uses authoritative detail when assignment responses finish in reverse", async () => {
    const user = userEvent.setup();
    const firstAssignment = deferred<BatchDetailDto>();
    const secondAssignment = deferred<BatchDetailDto>();
    const first = candidateFixture(itemFixture());
    const second = candidateFixture(
      itemFixture({
        id: "invoice-hotel",
        originalName: "酒店发票.pdf",
      }),
    );
    let serverDetail = detailFixture([]);
    let assignmentNumber = 0;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_batch", () => serverDetail);
    mockCommand("list_batch_candidates", {
      items: [first, second],
      nextCursor: null,
    });
    mockCommand("assign_items_to_batch", () => {
      assignmentNumber += 1;
      return assignmentNumber === 1
        ? firstAssignment.promise
        : secondAssignment.promise;
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const firstDialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(firstDialog).findByRole("checkbox", { name: "高铁电子发票.pdf" }),
    );
    await user.click(within(firstDialog).getByRole("button", { name: "归属所选票据" }));
    await user.click(within(firstDialog).getByRole("button", { name: "关闭调整票据" }));

    await user.click(screen.getByRole("button", { name: "调整票据" }));
    const secondDialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(secondDialog).findByRole("checkbox", { name: "酒店发票.pdf" }),
    );
    await user.click(within(secondDialog).getByRole("button", { name: "归属所选票据" }));

    const secondItem = { ...second.item, batchId: "batch-summer" };
    serverDetail = detailFixture([secondItem]);
    await act(async () => secondAssignment.resolve(serverDetail));
    expect(
      within(await screen.findByRole("region", { name: "已归属票据" })).getByText(
        "酒店发票.pdf",
      ),
    ).toBeInTheDocument();

    const firstItem = { ...first.item, batchId: "batch-summer" };
    serverDetail = detailFixture([firstItem, secondItem]);
    await act(async () => firstAssignment.resolve(detailFixture([firstItem])));

    const assignedItems = await screen.findByRole("region", { name: "已归属票据" });
    expect(within(assignedItems).getByText("高铁电子发票.pdf")).toBeInTheDocument();
    expect(within(assignedItems).getByText("酒店发票.pdf")).toBeInTheDocument();
    expect(commandCalls("get_batch")).toHaveLength(3);
  });

  it("reconciles a committed recommendation after route navigation", async () => {
    const user = userEvent.setup();
    const candidate = candidateFixture(itemFixture());
    const assignment = deferred<BatchDetailDto>();
    const committed = detailFixture([
      { ...candidate.item, batchId: "batch-summer" },
    ]);
    let serverDetail = detailFixture();
    mockDetail(serverDetail, [candidate]);
    mockCommand("get_batch", () => serverDetail);
    mockCommand("assign_items_to_batch", assignment.promise);
    mockCommand("list_batches", {
      items: [committed.batch],
      nextCursor: null,
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "加入推荐票据" }));
    await waitFor(() => expect(commandCalls("assign_items_to_batch")).toHaveLength(1));
    await user.click(screen.getByText("报销批次", { selector: ".batch-back-link" }));
    expect(await screen.findByRole("heading", { name: "报销批次" })).toBeInTheDocument();

    serverDetail = committed;
    await act(async () => assignment.resolve(committed));
    await user.click(
      await screen.findByRole("link", { name: /6-8 月整理批次/ }),
    );

    expect(
      within(await screen.findByRole("region", { name: "已归属票据" })).getByText(
        "高铁电子发票.pdf",
      ),
    ).toBeInTheDocument();
    expect(commandCalls("get_batch")).toHaveLength(2);
    expect(screen.queryByText("merged.pdf")).not.toBeInTheDocument();
  });

  it("shows assigned item status with Chinese copy", async () => {
    mockDetail(
      detailFixture([
        itemFixture({
          batchId: "batch-summer",
          status: "pending_confirmation",
          confirmationStatus: "pending",
        }),
      ]),
    );

    renderAppAt("/batches/batch-summer");
    const assignedItems = await screen.findByRole("region", { name: "已归属票据" });

    expect(within(assignedItems).getByRole("columnheader", { name: "状态" })).toBeInTheDocument();
    expect(within(assignedItems).getByText("待确认")).toBeInTheDocument();
  });

  it("distinguishes unknown and zero amounts in assigned rows", async () => {
    mockDetail(
      detailFixture([
        itemFixture({
          id: "invoice-unknown-amount",
          originalName: "金额未知发票.pdf",
          batchId: "batch-summer",
          amountCents: null,
        }),
        itemFixture({
          id: "invoice-zero-amount",
          originalName: "零金额发票.pdf",
          batchId: "batch-summer",
          amountCents: 0,
        }),
      ]),
    );

    renderAppAt("/batches/batch-summer");
    const assignedItems = await screen.findByRole("region", { name: "已归属票据" });
    const unknownRow = within(assignedItems).getByText("金额未知发票.pdf").closest("tr");
    const zeroRow = within(assignedItems).getByText("零金额发票.pdf").closest("tr");

    expect(within(unknownRow!).getByText("金额待补充")).toBeInTheDocument();
    expect(within(zeroRow!).getByText("¥0.00")).toBeInTheDocument();
  });

  it("distinguishes unknown and zero amounts in candidate rows", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture(), [
      candidateFixture(
        itemFixture({
          id: "candidate-unknown-amount",
          originalName: "候选金额未知.pdf",
          amountCents: null,
        }),
      ),
      candidateFixture(
        itemFixture({
          id: "candidate-zero-amount",
          originalName: "候选零金额.pdf",
          amountCents: 0,
        }),
      ),
    ]);

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const dialog = screen.getByRole("dialog", { name: "调整票据归属" });
    const unknownRow = (await within(dialog).findByText("候选金额未知.pdf")).closest("label");
    const zeroRow = within(dialog).getByText("候选零金额.pdf").closest("label");

    expect(within(unknownRow!).getByText("金额待补充")).toBeInTheDocument();
    expect(within(zeroRow!).getByText("¥0.00")).toBeInTheDocument();
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

  it("reconciles a committed removal after route navigation", async () => {
    const user = userEvent.setup();
    const removal = deferred<BatchDetailDto>();
    const committed = detailFixture([]);
    let serverDetail = detailFixture([
      { ...itemFixture(), batchId: "batch-summer" },
    ]);
    mockDetail(serverDetail);
    mockCommand("get_batch", () => serverDetail);
    mockCommand("remove_item_from_batch", removal.promise);
    mockCommand("list_batches", {
      items: [committed.batch],
      nextCursor: null,
    });

    renderAppAt("/batches/batch-summer");
    await user.click(
      await screen.findByRole("button", { name: "移出 高铁电子发票.pdf" }),
    );
    const confirmation = screen.getByRole("dialog", { name: "确认移出票据" });
    await user.click(within(confirmation).getByRole("button", { name: "确认移出" }));
    await waitFor(() => expect(commandCalls("remove_item_from_batch")).toHaveLength(1));
    await user.click(screen.getByText("报销批次", { selector: ".batch-back-link" }));
    expect(await screen.findByRole("heading", { name: "报销批次" })).toBeInTheDocument();

    serverDetail = committed;
    await act(async () => removal.resolve(committed));
    await user.click(
      await screen.findByRole("link", { name: /6-8 月整理批次/ }),
    );

    expect(await screen.findByText("尚未归属票据")).toBeInTheDocument();
    expect(screen.queryByText("高铁电子发票.pdf")).not.toBeInTheDocument();
    expect(commandCalls("get_batch")).toHaveLength(2);
  });

  it("keeps pending removal non-dismissible and focus-stable until success", async () => {
    const user = userEvent.setup();
    const removal = deferred<BatchDetailDto>();
    mockDetail(detailFixture([{ ...itemFixture(), batchId: "batch-summer" }]));
    mockCommand("remove_item_from_batch", removal.promise);

    renderAppAt("/batches/batch-summer");
    await user.click(
      await screen.findByRole("button", { name: "移出 高铁电子发票.pdf" }),
    );
    const confirmation = screen.getByRole("dialog", { name: "确认移出票据" });
    await user.click(within(confirmation).getByRole("button", { name: "确认移出" }));
    const waiting = within(confirmation).getByRole("button", { name: "正在移除" });

    expect(within(confirmation).getByRole("button", { name: "确认移出" })).toBeDisabled();
    expect(waiting).toBeEnabled();
    expect(waiting).toHaveAttribute("aria-disabled", "true");
    await waitFor(() => expect(waiting).toHaveFocus());
    await user.click(waiting);
    expect(screen.getByRole("dialog", { name: "确认移出票据" })).toBe(confirmation);
    await user.keyboard("{Escape}");
    expect(screen.getByRole("dialog", { name: "确认移出票据" })).toBe(confirmation);
    await user.tab();
    expect(waiting).toHaveFocus();
    await user.tab({ shift: true });
    expect(waiting).toHaveFocus();
    expect(commandCalls("remove_item_from_batch")).toHaveLength(1);

    await act(async () => removal.resolve(detailFixture([])));

    expect(screen.queryByRole("dialog", { name: "确认移出票据" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "调整票据" })).toHaveFocus();
  });

  it("focuses a stable fallback after successful removal", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture([{ ...itemFixture(), batchId: "batch-summer" }]));

    renderAppAt("/batches/batch-summer");
    const opener = await screen.findByRole("button", {
      name: "移出 高铁电子发票.pdf",
    });
    await user.click(opener);
    const confirmation = screen.getByRole("dialog", { name: "确认移出票据" });
    await user.click(within(confirmation).getByRole("button", { name: "确认移出" }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog", { name: "确认移出票据" })).not.toBeInTheDocument();
    });
    expect(opener).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "调整票据" })).toHaveFocus();
  });

  it("contains removal confirmation focus and restores it after Escape", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture([{ ...itemFixture(), batchId: "batch-summer" }]));

    renderAppAt("/batches/batch-summer");
    const opener = await screen.findByRole("button", { name: "移出 高铁电子发票.pdf" });
    await user.click(opener);
    const confirmation = screen.getByRole("dialog", { name: "确认移出票据" });
    const cancel = within(confirmation).getByRole("button", { name: "取消" });
    const confirm = within(confirmation).getByRole("button", { name: "确认移出" });

    expect(cancel).toHaveFocus();
    await user.tab({ shift: true });
    expect(confirm).toHaveFocus();
    await user.tab();
    expect(cancel).toHaveFocus();
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "确认移出票据" })).not.toBeInTheDocument();
    await waitFor(() => expect(opener).toHaveFocus());
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

  it("clears export error feedback when recommendations change content", async () => {
    const user = userEvent.setup();
    const candidate = candidateFixture(itemFixture());
    mockDetail(detailFixture(), [candidate]);
    mockCommand("export_batch", () =>
      Promise.reject({
        code: "external",
        service: "filesystem",
        retryable: true,
        message: "旧报销包导出失败",
      }),
    );

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("旧报销包导出失败");
    expect(screen.getByRole("button", { name: "重试导出" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "加入推荐票据" }));
    expect(
      within(await screen.findByRole("region", { name: "已归属票据" })).getByText(
        "高铁电子发票.pdf",
      ),
    ).toBeInTheDocument();

    expect(screen.queryByText("旧报销包导出失败")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "重试导出" })).not.toBeInTheDocument();
  });

  it("ignores an in-flight export after recommendations change content", async () => {
    const user = userEvent.setup();
    const candidate = candidateFixture(itemFixture());
    const exportRequest = deferred<{
      directory: string;
      itemCount: number;
      totalAmountCents: number;
    }>();
    mockDetail(detailFixture(), [candidate]);
    mockCommand("export_batch", exportRequest.promise);

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    expect(screen.getByRole("button", { name: "正在生成报销文件" })).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "加入推荐票据" }));
    expect(
      within(await screen.findByRole("region", { name: "已归属票据" })).getByText(
        "高铁电子发票.pdf",
      ),
    ).toBeInTheDocument();

    await act(async () => {
      exportRequest.resolve({
        directory: "/Users/finance/stale-export",
        itemCount: 0,
        totalAmountCents: 0,
      });
    });

    expect(screen.getByRole("button", { name: "导出报销包" })).toBeEnabled();
    expect(screen.queryByText("merged.pdf")).not.toBeInTheDocument();
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

  it("clears reveal feedback when removal changes content", async () => {
    const user = userEvent.setup();
    mockDetail(detailFixture([{ ...itemFixture(), batchId: "batch-summer" }]));
    mockCommand("export_batch", {
      directory: "/Users/finance/old-export",
      itemCount: 1,
      totalAmountCents: 12_850,
    });
    revealMock.mockRejectedValue(new Error("Finder unavailable"));

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    await user.click(await screen.findByRole("button", { name: "在文件夹中显示" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("无法在文件夹中显示");

    await user.click(screen.getByRole("button", { name: "移出 高铁电子发票.pdf" }));
    const confirmation = screen.getByRole("dialog", { name: "确认移出票据" });
    await user.click(within(confirmation).getByRole("button", { name: "确认移出" }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog", { name: "确认移出票据" })).not.toBeInTheDocument();
    });
    expect(screen.queryByText("无法在文件夹中显示")).not.toBeInTheDocument();
    expect(screen.queryByText("merged.pdf")).not.toBeInTheDocument();
  });

  it("clears export success when a closed-dialog assignment commits", async () => {
    const user = userEvent.setup();
    const assignment = deferred<BatchDetailDto>();
    const candidate = candidateFixture(itemFixture());
    let serverDetail = detailFixture([]);
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_batch", () => serverDetail);
    mockCommand("list_batch_candidates", {
      items: [candidate],
      nextCursor: null,
    });
    mockCommand("assign_items_to_batch", assignment.promise);
    mockCommand("export_batch", {
      directory: "/Users/finance/old-export",
      itemCount: 0,
      totalAmountCents: 0,
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    expect(await screen.findByText("merged.pdf")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "调整票据" }));
    const dialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(dialog).findByRole("checkbox", { name: "高铁电子发票.pdf" }),
    );
    await user.click(within(dialog).getByRole("button", { name: "归属所选票据" }));
    await user.click(within(dialog).getByRole("button", { name: "关闭调整票据" }));

    serverDetail = detailFixture([
      { ...candidate.item, batchId: "batch-summer" },
    ]);
    await act(async () => assignment.resolve(serverDetail));

    await waitFor(() => expect(screen.queryByText("merged.pdf")).not.toBeInTheDocument());
    expect(screen.getByText("高铁电子发票.pdf")).toBeInTheDocument();
  });

  it("clears a remounted batch export when an old assignment commits", async () => {
    const user = userEvent.setup();
    const assignment = deferred<BatchDetailDto>();
    const candidate = candidateFixture(itemFixture());
    let serverDetail = detailFixture([]);
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_batch", () => serverDetail);
    mockCommand("list_batch_candidates", {
      items: [candidate],
      nextCursor: null,
    });
    mockCommand("list_batches", () => ({
      items: [serverDetail.batch],
      nextCursor: null,
    }));
    mockCommand("assign_items_to_batch", assignment.promise);
    mockCommand("export_batch", {
      directory: "/Users/finance/current-export",
      itemCount: 0,
      totalAmountCents: 0,
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const dialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(dialog).findByRole("checkbox", { name: "高铁电子发票.pdf" }),
    );
    await user.click(within(dialog).getByRole("button", { name: "归属所选票据" }));
    await user.click(within(dialog).getByRole("button", { name: "关闭调整票据" }));

    await user.click(screen.getByText("报销批次", { selector: ".batch-back-link" }));
    await user.click(await screen.findByRole("link", { name: /6-8 月整理批次/ }));
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    expect(await screen.findByText("merged.pdf")).toBeInTheDocument();

    serverDetail = detailFixture([
      { ...candidate.item, batchId: "batch-summer" },
    ]);
    await act(async () => assignment.resolve(serverDetail));

    expect(await screen.findByText("高铁电子发票.pdf")).toBeInTheDocument();
    expect(screen.queryByText("merged.pdf")).not.toBeInTheDocument();
  });

  it("keeps current-route export feedback when an old assignment commits", async () => {
    const user = userEvent.setup();
    const assignment = deferred<BatchDetailDto>();
    const candidate = candidateFixture(itemFixture());
    const otherDetail = {
      ...detailFixture(),
      batch: batchFixture({ id: "batch-other", name: "其他批次" }),
    };
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_batch", (arguments_) =>
      (arguments_ as { batchId: string }).batchId === "batch-other"
        ? otherDetail
        : detailFixture(),
    );
    mockCommand("list_batch_candidates", {
      items: [candidate],
      nextCursor: null,
    });
    mockCommand("assign_items_to_batch", assignment.promise);
    mockCommand("export_batch", {
      directory: "/Users/finance/current-export",
      itemCount: 0,
      totalAmountCents: 0,
    });

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "调整票据" }));
    const dialog = screen.getByRole("dialog", { name: "调整票据归属" });
    await user.click(
      await within(dialog).findByRole("checkbox", { name: "高铁电子发票.pdf" }),
    );
    await user.click(within(dialog).getByRole("button", { name: "归属所选票据" }));
    await waitFor(() => expect(commandCalls("assign_items_to_batch")).toHaveLength(1));

    await act(async () => {
      window.history.pushState({}, "", "/batches/batch-other");
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    expect(await screen.findByRole("heading", { name: "其他批次" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "导出报销包" }));
    expect(await screen.findByText("merged.pdf")).toBeInTheDocument();

    await act(async () => {
      assignment.resolve(
        detailFixture([{ ...candidate.item, batchId: "batch-summer" }]),
      );
    });

    expect(screen.getByText("merged.pdf")).toBeInTheDocument();
  });

  it("invalidates a committed export after route navigation", async () => {
    const user = userEvent.setup();
    const exportRequest = deferred<{
      directory: string;
      itemCount: number;
      totalAmountCents: number;
    }>();
    let serverDetail = detailFixture();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_batch", () => serverDetail);
    mockCommand("list_batches", () => ({
      items: [serverDetail.batch],
      nextCursor: null,
    }));
    mockCommand("export_batch", exportRequest.promise);

    renderAppAt("/batches/batch-summer");
    await user.click(await screen.findByRole("button", { name: "导出报销包" }));
    await user.click(screen.getByText("报销批次", { selector: ".batch-back-link" }));
    expect(await screen.findByRole("heading", { name: "报销批次" })).toBeInTheDocument();

    serverDetail = {
      ...serverDetail,
      batch: batchFixture({
        status: "exported",
        lastExportedAt: "2026-07-17T10:00:00+08:00",
      }),
    };
    await act(async () => {
      exportRequest.resolve({
        directory: "/Users/finance/stale-export",
        itemCount: 0,
        totalAmountCents: 0,
      });
    });
    await user.click(
      await screen.findByRole("link", { name: /6-8 月整理批次/ }),
    );

    expect(await screen.findByText("已导出")).toBeInTheDocument();
    expect(commandCalls("get_batch")).toHaveLength(2);
    expect(screen.queryByText("merged.pdf")).not.toBeInTheDocument();
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
