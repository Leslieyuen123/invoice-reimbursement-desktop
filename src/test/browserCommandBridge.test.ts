import { beforeEach, describe, expect, it } from "vitest";

import * as apiModule from "../lib/api";

import type {
  BatchCandidateDto,
  BatchDetailDto,
  BatchDto,
  ExportResultDto,
  InvoiceItemDto,
  ItemStatus,
  ManualImportOutcomeDto,
  PageDto,
  StorageStatusDto,
} from "../types";
import {
  browserCommandNames,
  createBrowserCommandBridge,
  installBrowserCommandBridge,
} from "./browserCommandBridge";

function invoiceFixture(
  id: string,
  overrides: Partial<InvoiceItemDto> = {},
): InvoiceItemDto {
  return {
    id,
    originalName: `${id}.png`,
    previewUrl: "/src-tauri/tests/fixtures/image-invoice.png",
    sourceType: "manual_upload",
    sourceAccountId: null,
    fetchedAt: "2026-07-17T08:00:00Z",
    invoiceDate: "2026-07-15",
    suggestedPeriod: "2026-07",
    batchId: null,
    suggestedCategory: "dining",
    finalCategory: "dining",
    amountCents: 12_850,
    currency: "CNY",
    city: "上海",
    company: "测试餐厅",
    status: "ready",
    recognitionStatus: "succeeded",
    confirmationStatus: "confirmed",
    dedupeStatus: "unique",
    note: null,
    eventTag: null,
    projectTag: null,
    createdAt: "2026-07-17T08:00:00Z",
    updatedAt: "2026-07-17T08:00:00Z",
    ...overrides,
  };
}

function batchFixture(id = "batch-1", overrides: Partial<BatchDto> = {}): BatchDto {
  return {
    id,
    name: "2026 年 7 月报销",
    startDate: "2026-07-01",
    endDate: "2026-07-31",
    status: "draft",
    itemCount: 0,
    totalAmountCents: 0,
    unconfirmedCount: 0,
    note: null,
    createdAt: "2026-07-17T08:00:00Z",
    updatedAt: "2026-07-17T08:00:00Z",
    lastExportedAt: null,
    ...overrides,
  };
}

function createSeededBridge(seed: {
  items?: InvoiceItemDto[];
  batches?: BatchDto[];
}) {
  return createBrowserCommandBridge({ seed } as never);
}

describe("browser command bridge", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    window.history.replaceState({}, "", "/");
    Reflect.deleteProperty(window, "__INVOICE_COMMAND_BRIDGE__");
  });

  it("has an exact handler for every frontend wire command", () => {
    const apiCommandNames = (
      apiModule as typeof apiModule & { apiCommandNames?: () => string[] }
    ).apiCommandNames;
    expect(apiCommandNames).toBeTypeOf("function");
    expect(browserCommandNames()).toEqual(apiCommandNames?.());
  });

  it("keeps the manual review, batch assignment, and export flow stateful", async () => {
    const bridge = createBrowserCommandBridge();

    const emptyItems = await bridge<PageDto<InvoiceItemDto>>("list_items", {
      filter: {},
    });
    expect(emptyItems).toEqual({ items: [], nextCursor: null });

    const imports = await bridge<ManualImportOutcomeDto[]>(
      "import_manual_files",
      { paths: ["text-invoice.pdf"] },
    );
    expect(imports).toHaveLength(1);
    expect(imports[0]).toMatchObject({
      status: "imported",
      path: "text-invoice.pdf",
      item: {
        originalName: "text-invoice.pdf",
        previewUrl: "/src-tauri/tests/fixtures/image-invoice.png",
        status: "pending_confirmation",
      },
    });
    if (imports[0].status !== "imported") throw new Error("fixture import failed");

    const reviewed = await bridge<InvoiceItemDto>("review_item", {
      input: {
        id: imports[0].item.id,
        invoiceDate: "2026-07-15",
        suggestedPeriod: "2026-07",
        finalCategory: "dining",
        amountCents: 12_850,
        city: "上海",
        company: "测试餐厅",
        note: null,
        eventTag: null,
        projectTag: null,
      },
    });
    expect(reviewed).toMatchObject({
      finalCategory: "dining",
      status: "ready",
      confirmationStatus: "confirmed",
    });

    const batch = await bridge<BatchDto>("create_month_batch", {
      year: 2026,
      month: 7,
    });
    const candidates = await bridge<
      PageDto<{ item: InvoiceItemDto; outsideBatchRange: boolean; eligible: boolean }>
    >("list_batch_candidates", { batchId: batch.id });
    expect(candidates.items).toEqual([
      expect.objectContaining({
        item: expect.objectContaining({ id: reviewed.id }),
        outsideBatchRange: false,
        eligible: true,
      }),
    ]);

    const assigned = await bridge<BatchDetailDto>("assign_items_to_batch", {
      batchId: batch.id,
      itemIds: [reviewed.id],
    });
    expect(assigned.summary).toMatchObject({
      itemCount: 1,
      totalAmountCents: 12_850,
      unconfirmedCount: 0,
      dining: { itemCount: 1, amountCents: 12_850 },
    });

    const exported = await bridge<ExportResultDto>("export_batch", {
      batchId: batch.id,
    });
    expect(exported).toEqual({
      directory: "/tmp/invoice-reimbursement/e2e/2026-07",
      itemCount: 1,
      totalAmountCents: 12_850,
    });
    await expect(
      bridge<BatchDetailDto>("get_batch", { batchId: batch.id }),
    ).resolves.toMatchObject({ batch: { status: "exported" } });
  });

  it("implements every frontend command and rejects unknown commands clearly", async () => {
    const bridge = createBrowserCommandBridge();

    await expect(bridge("get_dashboard")).resolves.toMatchObject({
      pendingConfirmationCount: 0,
      recentBatches: [],
    });
    await expect(bridge("list_mailbox_accounts")).resolves.toEqual([]);
    await expect(bridge("get_preferences")).resolves.toMatchObject({
      backgroundSyncEnabled: true,
    });
    await expect(bridge<StorageStatusDto>("get_storage_status")).resolves.toMatchObject({
      recoveryError: null,
    });
    await expect(bridge("retry_export_recovery")).resolves.toMatchObject({
      recoveryError: null,
    });
    await expect(bridge("sync_account_now", { accountId: "missing" })).rejects.toMatchObject({
      code: "not_found",
    });
    await expect(bridge("not_a_real_command")).rejects.toThrow(
      "Unsupported browser command: not_a_real_command",
    );
  });

  it("opens only originals that belong to a known item", async () => {
    const bridge = createSeededBridge({
      items: [invoiceFixture("known")],
    });

    await expect(
      bridge("open_item_original", { itemId: "known" }),
    ).resolves.toBeUndefined();
    await expect(
      bridge("open_item_original", { itemId: "missing" }),
    ).rejects.toMatchObject({ code: "not_found", entity: "item" });
  });

  it("injects deterministic delay and command failures for browser state audits", async () => {
    const bridge = createBrowserCommandBridge({
      delayMs: 20,
      failCommands: new Set(["list_items"]),
    });
    const startedAt = performance.now();

    await expect(
      bridge("list_items", { filter: {} }),
    ).rejects.toMatchObject({
      code: "external",
      service: "browser_bridge",
      retryable: true,
      message: "Simulated browser command failure: list_items",
    });
    expect(performance.now() - startedAt).toBeGreaterThanOrEqual(15);
    await expect(bridge("list_batches")).resolves.toEqual({
      items: [],
      nextCursor: null,
    });
  });

  it("paginates items, batches, and candidates with stable next cursors", async () => {
    const bridge = createBrowserCommandBridge();
    await bridge("import_manual_files", {
      paths: ["one.png", "two.png", "three.png", "four.png", "five.png"],
    });
    for (const month of [5, 6, 7]) {
      await bridge("create_month_batch", { year: 2026, month });
    }

    const itemPageOne = await bridge<PageDto<InvoiceItemDto>>("list_items", {
      filter: {},
      page: { pageSize: 2 },
    });
    const itemPageTwo = await bridge<PageDto<InvoiceItemDto>>("list_items", {
      filter: {},
      page: { pageSize: 2, cursor: itemPageOne.nextCursor },
    });
    expect(itemPageOne.items).toHaveLength(2);
    expect(itemPageOne.nextCursor).not.toBeNull();
    expect(itemPageTwo.items).toHaveLength(2);
    expect(itemPageTwo.items.map(({ id }) => id)).not.toEqual(
      itemPageOne.items.map(({ id }) => id),
    );

    const batchPageOne = await bridge<PageDto<BatchDto>>("list_batches", {
      page: { pageSize: 2 },
    });
    const batchPageTwo = await bridge<PageDto<BatchDto>>("list_batches", {
      page: { pageSize: 2, cursor: batchPageOne.nextCursor },
    });
    expect(batchPageOne.items).toHaveLength(2);
    expect(batchPageOne.nextCursor).not.toBeNull();
    expect(batchPageTwo.items).toHaveLength(1);

    const candidatePageOne = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      { batchId: batchPageOne.items[0].id, page: { pageSize: 2 } },
    );
    const candidatePageTwo = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      {
        batchId: batchPageOne.items[0].id,
        page: { pageSize: 2, cursor: candidatePageOne.nextCursor },
      },
    );
    expect(candidatePageOne.items).toHaveLength(2);
    expect(candidatePageOne.nextCursor).not.toBeNull();
    expect(candidatePageTwo.items).toHaveLength(2);
  });

  it("keeps dashboard recent counts aligned with recent item filtering", async () => {
    const bridge = createSeededBridge({
      items: [
        invoiceFixture("seven-day-boundary", {
          createdAt: "2026-07-10T08:00:00Z",
        }),
        invoiceFixture("eight-days-old", {
          createdAt: "2026-07-09T08:00:00Z",
        }),
      ],
    });

    await expect(bridge("get_dashboard")).resolves.toMatchObject({
      recentlyAddedCount: 1,
    });
    const recent = await bridge<PageDto<InvoiceItemDto>>("list_items", {
      filter: { recent: true },
    });
    expect(recent.items.map((item) => item.id)).toEqual(["seven-day-boundary"]);
  });

  it("matches Rust candidate range and searched outside-date derivation", async () => {
    const bridge = createSeededBridge({
      batches: [batchFixture()],
      items: [
        invoiceFixture("missing-date", {
          originalName: "候选-无日期.png",
          invoiceDate: null,
          status: "pending_confirmation",
          confirmationStatus: "pending",
        }),
        invoiceFixture("outside-date", {
          originalName: "候选-范围外.png",
          invoiceDate: "2026-06-30",
        }),
        invoiceFixture("failed", {
          originalName: "候选-识别失败.png",
          status: "ready",
          recognitionStatus: "failed",
        }),
        invoiceFixture("duplicate", {
          originalName: "候选-疑似重复.png",
          status: "ready",
          recognitionStatus: "failed",
          dedupeStatus: "suspected_duplicate",
        }),
        invoiceFixture("inside", {
          originalName: "候选-范围内.png",
        }),
      ],
    });

    const inRange = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      { batchId: "batch-1" },
    );
    expect(inRange.items.map(({ item }) => item.id).sort()).toEqual([
      "duplicate",
      "failed",
      "inside",
    ]);
    expect(inRange.items.every((candidate) => !candidate.outsideBatchRange)).toBe(
      true,
    );

    const whitespaceQuery = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      { batchId: "batch-1", query: " \t\n " },
    );
    expect(whitespaceQuery).toEqual(inRange);

    const searched = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      { batchId: "batch-1", query: "  候选  " },
    );
    const byId = Object.fromEntries(
      searched.items.map((candidate) => [candidate.item.id, candidate]),
    );
    expect(Object.keys(byId).sort()).toEqual([
      "duplicate",
      "failed",
      "inside",
      "missing-date",
      "outside-date",
    ]);
    expect(byId["missing-date"]).toMatchObject({
      outsideBatchRange: true,
      eligible: true,
      disabledReason: null,
    });
    expect(byId["outside-date"]).toMatchObject({
      outsideBatchRange: true,
      eligible: true,
      disabledReason: null,
    });
    expect(byId.failed).toMatchObject({
      outsideBatchRange: false,
      eligible: false,
      disabledReason: "recognition_failed",
    });
    expect(byId.duplicate).toMatchObject({
      outsideBatchRange: false,
      eligible: false,
      disabledReason: "suspected_duplicate",
    });
  });

  it("treats a NEL-only candidate query as no query", async () => {
    const bridge = createSeededBridge({
      batches: [batchFixture()],
      items: [
        invoiceFixture("inside"),
        invoiceFixture("outside", { invoiceDate: "2026-06-30" }),
      ],
    });

    const result = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      { batchId: "batch-1", query: "\u0085" },
    );

    expect(result.items.map(({ item }) => item.id)).toEqual(["inside"]);
  });

  it("preserves a BOM-only candidate query as searchable content", async () => {
    const bridge = createSeededBridge({
      batches: [batchFixture()],
      items: [invoiceFixture("inside")],
    });

    const result = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      { batchId: "batch-1", query: "\uFEFF" },
    );

    expect(result).toEqual({ items: [], nextCursor: null });
  });

  it("accepts a candidate query containing 200 astral code points", async () => {
    const bridge = createSeededBridge({
      batches: [batchFixture()],
      items: [invoiceFixture("inside")],
    });

    const result = await bridge<PageDto<BatchCandidateDto>>(
      "list_batch_candidates",
      { batchId: "batch-1", query: "😀".repeat(200) },
    );

    expect(result).toEqual({ items: [], nextCursor: null });
  });

  it.each([
    ["more than 200 Unicode code points", "😀".repeat(201)],
    ["a control character", "候选\u0000票据"],
  ])("rejects a candidate query containing %s", async (_case, query) => {
    const bridge = createSeededBridge({ batches: [batchFixture()] });

    await expect(
      bridge("list_batch_candidates", { batchId: "batch-1", query }),
    ).rejects.toEqual({
      code: "validation",
      field: "query",
      message: "query must contain at most 200 printable characters",
    });
  });

  it.each<ItemStatus>([
    "pending_recognition",
    "pending_confirmation",
    "recognition_failed",
    "suspected_duplicate",
  ])("refuses to export a batch containing a %s item", async (status) => {
    const bridge = createSeededBridge({
      batches: [batchFixture()],
      items: [
        invoiceFixture(`item-${status}`, {
          batchId: "batch-1",
          status,
          recognitionStatus:
            status === "pending_recognition"
              ? "pending"
              : status === "recognition_failed"
                ? "failed"
                : "succeeded",
          confirmationStatus: "pending",
          dedupeStatus:
            status === "suspected_duplicate" ? "suspected_duplicate" : "unique",
        }),
      ],
    });

    await expect(
      bridge("export_batch", { batchId: "batch-1" }),
    ).rejects.toMatchObject({ code: "conflict" });
  });

  it("refuses to export an empty batch", async () => {
    const bridge = createSeededBridge({ batches: [batchFixture()] });
    await expect(
      bridge("export_batch", { batchId: "batch-1" }),
    ).rejects.toMatchObject({ code: "conflict" });
  });

  it("returns exported batches to draft after review, assignment, or removal", async () => {
    const exported = batchFixture("batch-1", {
      status: "exported",
      lastExportedAt: "2026-07-17T07:00:00Z",
    });
    const reviewBridge = createSeededBridge({
      batches: [exported],
      items: [invoiceFixture("assigned", { batchId: "batch-1" })],
    });
    await reviewBridge("review_item", {
      input: {
        id: "assigned",
        invoiceDate: "2026-07-16",
        suggestedPeriod: "2026-07",
        finalCategory: "transport",
        amountCents: 15_000,
        city: null,
        company: null,
        note: null,
        eventTag: null,
        projectTag: null,
      },
    });
    await expect(
      reviewBridge<BatchDetailDto>("get_batch", { batchId: "batch-1" }),
    ).resolves.toMatchObject({
      batch: { status: "draft", lastExportedAt: "2026-07-17T07:00:00Z" },
    });

    const assignmentBridge = createSeededBridge({
      batches: [exported],
      items: [invoiceFixture("unassigned")],
    });
    await assignmentBridge("assign_items_to_batch", {
      batchId: "batch-1",
      itemIds: ["unassigned"],
    });
    await expect(
      assignmentBridge<BatchDetailDto>("get_batch", { batchId: "batch-1" }),
    ).resolves.toMatchObject({ batch: { status: "draft" } });

    const removalBridge = createSeededBridge({
      batches: [exported],
      items: [invoiceFixture("assigned", { batchId: "batch-1" })],
    });
    await removalBridge("remove_item_from_batch", {
      batchId: "batch-1",
      itemId: "assigned",
    });
    await expect(
      removalBridge<BatchDetailDto>("get_batch", { batchId: "batch-1" }),
    ).resolves.toMatchObject({ batch: { status: "draft" } });
  });

  it("rejects missing IDs without partially mutating bridge state", async () => {
    const bridge = createSeededBridge({
      batches: [batchFixture()],
      items: [invoiceFixture("known")],
    });
    await expect(
      bridge("assign_items_to_batch", {
        batchId: "batch-1",
        itemIds: ["known", "missing"],
      }),
    ).rejects.toMatchObject({ code: "not_found", entity: "item" });
    await expect(
      bridge<InvoiceItemDto>("get_item", { itemId: "known" }),
    ).resolves.toMatchObject({ batchId: null });
    await expect(
      bridge("delete_mailbox_account", { accountId: "missing" }),
    ).rejects.toMatchObject({ code: "not_found", entity: "account" });
    await expect(
      bridge("save_mailbox_account", {
        input: {
          id: "missing",
          provider: "gmail",
          email: "finance@example.com",
          secret: "",
          imapHost: null,
          imapPort: null,
          enabled: true,
          syncIntervalMinutes: 15,
        },
      }),
    ).rejects.toMatchObject({ code: "not_found", entity: "account" });
    await expect(
      bridge("test_mailbox_account", {
        input: {
          id: "missing",
          provider: "gmail",
          email: "finance@example.com",
          secret: "",
          imapHost: null,
          imapPort: null,
        },
      }),
    ).rejects.toMatchObject({ code: "not_found", entity: "account" });
  });

  it("persists bridge state across reinstall while retaining query failures and reset", async () => {
    window.history.replaceState({}, "", "/?bridgeReset=1");
    installBrowserCommandBridge();
    await window.__INVOICE_COMMAND_BRIDGE__?.("import_manual_files", {
      paths: ["durable.png"],
    });
    await window.__INVOICE_COMMAND_BRIDGE__?.("create_month_batch", {
      year: 2026,
      month: 7,
    });

    window.history.replaceState({}, "", "/?bridgeError=get_dashboard");
    installBrowserCommandBridge();
    await expect(
      window.__INVOICE_COMMAND_BRIDGE__?.<PageDto<InvoiceItemDto>>("list_items", {
        filter: {},
      }),
    ).resolves.toMatchObject({
      items: [expect.objectContaining({ originalName: "durable.png" })],
    });
    await expect(
      window.__INVOICE_COMMAND_BRIDGE__?.("get_dashboard"),
    ).rejects.toMatchObject({ code: "external" });

    window.history.replaceState({}, "", "/?bridgeReset=1");
    installBrowserCommandBridge();
    await expect(
      window.__INVOICE_COMMAND_BRIDGE__?.("list_items", { filter: {} }),
    ).resolves.toEqual({ items: [], nextCursor: null });
  });
});
