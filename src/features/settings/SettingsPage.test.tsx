import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
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
    "/Users/person/Library/Application Support/com.invoice-desk.app",
  exportDirectory:
    "/Users/person/Library/Application Support/com.invoice-desk.app/storage/exports",
  availableBytes: 128_849_018_880,
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

  it("preserves an existing keychain secret and invalidates a tested connection after edits", async () => {
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
    await user.click(screen.getByRole("button", { name: "保存账号" }));
    await waitFor(() => expect(commandCalls("save_mailbox_account")).toHaveLength(1));
    expect(commandCalls("save_mailbox_account")[0]).toMatchObject({
      id: accountFixture.id,
      secret: "",
    });
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

  it("shows storage status and saves the selected future export directory", async () => {
    const user = userEvent.setup();
    mockSettingsCommands();
    openMock.mockResolvedValue("/Users/person/Reimbursement Exports");
    mockCommand("save_preferences", {
      backgroundSyncEnabled: false,
      exportDirectory: "/Users/person/Reimbursement Exports",
      batchDirectoryPattern: "{batchName}-{timestamp}",
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
    });
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
      recentBatches: [],
    });
    renderAppAt("/");
    const banner = await screen.findByRole("alert", { name: "邮箱授权失效" });
    expect(banner).toHaveTextContent(accountFixture.email);
    await user.click(within(banner).getByRole("link", { name: "前往账号设置" }));

    const form = await screen.findByRole("form", { name: accountFixture.email });
    await waitFor(() => expect(form).toHaveFocus());
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
