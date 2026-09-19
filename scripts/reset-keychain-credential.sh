#!/usr/bin/env bash
#
# Reset the mailbox credential when the app can no longer read it.
#
# Background, measured on macOS 26 on 2026-09-19: rebuilding the app changes its
# ad-hoc cdhash, and the keychain entry written by the previous build then no
# longer recognises the new one. Two command line repairs were tried and neither
# restored access:
#
#   * `security add-generic-password -T /Applications/发票报销.app` puts the app
#     in the entry's trusted list, but the entry's authorization bits do not
#     cover reading the secret, so the app still blocks and then reports
#     "读取邮箱凭据超时".
#   * `-A` (allow any application) is accepted but has no effect.
#
# The only reliable repair is to let the app own the entry again: delete it, then
# save the mailbox account in the app once. This script automates the part that
# can be automated and explains the one step a person has to do.
#
#   bash scripts/reset-keychain-credential.sh
#
# It never prints the authorization code. When it can read the code it copies it
# to the clipboard so the app save is a single paste; when it cannot (a broken
# entry is usually unreadable from the command line too) it says so, and the code
# has to come from QQ 邮箱 → 设置 → 账户 → POP3/IMAP/SMTP 服务.
set -euo pipefail

SERVICE="com.invoice-desk.credentials"
DATABASE="$HOME/Library/Application Support/com.invoice-desk.desktop/invoice.sqlite3"
TIMEOUT_SECONDS=15

# macOS ships no `timeout`, and a keychain call waiting for an authorization
# dialog never returns on its own.
with_timeout() {
  "$@" &
  local pid=$!
  local waited=0
  while [ "$waited" -lt "$TIMEOUT_SECONDS" ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid"
      return $?
    fi
    sleep 1
    waited=$((waited + 1))
  done
  kill -9 "$pid" 2>/dev/null || true
  # A killed request can leave securityd waiting on a dialog nobody sees.
  pkill -9 -f "SecurityAgent.bundle" 2>/dev/null || true
  return 124
}

if [ ! -f "$DATABASE" ]; then
  echo "找不到数据库：$DATABASE" >&2
  echo "先启动一次 App，让它创建数据目录。" >&2
  exit 1
fi

accounts="$(sqlite3 "$DATABASE" "SELECT id FROM mailbox_accounts ORDER BY created_at;")"
if [ -z "$accounts" ]; then
  echo "还没有已保存的邮箱账号，无需处理。"
  exit 0
fi

preserved=0
while IFS= read -r account; do
  [ -n "$account" ] || continue
  echo "账号 ${account:0:8}…"
  if code="$(with_timeout security find-generic-password -s "$SERVICE" -a "$account" -w 2>/dev/null)" \
     && [ -n "$code" ]; then
    printf '%s' "$code" | pbcopy
    echo "  已把授权码复制到剪贴板（未打印）"
    preserved=1
  else
    echo "  读取不到现有授权码：需要你从 QQ 邮箱重新获取（设置 → 账户 → POP3/IMAP/SMTP 服务）。"
  fi
  unset code
  with_timeout security delete-generic-password -s "$SERVICE" -a "$account" >/dev/null 2>&1 || true
  echo "  已删除旧条目"
done <<< "$accounts"

echo
echo "接下来（只能手动做，一次即可）："
echo "  1. 打开 App → 运行设置 → 邮箱账号"
echo "  2. 在「应用专用密码」里粘贴授权码（若刚才已复制到剪贴板，按 ⌘V 即可）"
echo "  3. 点「测试连接」，看到「连接成功」后点「保存账号」"
echo
echo "App 会自己重建钥匙串条目：创建者身份才拥有完整授权，之后不会再弹窗。"
echo "保存成功后回到 App 点一次「立即同步」：凭据读取失败会把账号标记为暂停自动同步，"
echo "一次成功的手动同步就会清除它。"
if [ "$preserved" -eq 0 ]; then
  echo
  echo "注意：本次没能保留授权码，请先从 QQ 邮箱获取后再操作第 2 步。"
fi
