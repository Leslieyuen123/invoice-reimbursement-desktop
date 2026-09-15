import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { App } from "../../app/App";
import {
  invokeMock,
  mockCommand,
  resetMockApi,
} from "../../test/mockApi";
import type { DashboardDto } from "../../types";

function dashboardFixture(
  overrides: Partial<DashboardDto> = {},
): DashboardDto {
  return {
    mailboxAccounts: [
      {
        id: "enabled-account",
        provider: "gmail",
        email: "finance@example.com",
        imapHost: "imap.gmail.com",
        imapPort: 993,
        enabled: true,
        syncIntervalMinutes: 15,
        lastSyncedAt: "2026-07-16T08:30:00+08:00",
        lastError: null,
        lastErrorKind: null,
        lastErrorAt: null,
      },
      {
        id: "paused-account",
        provider: "qq",
        email: "archive@example.com",
        imapHost: "imap.qq.com",
        imapPort: 993,
        enabled: false,
        syncIntervalMinutes: 30,
        lastSyncedAt: null,
        lastError: null,
        lastErrorKind: null,
        lastErrorAt: null,
      },
    ],
    recentlyAddedCount: 7,
    pendingConfirmationCount: 4,
    recognitionFailedCount: 1,
    suspectedDuplicateCount: 2,
    mailNeedsAttentionCount: 0,
    recentBatches: [
      {
        id: "batch-july",
        name: "2026 年 7 月报销",
        startDate: "2026-07-01",
        endDate: "2026-07-31",
        status: "draft",
        itemCount: 5,
        totalAmountCents: 128_550,
        unconfirmedCount: 2,
        note: null,
        createdAt: "2026-07-15T09:00:00+08:00",
        updatedAt: "2026-07-16T08:00:00+08:00",
        lastExportedAt: null,
      },
    ],
    ...overrides,
  };
}

function renderAppAt(path = "/") {
  window.history.replaceState({}, "", path);
  return render(<App />);
}

describe("DashboardPage", () => {
  beforeEach(() => {
    cleanup();
    resetMockApi();
  });

  it("shows synchronization health and work queues", async () => {
    mockCommand("get_dashboard", dashboardFixture());

    renderAppAt();

    expect(await screen.findByText("待确认")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /待确认 4/ })).toBeInTheDocument();
    expect(screen.getByText("识别失败")).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: /识别失败 1/ }),
    ).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /疑似重复 2/ })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /最近新增 7/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "立即同步" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "新建批次" })).toBeEnabled();
  });

  it("navigates from a queue count to the matching inbox filter", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", dashboardFixture());

    renderAppAt();
    await user.click(await screen.findByRole("link", { name: /待确认 4/ }));

    expect(window.location.pathname).toBe("/inbox");
    expect(window.location.search).toBe("?status=pending_confirmation");
    expect(
      screen.getByRole("heading", { name: "待处理池" }),
    ).toBeInTheDocument();
  });

  it("requests the recent item filter after recent dashboard navigation", async () => {
    const user = userEvent.setup();
    mockCommand("get_dashboard", dashboardFixture());
    mockCommand("list_items", { items: [], nextCursor: null });

    renderAppAt();
    await user.click(await screen.findByRole("link", { name: /最近新增 7/ }));

    expect(window.location.pathname).toBe("/inbox");
    expect(window.location.search).toBe("?scope=recent");
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("list_items", {
        filter: { recent: true },
        page: { cursor: undefined, pageSize: 50 },
      });
    });
  });

  it("synchronizes enabled accounts once and refreshes dashboard data", async () => {
    const user = userEvent.setup();
    let finishSynchronization: (() => void) | undefined;
    mockCommand("get_dashboard", dashboardFixture());
    mockCommand(
      "sync_account_now",
      new Promise<void>((resolve) => {
        finishSynchronization = resolve;
      }),
    );

    renderAppAt();
    const syncButton = await screen.findByRole("button", { name: "立即同步" });
    await user.click(syncButton);

    expect(syncButton).toBeDisabled();
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("sync_account_now", {
        accountId: "enabled-account",
      });
    });
    expect(invokeMock).not.toHaveBeenCalledWith("sync_account_now", {
      accountId: "paused-account",
    });
    finishSynchronization?.();
    await screen.findByText("同步完成");
    expect(
      invokeMock.mock.calls.filter(([command]) => command === "get_dashboard"),
    ).toHaveLength(2);
  });

  it("uses a labeled skeleton while the dashboard is loading", () => {
    mockCommand("get_dashboard", new Promise<DashboardDto>(() => undefined));

    renderAppAt();

    expect(
      screen.getByRole("status", { name: "正在加载控制台" }),
    ).toBeInTheDocument();
    expect(screen.getByLabelText("同步状态：同步中")).toBeInTheDocument();
  });

  it("shows quiet empty states when there are no accounts or batches", async () => {
    mockCommand(
      "get_dashboard",
      dashboardFixture({
        mailboxAccounts: [],
        recentlyAddedCount: 0,
        pendingConfirmationCount: 0,
        recognitionFailedCount: 0,
        suspectedDuplicateCount: 0,
        mailNeedsAttentionCount: 0,
        recentBatches: [],
      }),
    );

    renderAppAt();

    expect(await screen.findByText("尚未连接邮箱")).toBeInTheDocument();
    expect(screen.getByText("还没有报销批次")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "立即同步" })).toBeDisabled();
  });

  it("recovers from a dashboard error through an inline retry", async () => {
    const user = userEvent.setup();
    let attempts = 0;
    mockCommand("get_dashboard", () => {
      attempts += 1;
      if (attempts === 1) {
        return Promise.reject({
          code: "external",
          service: "database",
          retryable: true,
          message: "控制台暂时无法加载",
        });
      }
      return dashboardFixture();
    });

    renderAppAt();

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "控制台暂时无法加载",
    );
    await user.click(screen.getByRole("button", { name: "重试" }));
    expect(await screen.findByText("待确认")).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("exposes named navigation with an accessible current item", async () => {
    mockCommand("get_dashboard", dashboardFixture());

    renderAppAt();

    expect(screen.getByRole("navigation", { name: "主导航" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "控制台" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    expect(screen.getByRole("link", { name: "待处理池" })).toHaveAttribute(
      "title",
      "待处理池",
    );
  });
});
