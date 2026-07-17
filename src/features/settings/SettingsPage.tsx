import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { open } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, FolderOpen, HardDrive, Mail, Plus, Save } from "lucide-react";
import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";

import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import { MailboxAccountForm } from "./MailboxAccountForm";
import "./SettingsPage.css";

export function SettingsPage() {
  const queryClient = useQueryClient();
  const [searchParams] = useSearchParams();
  const accountsQuery = useQuery({
    queryKey: queryKeys.mailboxAccounts,
    queryFn: api.listMailboxAccounts,
  });
  const preferencesQuery = useQuery({
    queryKey: queryKeys.preferences,
    queryFn: api.getPreferences,
  });
  const storageQuery = useQuery({
    queryKey: queryKeys.storageStatus,
    queryFn: api.getStorageStatus,
  });
  const [backgroundSyncEnabled, setBackgroundSyncEnabled] = useState(true);
  const [exportDirectory, setExportDirectory] = useState("exports");
  const [isAddingAccount, setIsAddingAccount] = useState(false);

  useEffect(() => {
    if (preferencesQuery.data) {
      setBackgroundSyncEnabled(preferencesQuery.data.backgroundSyncEnabled);
      setExportDirectory(preferencesQuery.data.exportDirectory);
    }
  }, [preferencesQuery.data]);

  useEffect(() => {
    const accountId = searchParams.get("account");
    if (!accountsQuery.data || !accountId) return;
    document.getElementById(`mailbox-account-${accountId}`)?.focus();
  }, [accountsQuery.data, searchParams]);

  const preferencesMutation = useMutation({
    mutationFn: () =>
      api.savePreferences({
        backgroundSyncEnabled,
        exportDirectory,
        batchDirectoryPattern: "{batchName}-{timestamp}",
      }),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: queryKeys.preferences }),
        queryClient.invalidateQueries({ queryKey: queryKeys.storageStatus }),
      ]);
    },
  });
  const recoveryMutation = useMutation({
    mutationFn: api.retryExportRecovery,
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: queryKeys.storageStatus });
    },
  });

  const isPending = accountsQuery.isPending || preferencesQuery.isPending;
  const isError = accountsQuery.isError || preferencesQuery.isError;

  if (isPending) {
    return (
      <section className="settings-page" aria-busy="true">
        <div className="settings-skeleton" role="status" aria-label="正在加载设置" />
      </section>
    );
  }

  if (isError) {
    return (
      <section className="settings-page">
        <div className="settings-load-error" role="alert">
          <AlertTriangle size={18} aria-hidden="true" />
          <span>无法加载邮箱设置</span>
          <button
            className="button button-secondary"
            type="button"
            onClick={() =>
              void Promise.all([
                accountsQuery.refetch(),
                preferencesQuery.refetch(),
              ])
            }
          >
            重试
          </button>
        </div>
      </section>
    );
  }

  const accounts = accountsQuery.data;
  const storage = storageQuery.data;
  const storageStatusFallback = storageQuery.isError
    ? "存储状态读取失败"
    : "正在读取";

  async function chooseExportDirectory() {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "选择未来报销包的导出目录",
    });
    if (typeof selected === "string") setExportDirectory(selected);
  }

  return (
    <section className="settings-page">
      <header className="settings-heading">
        <div>
          <h1>运行设置</h1>
          <p>管理邮箱抓取、本地存储和后台同步。</p>
        </div>
      </header>

      <section className="settings-section" aria-labelledby="mailbox-settings-title">
        <div className="settings-section-heading">
          <Mail size={17} aria-hidden="true" />
          <div>
            <h2 id="mailbox-settings-title">邮箱账号</h2>
            <p>连接信息仅保存在此 Mac。</p>
          </div>
        </div>

        {accounts.length === 0 ? (
          <div className="settings-empty">尚未配置邮箱账号</div>
        ) : null}
        {accounts.map((account) => (
          <MailboxAccountForm key={account.id} account={account} />
        ))}
        {accounts.length === 0 || isAddingAccount ? (
          <MailboxAccountForm
            onSaved={isAddingAccount ? () => setIsAddingAccount(false) : undefined}
          />
        ) : (
          <button
            className="button button-secondary settings-add-account"
            type="button"
            onClick={() => setIsAddingAccount(true)}
          >
            <Plus size={15} aria-hidden="true" />
            添加邮箱账号
          </button>
        )}
      </section>

      <section className="settings-section" aria-labelledby="runtime-settings-title">
        <div className="settings-section-heading">
          <HardDrive size={17} aria-hidden="true" />
          <div>
            <h2 id="runtime-settings-title">同步与存储</h2>
            <p>目录变更只影响之后创建的报销包。</p>
          </div>
        </div>

        {storageQuery.isError ? (
          <div
            className="settings-load-error settings-storage-alert"
            role="alert"
            aria-label="无法读取存储状态"
          >
            <AlertTriangle size={18} aria-hidden="true" />
            <span>无法读取存储状态，仍可选择并保存新的导出目录。</span>
            <button
              className="button button-secondary"
              type="button"
              onClick={() => void storageQuery.refetch()}
            >
              重试
            </button>
          </div>
        ) : null}

        {storage?.recoveryError ? (
          <div
            className="settings-load-error settings-storage-alert"
            role="alert"
            aria-label="导出恢复失败"
          >
            <AlertTriangle size={18} aria-hidden="true" />
            <span>{storage.recoveryError}</span>
            <button
              className="button button-secondary"
              type="button"
              disabled={recoveryMutation.isPending}
              onClick={() => recoveryMutation.mutate()}
            >
              重试导出恢复
            </button>
          </div>
        ) : null}

        <div className="settings-runtime-grid">
          <label className="settings-switch settings-runtime-switch">
            <input
              type="checkbox"
              role="switch"
              checked={backgroundSyncEnabled}
              onChange={(event) => setBackgroundSyncEnabled(event.target.checked)}
            />
            <span>后台自动同步</span>
          </label>

          <div className="settings-storage-row">
            <span>本地数据目录</span>
            <code>{storage?.localDataDirectory ?? storageStatusFallback}</code>
          </div>
          <div className="settings-storage-row">
            <span>当前有效导出目录</span>
            <code>{storage?.exportDirectory ?? storageStatusFallback}</code>
          </div>
          <div className="settings-storage-row">
            <span>可用空间</span>
            <strong>
              {storage?.availableBytes == null
                ? storageQuery.isError
                  ? "存储状态读取失败"
                  : storageQuery.isPending
                    ? "正在读取"
                  : "存储目录不可用"
                : `${formatBytes(storage.availableBytes)} 可用`}
            </strong>
          </div>

          <label className="settings-field settings-export-field">
            <span>之后的导出目录</span>
            <span className="settings-directory-control">
              <input value={exportDirectory} readOnly />
              <button
                className="button button-secondary"
                type="button"
                onClick={() => void chooseExportDirectory()}
              >
                <FolderOpen size={15} aria-hidden="true" />
                选择导出目录
              </button>
            </span>
          </label>

          <div className="settings-pattern-preview">
            <span>批次目录命名预览</span>
            <code>7 月报销-20260717-143025</code>
            <small>{"{batchName}-{timestamp}"}</small>
          </div>
        </div>

        <div className="settings-runtime-actions">
          <button
            className="button button-primary"
            type="button"
            disabled={preferencesMutation.isPending}
            onClick={() => preferencesMutation.mutate()}
          >
            <Save size={15} aria-hidden="true" />
            保存运行设置
          </button>
          <span aria-live="polite">
            {preferencesMutation.isSuccess ? "运行设置已保存" : null}
            {preferencesMutation.isError ? "运行设置保存失败" : null}
          </span>
        </div>
      </section>
    </section>
  );
}

function formatBytes(bytes: number) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = Math.max(0, bytes);
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = value >= 10 || Number.isInteger(value) ? 0 : 1;
  return `${value.toFixed(digits)} ${units[unit]}`;
}
