# 本机代码签名（稳定签名身份）

## 为什么需要

App 默认用 ad-hoc 签名（`codesign --sign -`），**每次重建 cdhash 都会变**。钥匙串条目的受信任列表记录的是签名要求：

- ad-hoc 签名的要求是 `cdhash H"…"` → 重建后不再匹配 → macOS 对每次凭据读取都弹授权框 → App 的 20 秒读取超时 → 账号被标记为暂停自动同步（`读取邮箱凭据超时，请解锁 Mac 后重试`）。
- 用固定证书签名后，要求变成 `identifier "com.invoice-desk.desktop" and certificate root = H"…"` → **重建后完全一致** → 授权一次，长期有效。

2026-09-19 起本机使用自签名身份 **`Invoice Desk Local Signing`**（有效期至 2036-09-16）。

## 已做的本机设置（一次性）

```bash
# 1. 生成自签名代码签名证书（本机临时目录内完成，密钥材料用完即删）
#    keyUsage=digitalSignature, extendedKeyUsage=codeSigning
openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
  -config openssl.cnf -keyout sign.key -out sign.crt
openssl pkcs12 -export -inkey sign.key -in sign.crt \
  -name "Invoice Desk Local Signing" -out sign.p12 -passout pass:<临时口令>

# 2. 导入用户登录钥匙串
security import sign.p12 -k ~/Library/Keychains/login.keychain-db \
  -P <临时口令> -A -T /usr/bin/codesign -T /usr/bin/security

# 3. 让 codesign 无需弹窗即可使用私钥（等价于 CI 上的标准做法）
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
  -k <登录密码> ~/Library/Keychains/login.keychain-db
```

> 注意：仅执行第 2 步不够。这台 macOS 上 `security import … -A` 被接受但**不生效**，`security add-generic-password … -A` 同样无效；必须执行第 3 步的 partition list，否则 `codesign` 会卡在授权弹窗（`SIGN HUNG`）。
>
> 另外：给证书添加系统信任（`security add-trusted-cert -p codeSign`）需要图形授权，本机未执行。`security find-identity -v -p codesigning` 因此显示 0 个有效身份，但 `codesign --sign "Invoice Desk Local Signing"` 仍可正常签名，且签名要求稳定——这正是钥匙串需要的东西。代价：Gatekeeper 对外分发时仍视为未签名（与 ad-hoc 相比没有变差）。

## 构建

```bash
APPLE_SIGNING_IDENTITY="Invoice Desk Local Signing" npm run tauri build -- --bundles app
```

或用脚本：`bash scripts/build-signed-app.sh`

CI 保持原有的 unsigned/ad-hoc 构建（`bundle.macOS.signingIdentity` 不写进 `tauri.conf.json`，否则 CI 找不到该身份会失败）。

## 安装后的检查

```bash
codesign -d -r- "/Applications/发票报销.app" 2>&1 | grep designated
# 期望：designated => identifier "com.invoice-desk.desktop" and certificate root = H"74a1b0c6…"
```

要求里出现 `certificate root` 即表示稳定签名生效；若仍是 `cdhash H"…"`，说明构建时没有传 `APPLE_SIGNING_IDENTITY`。

## 如果钥匙串仍然弹窗

App 读不到凭据时（条目由别的程序写入、或换了签名身份），唯一可靠的修复是**让 App 重新创建条目**：

1. `bash scripts/reset-keychain-credential.sh`（保留授权码到剪贴板并删除旧条目）
2. 在 App 的「运行设置 → 邮箱账号」里粘贴授权码 → 「测试连接」→「保存账号」
3. 回到 App 点一次「立即同步」清除暂停状态

命令行改 ACL 无法修复：`-T` 写入的授权位不覆盖读取密钥，`-A` 无效（见上文）。
