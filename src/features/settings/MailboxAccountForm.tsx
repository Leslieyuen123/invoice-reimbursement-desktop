import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Check, LoaderCircle, Save, TestTube2 } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useMemo, useRef, useState } from "react";

import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import type {
  MailboxAccountDto,
  MailboxProvider,
  SaveMailboxAccountInputDto,
  TestMailboxAccountInputDto,
} from "../../types";

const PROVIDER_DEFAULTS: Record<MailboxProvider, { host: string; port: number }> = {
  gmail: { host: "imap.gmail.com", port: 993 },
  qq: { host: "imap.qq.com", port: 993 },
};

const PROVIDER_HELP: Record<
  MailboxProvider,
  { label: string; url: string }
> = {
  gmail: {
    label: "查看 Gmail 应用专用密码说明",
    url: "https://support.google.com/accounts/answer/185833",
  },
  qq: {
    label: "查看 QQ 邮箱授权码说明",
    url: "https://service.mail.qq.com/detail/0/75",
  },
};

function errorMessage(error: unknown) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return error.message;
  }
  return "操作失败，请检查设置后重试";
}

export function MailboxAccountForm({
  account,
  onSaved,
}: {
  account?: MailboxAccountDto;
  onSaved?: () => void;
}) {
  const queryClient = useQueryClient();
  const initialProvider = account?.provider ?? "gmail";
  const [provider, setProvider] = useState<MailboxProvider>(initialProvider);
  const [email, setEmail] = useState(account?.email ?? "");
  const [secret, setSecret] = useState("");
  const [imapHost, setImapHost] = useState(
    account?.imapHost ?? PROVIDER_DEFAULTS[initialProvider].host,
  );
  const [imapPort, setImapPort] = useState(
    String(account?.imapPort ?? PROVIDER_DEFAULTS[initialProvider].port),
  );
  const [enabled, setEnabled] = useState(account?.enabled ?? true);
  const [syncIntervalMinutes, setSyncIntervalMinutes] = useState(
    String(account?.syncIntervalMinutes ?? 15),
  );
  const connectionRevision = useRef(0);
  const [testedRevision, setTestedRevision] = useState<number | null>(null);
  const [saved, setSaved] = useState(false);

  const connectionInput = useMemo<TestMailboxAccountInputDto>(
    () => ({
      id: account?.id ?? null,
      provider,
      email: email.trim(),
      secret,
      imapHost: imapHost.trim(),
      imapPort: Number(imapPort),
    }),
    [account?.id, email, imapHost, imapPort, provider, secret],
  );
  const interval = Number(syncIntervalMinutes);
  const connectionIsValid =
    email.trim().length > 0 &&
    (account !== undefined || secret.trim().length > 0) &&
    imapHost.trim().length > 0 &&
    Number.isInteger(Number(imapPort)) &&
    Number(imapPort) >= 1 &&
    Number(imapPort) <= 65_535;
  const settingsAreValid =
    Number.isInteger(interval) && interval >= 5 && interval <= 1_440;

  const testMutation = useMutation({
    mutationFn: ({ input }: { input: TestMailboxAccountInputDto; revision: number }) =>
      api.testMailboxAccount(input),
    onSuccess: (_, attempt) => {
      if (attempt.revision === connectionRevision.current) {
        setTestedRevision(attempt.revision);
        setSaved(false);
      }
    },
  });
  const saveMutation = useMutation({
    mutationFn: () => {
      const input: SaveMailboxAccountInputDto = {
        ...connectionInput,
        enabled,
        syncIntervalMinutes: interval,
      };
      return api.saveMailboxAccount(input);
    },
    onSuccess: async () => {
      setSaved(true);
      onSaved?.();
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: queryKeys.mailboxAccounts }),
        queryClient.invalidateQueries({ queryKey: queryKeys.dashboard }),
      ]);
    },
  });

  function selectProvider(nextProvider: MailboxProvider) {
    invalidateConnectionTest();
    setProvider(nextProvider);
    setImapHost(PROVIDER_DEFAULTS[nextProvider].host);
    setImapPort(String(PROVIDER_DEFAULTS[nextProvider].port));
  }

  function invalidateConnectionTest() {
    connectionRevision.current += 1;
    setTestedRevision(null);
    setSaved(false);
    testMutation.reset();
  }

  return (
    <form
      className="mailbox-account-form"
      id={`mailbox-account-${account?.id ?? "new"}`}
      aria-label={account?.email ?? "新邮箱账号"}
      tabIndex={-1}
      onSubmit={(event) => {
        event.preventDefault();
        if (testedRevision === connectionRevision.current && settingsAreValid) {
          saveMutation.mutate();
        }
      }}
    >
      <div className="mailbox-form-grid">
        <label className="settings-field">
          <span>邮箱类型</span>
          <select
            value={provider}
            onChange={(event) =>
              selectProvider(event.target.value as MailboxProvider)
            }
          >
            <option value="gmail">Gmail</option>
            <option value="qq">QQ 邮箱</option>
          </select>
        </label>

        <label className="settings-field settings-field-wide">
          <span>邮箱地址</span>
          <input
            type="email"
            value={email}
            onChange={(event) => {
              invalidateConnectionTest();
              setEmail(event.target.value);
            }}
            autoComplete="email"
          />
        </label>

        <label className="settings-field settings-field-wide">
          <span>应用专用密码</span>
          <input
            type="password"
            value={secret}
            onChange={(event) => {
              invalidateConnectionTest();
              setSecret(event.target.value);
            }}
            autoComplete="new-password"
            placeholder={account ? "留空以保留钥匙串中的密码" : undefined}
          />
          <button
            className="settings-help-link"
            type="button"
            aria-label={PROVIDER_HELP[provider].label}
            onClick={() => void openUrl(PROVIDER_HELP[provider].url)}
          >
            {provider === "gmail" ? "获取应用专用密码" : "获取邮箱授权码"}
          </button>
        </label>

        <details className="mailbox-advanced settings-field-wide">
          <summary>高级连接设置</summary>
          <div className="mailbox-form-grid">
            <label className="settings-field settings-field-wide">
              <span>IMAP 服务器</span>
              <input
                value={imapHost}
                onChange={(event) => {
                  invalidateConnectionTest();
                  setImapHost(event.target.value);
                }}
                autoComplete="off"
              />
            </label>
            <label className="settings-field">
              <span>IMAP 端口</span>
              <input
                type="number"
                min="1"
                max="65535"
                value={imapPort}
                onChange={(event) => {
                  invalidateConnectionTest();
                  setImapPort(event.target.value);
                }}
              />
            </label>
          </div>
        </details>

        <label className="settings-field">
          <span>同步间隔（分钟）</span>
          <input
            type="number"
            min="5"
            max="1440"
            value={syncIntervalMinutes}
            onChange={(event) => setSyncIntervalMinutes(event.target.value)}
          />
        </label>

        <label className="settings-switch">
          <input
            type="checkbox"
            role="switch"
            checked={enabled}
            onChange={(event) => setEnabled(event.target.checked)}
          />
          <span>启用此账号</span>
        </label>
      </div>

      <div className="mailbox-form-actions">
        <button
          className="button button-secondary"
          type="button"
          disabled={!connectionIsValid || testMutation.isPending}
          onClick={() =>
            testMutation.mutate({
              input: connectionInput,
              revision: connectionRevision.current,
            })
          }
        >
          {testMutation.isPending ? (
            <LoaderCircle size={15} aria-hidden="true" />
          ) : (
            <TestTube2 size={15} aria-hidden="true" />
          )}
          测试连接
        </button>
        <button
          className="button button-primary"
          type="submit"
          disabled={
            testedRevision !== connectionRevision.current ||
            !settingsAreValid ||
            saveMutation.isPending
          }
        >
          <Save size={15} aria-hidden="true" />
          保存账号
        </button>
        <span className="mailbox-form-feedback" aria-live="polite">
          {testMutation.isSuccess && testedRevision === connectionRevision.current ? (
            <><Check size={14} aria-hidden="true" />连接成功</>
          ) : null}
          {saved ? "账号已保存" : null}
          {testMutation.isError ? errorMessage(testMutation.error) : null}
          {saveMutation.isError ? errorMessage(saveMutation.error) : null}
        </span>
      </div>
    </form>
  );
}
