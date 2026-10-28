import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { StrictMode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ revealItemInDir: vi.fn() }));
vi.mock("@tauri-apps/api/path", () => ({
  join: vi.fn(async (...parts: string[]) => parts.join("/")),
}));

import { App } from "../../app/App";
import { mockCommand, resetMockApi } from "../../test/mockApi";
import type {
  InvoiceItemDto,
  MailLedgerEntryDto,
  MailLedgerFilter,
} from "../../types";

function entryFixture(overrides: Partial<MailLedgerEntryDto> = {}): MailLedgerEntryDto {
  return {
    accountId: "account-finance",
    mailbox: "INBOX",
    uid: 101,
    subject: "9 月发票",
    sender: "billing@example.com",
    receivedAt: "2026-09-10T06:35:00Z",
    processedAt: "2026-09-10T06:36:00Z",
    candidateCount: 2,
    importedCount: 2,
    existingCount: 0,
    failedCount: 0,
    outcome: "imported",
    reason: null,
    markedSeen: true,
    seenMismatch: false,
    ...overrides,
  };
}

function itemFixture(overrides: Partial<InvoiceItemDto> = {}): InvoiceItemDto {
  return {
    id: "invoice-1",
    originalName: "2631700000015392664_上海蓝叶草.pdf",
    previewUrl: "invoice-file://item/invoice-1?variant=normalized",
    sourceType: "email",
    sourceAccountId: "account-finance",
    fetchedAt: "2026-09-10T06:35:00Z",
    sourceReceivedDate: "2026-09-10",
    invoiceDate: "2026-09-09",
    suggestedPeriod: "2026-09",
    batchId: null,
    suggestedCategory: "dining",
    finalCategory: null,
    amountCents: 44_360,
    currency: "CNY",
    city: "上海",
    company: "上海顺庭餐饮有限公司",
    status: "pending_confirmation",
    recognitionStatus: "succeeded",
    confirmationStatus: "pending",
    dedupeStatus: "unique",
    hasNormalizedPdf: true,
    note: null,
    eventTag: null,
    projectTag: null,
    createdAt: "2026-09-10T06:35:00Z",
    updatedAt: "2026-09-10T06:35:00Z",
    ...overrides,
  };
}

function renderAppAt(path: string) {
  window.history.replaceState({}, "", path);
  return render(<App />);
}

beforeEach(() => {
  cleanup();
  resetMockApi();
});

describe("MailLedgerPage", () => {
  it("lists scanned mail with its result and asks for the attention filter first", async () => {
    const ledger: MailLedgerEntryDto[] = [
      entryFixture({
        uid: 201,
        subject: "缺票邮件",
        outcome: "failed",
        importedCount: 0,
        failedCount: 1,
        reason: "link_download_failed",
        markedSeen: false,
      }),
      entryFixture({ uid: 202, subject: "已处理的发票" }),
    ];
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_mail_ledger_counts", {
      imported: 7,
      partial: 0,
      failed: 1,
      ignored: 3,
      needsAttention: 1,
    });
    mockCommand("list_mail_ledger", (arguments_) => {
      const filter = (arguments_ as { filter: MailLedgerFilter }).filter;
      const items = filter.needsAttention
        ? ledger.filter((entry) => ["partial", "failed"].includes(entry.outcome))
        : ledger;
      return { items, nextCursor: null };
    });

    renderAppAt("/mail");

    expect(
      await screen.findByRole("heading", { name: "邮件台账" }),
    ).toBeInTheDocument();
    // 需关注 is the default tab, so the failed mail shows and the settled one does not.
    expect(await screen.findByText("缺票邮件")).toBeInTheDocument();
    expect(screen.queryByText("已处理的发票")).not.toBeInTheDocument();
    // "未提取" is both a filter tab and the row badge.
    expect(screen.getAllByText("未提取")).toHaveLength(2);
    expect(screen.getByText("link_download_failed")).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "需关注" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.getByText("1 封")).toBeInTheDocument();

    await userEvent.setup().click(screen.getByRole("tab", { name: "全部" }));

    expect(await screen.findByText("已处理的发票")).toBeInTheDocument();
    expect(screen.getAllByText("已提取").length).toBeGreaterThan(0);
  });

  it("shows the mail time to the day and expands the invoices it produced", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_mail_ledger_counts", {
      imported: 1,
      partial: 0,
      failed: 0,
      ignored: 0,
      needsAttention: 0,
    });
    mockCommand("list_mail_ledger", () => ({
      items: [entryFixture({ uid: 301, subject: "九月发票" })],
      nextCursor: null,
    }));
    mockCommand("list_items", (arguments_) => {
      const filter = (arguments_ as { filter: Record<string, unknown> }).filter;
      expect(filter).toMatchObject({
        sourceAccountId: "account-finance",
        sourceUid: 301,
      });
      return { items: [itemFixture()], nextCursor: null };
    });

    renderAppAt("/mail");

    await user.click(screen.getByRole("tab", { name: "全部" }));
    const subject = await screen.findByRole("button", { name: /九月发票/ });
    // Rendered in the machine's own time zone, so the expectation is derived
    // the same way instead of hard-coding one zone (CI runs in UTC).
    const received = new Date("2026-09-10T06:35:00Z");
    const pad = (value: number) => String(value).padStart(2, "0");
    const localTime =
      `${received.getFullYear()}-${pad(received.getMonth() + 1)}-` +
      `${pad(received.getDate())} ${pad(received.getHours())}:${pad(received.getMinutes())}`;
    expect(screen.getByText(localTime)).toBeInTheDocument();

    await user.click(subject);

    expect(
      await screen.findByText("2631700000015392664_上海蓝叶草.pdf"),
    ).toBeInTheDocument();
    expect(screen.getByText("¥443.60")).toBeInTheDocument();
  });

  it("flags mail whose invoices landed but the mailbox copy is still unread", async () => {
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_mail_ledger_counts", {
      imported: 1,
      partial: 0,
      failed: 0,
      ignored: 0,
      needsAttention: 0,
    });
    mockCommand("list_mail_ledger", () => ({
      items: [
        entryFixture({ uid: 401, subject: "标记失败邮件", markedSeen: false, seenMismatch: true }),
      ],
      nextCursor: null,
    }));

    renderAppAt("/mail");
    await userEvent.setup().click(screen.getByRole("tab", { name: "全部" }));

    expect(await screen.findByText("标记失败")).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.queryByText("未读")).not.toBeInTheDocument(),
    );
  });

  it("keeps the strict mode render path free of duplicate rows", async () => {
    mockCommand("get_dashboard", new Promise(() => undefined));
    mockCommand("get_mail_ledger_counts", {
      imported: 0,
      partial: 1,
      failed: 0,
      ignored: 0,
      needsAttention: 1,
    });
    mockCommand("list_mail_ledger", () => ({
      items: [entryFixture({ uid: 501, subject: "严格模式邮件", outcome: "partial" })],
      nextCursor: null,
    }));

    window.history.replaceState({}, "", "/mail");
    render(
      <StrictMode>
        <App />
      </StrictMode>,
    );

    expect(await screen.findAllByText("严格模式邮件")).toHaveLength(1);
  });
});
