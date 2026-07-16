import {
  act,
  cleanup,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

import { open } from "@tauri-apps/plugin-dialog";
import { App } from "../../app/App";
import {
  invokeMock,
  mockCommand,
  resetMockApi,
} from "../../test/mockApi";
import type { InvoiceItemDto } from "../../types";

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
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", { items: [], nextCursor: null });
    openDialogMock.mockResolvedValue([
      "/Users/finance/出租车电子发票.pdf",
      "/Users/finance/损坏附件.pdf",
    ]);
    let importAttempt = 0;
    mockCommand("import_manual_files", (arguments_) => {
      importAttempt += 1;
      const paths = (arguments_ as { paths: string[] }).paths;
      if (importAttempt === 1) {
        return [
          {
            status: "imported" as const,
            path: paths[0],
            item: pendingInvoiceFixture({ sourceType: "manual_upload" }),
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
      return [
        {
          status: "imported" as const,
          path: paths[0],
          item: pendingInvoiceFixture({
            id: "invoice-recovered",
            originalName: "损坏附件.pdf",
            sourceType: "manual_upload",
          }),
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

    await user.click(within(drawer).getByRole("button", { name: "重新加载" }));
    expect(await within(drawer).findByTitle("票据预览")).toBeInTheDocument();
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
    await user.click(screen.getByRole("button", { name: "出租车电子发票.pdf" }));

    await act(async () => {
      resolveSave?.(
        pendingInvoiceFixture({
          originalName: "旧保存响应.pdf",
          status: "ready",
          confirmationStatus: "confirmed",
        }),
      );
    });

    expect(screen.getByRole("button", { name: "旧保存响应.pdf" })).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "票据详情" })).toHaveTextContent(
      "出租车电子发票.pdf",
    );
  });

  it("keeps a reopened drawer isolated from an older recognition response", async () => {
    const user = userEvent.setup();
    let resolveRetry: ((item: InvoiceItemDto) => void) | undefined;
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [pendingInvoiceFixture()],
      nextCursor: null,
    });
    mockCommand(
      "retry_recognition",
      new Promise<InvoiceItemDto>((resolve) => {
        resolveRetry = resolve;
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
        }),
      );
    });

    expect(screen.getByRole("button", { name: "旧识别响应.pdf" })).toBeInTheDocument();
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
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", { items: [duplicate], nextCursor: null });
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
    await user.click(screen.getByRole("button", { name: "出租车电子发票.pdf" }));

    await act(async () => {
      resolveDeletion?.(null);
    });

    expect(
      screen.queryByRole("button", { name: "出租车电子发票.pdf" }),
    ).not.toBeInTheDocument();
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
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", {
      items: [duplicateOne, duplicateTwo],
      nextCursor: null,
    });
    mockCommand("resolve_duplicate", (arguments_) => {
      const request = arguments_ as { itemId: string; keep: boolean };
      return request.keep
        ? pendingInvoiceFixture({ status: "pending_confirmation", dedupeStatus: "resolved" })
        : null;
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
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("list_items", { items: [], nextCursor: null });
    openDialogMock.mockResolvedValue(["/Users/finance/重复票据.pdf"]);
    mockCommand("import_manual_files", [
      {
        status: "imported" as const,
        path: "/Users/finance/重复票据.pdf",
        item: pendingInvoiceFixture({
          id: "invoice-imported-duplicate",
          originalName: "重复票据.pdf",
          sourceType: "manual_upload",
          status: "suspected_duplicate",
          dedupeStatus: "suspected_duplicate",
        }),
      },
    ]);

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
