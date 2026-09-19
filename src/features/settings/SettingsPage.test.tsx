import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

import { App } from "../../app/App";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  invokeMock,
  mockCommand,
  resetMockApi,
} from "../../test/mockApi";

const openMock = vi.mocked(open);
const openUrlMock = vi.mocked(openUrl);

const accountFixture = {
  id: "account-finance",
  provider: "gmail" as const,
  email: "finance@gmail.com",
  imapHost: "imap.gmail.com",
  imapPort: 993,
  enabled: true,
  syncIntervalMinutes: 15,
  lastSyncedAt: null,
  lastError: null,
  lastErrorKind: null,
  lastErrorAt: null,
};

const storageFixture = {
  localDataDirectory:
    "/Users/person/Library/Application Support/com.invoice-desk.desktop",
  exportDirectory:
    "/Users/person/Library/Application Support/com.invoice-desk.desktop/storage/exports",
  availableBytes: 128_849_018_880,
  recoveryError: null,
};

function renderAppAt(path: string) {
  window.history.replaceState({}, "", path);
  return render(<App />);
}

function commandCalls(command: string) {
  return invokeMock.mock.calls
    .filter(([wireCommand]) => wireCommand === command)
    .map(([, arguments_]) =>
      typeof arguments_ === "object" &&
      arguments_ !== null &&
      "input" in arguments_
        ? arguments_.input
        : arguments_,
    );
}

function mockSettingsCommands() {
  mockCommand("get_dashboard", new Promise(() => undefined));
  mockCommand("list_mailbox_accounts", []);
  mockCommand("get_preferences", {
    backgroundSyncEnabled: true,
    exportDirectory: "exports",
    batchDirectoryPattern: "{batchName}-{timestamp}",
    markProcessedMailSeen: true,
  });
  mockCommand("get_storage_status", storageFixture);
}

describe("Settings page", () => {
  beforeEach(() => {
    cleanup();
    resetMockApi();
    openMock.mockReset();
    openUrlMock.mockReset();
  });

  it("saves the mail read-marking preference with the runtime settings", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    renderAppAt("/settings");

    const toggle = await screen.findByRole("switch", {
      name: "发票全部提取完成后标记邮件为已读",
    });
    expect(toggle).toBeChecked();

    await user.click(toggle);
    await user.click(screen.getByRole("button", { name: "保存运行设置" }));

    await waitFor(() => expect(commandCalls("save_preferences")).toHaveLength(1));
    expect(commandCalls("save_preferences")[0]).toMatchObject({
      markProcessedMailSeen: false,
    });
  });

  it("tests a Gmail connection before saving the exact account fields", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    mockCommand("test_mailbox_account", undefined);
    mockCommand("save_mailbox_account", {
      id: "account-personal",
      provider: "gmail",
      email: "person@gmail.com",
      imapHost: "imap.gmail.com",
      imapPort: 993,
      enabled: true,
      syncIntervalMinutes: 15,
      lastSyncedAt: null,
      lastError: null,
      lastErrorKind: null,
      lastErrorAt: null,
    });

    renderAppAt("/settings");

    await user.selectOptions(await screen.findByLabelText("邮箱类型"), "gmail");
    await user.type(screen.getByLabelText("邮箱地址"), "person@gmail.com");
    await user.type(screen.getByLabelText("应用专用密码"), "secret");
    expect(screen.getByLabelText("IMAP 服务器")).toHaveValue("imap.gmail.com");
    expect(screen.getByLabelText("IMAP 端口")).toHaveValue(993);
    expect(screen.getByRole("button", { name: "保存账号" })).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "测试连接" }));
    expect(await screen.findByText("连接成功")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "保存账号" }));

    await waitFor(() => {
      expect(commandCalls("save_mailbox_account")).toHaveLength(1);
    });
    expect(commandCalls("test_mailbox_account")[0]).toMatchObject({
      provider: "gmail",
      email: "person@gmail.com",
      secret: "secret",
      imapHost: "imap.gmail.com",
      imapPort: 993,
    });
  });

  it("preserves an existing keychain secret and requires another test after fields change and revert", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    mockCommand("list_mailbox_accounts", [accountFixture]);
    mockCommand("test_mailbox_account", undefined);
    mockCommand("save_mailbox_account", accountFixture);

    renderAppAt("/settings");

    const secret = await screen.findByLabelText("应用专用密码");
    expect(secret).toHaveValue("");
    expect(secret).toHaveAttribute("placeholder", "留空以保留钥匙串中的密码");
    await user.click(screen.getByRole("button", { name: "测试连接" }));
    expect(await screen.findByText("连接成功")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "保存账号" })).toBeEnabled();

    await user.clear(screen.getByLabelText("邮箱地址"));
    await user.type(screen.getByLabelText("邮箱地址"), "changed@gmail.com");
    expect(screen.queryByText("连接成功")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "保存账号" })).toBeDisabled();

    await user.clear(screen.getByLabelText("邮箱地址"));
    await user.type(screen.getByLabelText("邮箱地址"), accountFixture.email);
    expect(screen.getByRole("button", { name: "保存账号" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "测试连接" }));
    expect(await screen.findByText("连接成功")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "保存账号" }));
    await waitFor(() => expect(commandCalls("save_mailbox_account")).toHaveLength(1));
    expect(commandCalls("save_mailbox_account")[0]).toMatchObject({
      id: accountFixture.id,
      secret: "",
    });
  });

  it("clears an existing account replacement secret after saving", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    mockCommand("list_mailbox_accounts", [accountFixture]);
    mockCommand("test_mailbox_account", undefined);
    mockCommand("save_mailbox_account", accountFixture);

    renderAppAt("/settings");

    const secret = await screen.findByLabelText("应用专用密码");
    await user.type(secret, "replacement-secret");
    await user.click(screen.getByRole("button", { name: "测试连接" }));
    await screen.findByText("连接成功");
    await user.click(screen.getByRole("button", { name: "保存账号" }));
    await waitFor(() =>
      expect(commandCalls("save_mailbox_account")).toHaveLength(1),
    );

    expect(secret).toHaveValue("");
  });

  it("ignores a successful connection test when fields change while it is pending", async () => {
    const user = userEvent.setup();
    let resolveConnection!: () => void;
    const pendingConnection = new Promise<void>((resolve) => {
      resolveConnection = resolve;
    });
    mockSettingsCommands();
    mockCommand("list_mailbox_accounts", [accountFixture]);
    mockCommand("test_mailbox_account", () => pendingConnection);

    renderAppAt("/settings");

    await screen.findByLabelText("邮箱地址");
    await user.click(screen.getByRole("button", { name: "测试连接" }));
    await waitFor(() => expect(commandCalls("test_mailbox_account")).toHaveLength(1));
    await user.clear(screen.getByLabelText("邮箱地址"));
    await user.type(screen.getByLabelText("邮箱地址"), "new@gmail.com");

    await act(async () => {
      resolveConnection();
      await pendingConnection;
    });

    expect(screen.queryByText("连接成功")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "保存账号" })).toBeDisabled();
  });

  it("applies QQ defaults, opens credential help, and enforces sync interval bounds", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    mockCommand("test_mailbox_account", undefined);
    openUrlMock.mockResolvedValue(undefined);

    renderAppAt("/settings");

    await user.selectOptions(await screen.findByLabelText("邮箱类型"), "qq");
    expect(screen.getByLabelText("IMAP 服务器")).toHaveValue("imap.qq.com");
    expect(screen.getByLabelText("IMAP 端口")).toHaveValue(993);
    await user.click(screen.getByRole("button", { name: "查看 QQ 邮箱授权码说明" }));
    expect(openUrlMock).toHaveBeenCalledWith(expect.stringMatching(/^https:\/\//));

    await user.type(screen.getByLabelText("邮箱地址"), "finance@qq.com");
    await user.type(screen.getByLabelText("应用专用密码"), "authorization-code");
    await user.clear(screen.getByLabelText("同步间隔（分钟）"));
    await user.type(screen.getByLabelText("同步间隔（分钟）"), "4");
    await user.click(screen.getByRole("button", { name: "测试连接" }));
    await screen.findByText("连接成功");
    expect(screen.getByRole("button", { name: "保存账号" })).toBeDisabled();

    await user.clear(screen.getByLabelText("同步间隔（分钟）"));
    await user.type(screen.getByLabelText("同步间隔（分钟）"), "5");
    expect(screen.getByRole("button", { name: "保存账号" })).toBeEnabled();
    expect(screen.getByRole("switch", { name: "启用此账号" })).toBeChecked();
  });

  it("adds another mailbox account and closes the temporary form after saving", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    mockCommand("list_mailbox_accounts", [accountFixture]);
    mockCommand("test_mailbox_account", undefined);
    mockCommand("save_mailbox_account", {
      ...accountFixture,
      id: "account-qq",
      provider: "qq",
      email: "finance@qq.com",
      imapHost: "imap.qq.com",
    });

    renderAppAt("/settings");

    await screen.findByRole("form", { name: accountFixture.email });
    expect(screen.queryByRole("form", { name: "新邮箱账号" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "添加邮箱账号" }));
    const newAccountForm = screen.getByRole("form", { name: "新邮箱账号" });
    await user.selectOptions(within(newAccountForm).getByLabelText("邮箱类型"), "qq");
    await user.type(within(newAccountForm).getByLabelText("邮箱地址"), "finance@qq.com");
    await user.type(
      within(newAccountForm).getByLabelText("应用专用密码"),
      "authorization-code",
    );
    await user.click(within(newAccountForm).getByRole("button", { name: "测试连接" }));
    await within(newAccountForm).findByText("连接成功");
    await user.click(within(newAccountForm).getByRole("button", { name: "保存账号" }));

    await waitFor(() => {
      expect(screen.queryByRole("form", { name: "新邮箱账号" })).not.toBeInTheDocument();
    });
    expect(screen.getByRole("button", { name: "添加邮箱账号" })).toBeInTheDocument();
  });

  it("shows storage status and saves the selected future export directory", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    openMock.mockResolvedValue("/Users/person/Reimbursement Exports");
    mockCommand("save_preferences", {
      backgroundSyncEnabled: false,
      exportDirectory: "/Users/person/Reimbursement Exports",
      batchDirectoryPattern: "{batchName}-{timestamp}",
      markProcessedMailSeen: true,
    });

    renderAppAt("/settings");

    expect(await screen.findByText(storageFixture.localDataDirectory)).toBeInTheDocument();
    expect(screen.getByText(storageFixture.exportDirectory)).toBeInTheDocument();
    expect(screen.getByText("120 GB 可用")).toBeInTheDocument();
    expect(screen.getByText("7 月报销-20260717-143025")).toBeInTheDocument();
    await user.click(screen.getByRole("switch", { name: "后台自动同步" }));
    await user.click(screen.getByRole("button", { name: "选择导出目录" }));
    expect(screen.getByDisplayValue("/Users/person/Reimbursement Exports")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "保存运行设置" }));

    await waitFor(() => expect(commandCalls("save_preferences")).toHaveLength(1));
    expect(commandCalls("save_preferences")[0]).toEqual({
      backgroundSyncEnabled: false,
      exportDirectory: "/Users/person/Reimbursement Exports",
      batchDirectoryPattern: "{batchName}-{timestamp}",
      markProcessedMailSeen: true,
    });
  });

  it("exports a diagnostics bundle and reports where it was written", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    mockCommand("export_diagnostics", {
      path: "/Users/person/Downloads/invoice-diagnostics-20260918-070000.zip",
      directory: "/Users/person/Downloads",
      bytes: 2048,
    });

    renderAppAt("/settings");

    await user.click(await screen.findByRole("button", { name: "导出诊断包" }));

    expect(
      await screen.findByText(
        "/Users/person/Downloads/invoice-diagnostics-20260918-070000.zip",
      ),
    ).toBeInTheDocument();
    expect(commandCalls("export_diagnostics")).toHaveLength(1);
  });

  it("keeps settings editable when storage status cannot be loaded", async () => {
    const user = userEvent.setup();
    let unavailable = true;
    mockSettingsCommands();
    mockCommand("get_storage_status", () => {
      if (unavailable) throw new Error("storage temporarily unavailable");
      return storageFixture;
    });

    renderAppAt("/settings");

    const alert = await screen.findByRole("alert", { name: "无法读取存储状态" });
    expect(screen.getByRole("button", { name: "选择导出目录" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "保存运行设置" })).toBeEnabled();
    expect(screen.getAllByText("存储状态读取失败")).not.toHaveLength(0);
    expect(screen.queryByText("正在读取")).not.toBeInTheDocument();

    unavailable = false;
    await user.click(within(alert).getByRole("button", { name: "重试" }));
    expect(await screen.findByText(storageFixture.exportDirectory)).toBeInTheDocument();
  });

  it("shows a retryable export recovery error without hiding directory controls", async () => {
    const user = userEvent.setup();
    let unavailable = true;
    mockSettingsCommands();
    mockCommand("get_storage_status", () =>
      unavailable
        ? {
            ...storageFixture,
            exportDirectory: "/Volumes/Finance/Exports",
            availableBytes: null,
            recoveryError: "saved export directory is unavailable",
          }
        : storageFixture,
    );
    mockCommand("retry_export_recovery", () => {
      unavailable = false;
    });

    renderAppAt("/settings");

    const alert = await screen.findByRole("alert", { name: "导出恢复失败" });
    expect(alert).toHaveTextContent("saved export directory is unavailable");
    expect(screen.getByRole("button", { name: "选择导出目录" })).toBeEnabled();
    expect(screen.getByText("存储目录不可用")).toBeInTheDocument();

    await user.click(within(alert).getByRole("button", { name: "重试导出恢复" }));

    await waitFor(() =>
      expect(commandCalls("retry_export_recovery")).toHaveLength(1),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("alert", { name: "导出恢复失败" }),
      ).not.toBeInTheDocument(),
    );
  });

  it("refreshes the recovery status after a manual retry fails", async () => {
    const user = userEvent.setup();
    let retryFailed = false;
    mockSettingsCommands();
    mockCommand("get_storage_status", () => ({
      ...storageFixture,
      availableBytes: null,
      recoveryError: retryFailed
        ? "latest recovery failure"
        : "initial recovery failure",
    }));
    mockCommand("retry_export_recovery", () => {
      retryFailed = true;
      throw new Error("recovery retry failed");
    });

    renderAppAt("/settings");

    const alert = await screen.findByRole("alert", { name: "导出恢复失败" });
    expect(alert).toHaveTextContent("initial recovery failure");
    await user.click(within(alert).getByRole("button", { name: "重试导出恢复" }));

    await waitFor(() =>
      expect(commandCalls("retry_export_recovery")).toHaveLength(1),
    );
    expect(await screen.findByText("latest recovery failure")).toBeInTheDocument();
    expect(screen.queryByText("initial recovery failure")).not.toBeInTheDocument();
  });

  it("retries a failed settings load and focuses a linked mailbox form", async () => {
    const user = userEvent.setup();
    let shouldFail = true;
    mockSettingsCommands();
    mockCommand("list_mailbox_accounts", () => {
      if (shouldFail) throw new Error("database unavailable");
      return [accountFixture];
    });

    renderAppAt(`/settings?account=${accountFixture.id}`);
    expect(await screen.findByRole("alert")).toHaveTextContent("无法加载邮箱设置");
    shouldFail = false;
    await user.click(screen.getByRole("button", { name: "重试" }));

    const form = await screen.findByRole("form", { name: accountFixture.email });
    await waitFor(() => expect(form).toHaveFocus());
  });

  it("links an authentication failure directly to the affected mailbox settings", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    mockCommand("list_mailbox_accounts", [accountFixture]);
    mockCommand("get_dashboard", {
      mailboxAccounts: [
        {
          ...accountFixture,
          lastError: "IMAP authentication failed",
          lastErrorKind: "authentication",
          lastErrorAt: "2026-07-17T09:40:00+08:00",
        },
      ],
      recentlyAddedCount: 0,
      pendingConfirmationCount: 0,
      recognitionFailedCount: 0,
      suspectedDuplicateCount: 0,
      mailNeedsAttentionCount: 0,
      recentBatches: [],
    });
    renderAppAt("/");
    const banner = await screen.findByRole("alert", { name: "邮箱授权失效" });
    expect(banner).toHaveTextContent(accountFixture.email);
    await user.click(within(banner).getByRole("link", { name: "前往账号设置" }));

    const form = await screen.findByRole("form", { name: accountFixture.email });
    await waitFor(() => expect(form).toHaveFocus());
  });

  it("polls the dashboard so a durable mailbox failure appears without remounting", async () => {
    vi.useFakeTimers();
    try {
      let failed = false;
      mockSettingsCommands();
      mockCommand("get_dashboard", () => ({
        mailboxAccounts: failed
          ? [
              {
                ...accountFixture,
                lastError: "IMAP authentication failed",
                lastErrorKind: "authentication",
                lastErrorAt: "2026-07-17T09:40:00+08:00",
              },
            ]
          : [accountFixture],
        recentlyAddedCount: 0,
        pendingConfirmationCount: 0,
        recognitionFailedCount: 0,
        suspectedDuplicateCount: 0,
        mailNeedsAttentionCount: 0,
        recentBatches: [],
      }));

      renderAppAt("/settings");
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(commandCalls("get_dashboard")).toHaveLength(1);
      expect(
        screen.queryByRole("alert", { name: "邮箱授权失效" }),
      ).not.toBeInTheDocument();

      failed = true;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15_000);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1);
      });

      expect(commandCalls("get_dashboard")).toHaveLength(2);
      expect(
        screen.getByRole("alert", { name: "邮箱授权失效" }),
      ).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows unknown mailbox errors without claiming authorization failed", async () => {
    mockSettingsCommands();
    mockCommand("get_dashboard", {
      mailboxAccounts: [
        {
          ...accountFixture,
          lastError: "IMAP configuration failed",
          lastErrorKind: "unknown",
          lastErrorAt: "2026-07-17T09:40:00+08:00",
        },
      ],
      recentlyAddedCount: 0,
      pendingConfirmationCount: 0,
      recognitionFailedCount: 0,
      suspectedDuplicateCount: 0,
      mailNeedsAttentionCount: 0,
      recentBatches: [],
    });

    renderAppAt("/");

    const banner = await screen.findByRole("alert", { name: "邮箱同步失败" });
    expect(banner).toHaveTextContent("2026/07/17");
    expect(banner).not.toHaveTextContent("邮箱授权失效");
    expect(
      within(banner).getByRole("link", { name: "检查账号设置" }),
    ).toHaveAttribute("href", `/settings?account=${accountFixture.id}`);
  });

  it("retries the exact network-failed account and clears durable error queries", async () => {
    const user = userEvent.setup();
    let failed = true;
    const failedAccount = {
      ...accountFixture,
      lastError: "network timeout",
      lastErrorKind: "network",
      lastErrorAt: "2026-07-17T09:40:00+08:00",
    } as const;
    mockCommand("get_dashboard", () => ({
      mailboxAccounts: failed ? [failedAccount] : [accountFixture],
      recentlyAddedCount: 0,
      pendingConfirmationCount: 0,
      recognitionFailedCount: 0,
      suspectedDuplicateCount: 0,
      mailNeedsAttentionCount: 0,
      recentBatches: [],
    }));
    mockCommand("list_mailbox_accounts", () =>
      failed ? [failedAccount] : [accountFixture],
    );
    mockCommand("sync_account_now", () => {
      failed = false;
    });
    mockCommand("get_preferences", {
      backgroundSyncEnabled: true,
      exportDirectory: "exports",
      batchDirectoryPattern: "{batchName}-{timestamp}",
      markProcessedMailSeen: true,
    });
    mockCommand("get_storage_status", storageFixture);

    renderAppAt("/settings");

    const banner = await screen.findByRole("alert", { name: "邮箱网络连接失败" });
    expect(banner).toHaveTextContent("2026/07/17");
    await user.click(within(banner).getByRole("button", { name: "立即重试" }));
    await waitFor(() => expect(commandCalls("sync_account_now")).toHaveLength(1));
    expect(commandCalls("sync_account_now")[0]).toEqual({
      accountId: accountFixture.id,
    });
    await waitFor(() => {
      expect(
        screen.queryByRole("alert", { name: "邮箱网络连接失败" }),
      ).not.toBeInTheDocument();
    });
  });
});
