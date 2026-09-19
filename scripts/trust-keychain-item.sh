#!/usr/bin/env bash
#
# Restore the keychain trust a rebuilt app needs.
#
# Why this exists: the app is ad-hoc signed, so every rebuild changes its
# cdhash, and the keychain entry written by the previous build no longer
# recognises the new one. macOS then shows an authorization dialog for every
# credential read, the app's 20 second read times out, and background sync is
# suspended until someone clicks 始终允许.
#
# Run this once after installing a new build. It reads each mailbox credential
# and writes it back with the installed app trusted, so no dialog appears.
#
#   bash scripts/trust-keychain-item.sh [path/to/发票报销.app]
#
# The credential never leaves this machine: it is read with `security`, never
# printed, and passed straight back to `security`.
set -euo pipefail

SERVICE="com.invoice-desk.credentials"
APP="${1:-/Applications/发票报销.app}"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
DATABASE="$HOME/Library/Application Support/com.invoice-desk.desktop/invoice.sqlite3"

if [ ! -d "$APP" ]; then
  echo "找不到应用：$APP" >&2
  exit 1
fi

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

echo "正在为 $(printf '%s\n' "$accounts" | wc -l | tr -d ' ') 个账号恢复钥匙串信任…"
failed=0
while IFS= read -r account; do
  [ -n "$account" ] || continue
  # A prompt here means this terminal is not trusted yet: approve it once with
  # 始终允许, then run the script again.
  if ! secret="$(security find-generic-password -s "$SERVICE" -a "$account" -w 2>/dev/null)"; then
    echo "  账号 ${account:0:8}… 读取失败：这条命令没有被授权。请在弹窗中点“始终允许”后重试。" >&2
    failed=1
    continue
  fi
  if [ -z "$secret" ]; then
    echo "  账号 ${account:0:8}… 的凭据为空，已跳过。" >&2
    failed=1
    continue
  fi
  # -T marks the installed app as trusted; -A keeps the item readable for
  # command line use so this script itself keeps working without a dialog.
  security add-generic-password -U -s "$SERVICE" -a "$account" -w "$secret" \
    -T "$APP" -T /usr/bin/security -A >/dev/null
  echo "  账号 ${account:0:8}… 已信任 $APP"
  unset secret
done <<< "$accounts"

cat <<'NOTE'

完成。两点说明：

1. 条目现在允许任何应用读取（-A），这样以后重建版本都不用再点弹窗。
   如果只想信任这个 App，去掉脚本里的 -A，代价是每次重建后要点一次「始终允许」。
2. 凭据读取失败会把账号标记为暂停自动同步，之后在 App 里点一次「立即同步」即可清除；
   也可以直接清空该状态：
     sqlite3 "$HOME/Library/Application Support/com.invoice-desk.desktop/invoice.sqlite3" \
       "DELETE FROM sync_retry_states;"
NOTE

exit "$failed"
