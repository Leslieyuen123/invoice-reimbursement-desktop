#!/usr/bin/env bash
#
# Build the app with the stable local signing identity.
#
# Ad-hoc signing changes the cdhash on every build, which makes macOS ask for
# the mailbox credential again each time (see docs/operations/local-code-signing.md).
# Signing with a fixed certificate keeps the designated requirement identical
# across rebuilds, so the keychain keeps trusting the app.
set -euo pipefail

IDENTITY="${APPLE_SIGNING_IDENTITY:-Invoice Desk Local Signing}"

if ! security find-certificate -c "$IDENTITY" >/dev/null 2>&1; then
  echo "找不到签名身份：$IDENTITY" >&2
  echo "先按 docs/operations/local-code-signing.md 创建并导入证书，或用 APPLE_SIGNING_IDENTITY 指定其他身份。" >&2
  exit 1
fi

echo "使用签名身份：$IDENTITY"
APPLE_SIGNING_IDENTITY="$IDENTITY" npm run tauri build -- --bundles app

APP="src-tauri/target/release/bundle/macos/发票报销.app"
echo
echo "构建完成：$APP"
codesign -d -r- "$APP" 2>&1 | grep designated || true
echo
echo "要求里出现 certificate root 才是稳定签名；出现 cdhash 说明身份没生效。"
