# HTTPS 发票链接自动下载设计

## 1. 目标

当邮件正文包含发票下载链接时，桌面 APP 在同步期间自动取得真实原件，并进入已有的识别、去重、自动审核、批次归属和导出流程。该功能要能替换 `v0.2.0` 已产生的 `.url` 占位项，不要求用户逐张人工打开。

## 2. 范围

- 支持 HTTPS 直链返回的 PDF、JPEG、PNG 和 ZIP。
- ZIP 复用已有的安全展开逻辑，原 ZIP 与可识别内部文件保留独立来源 part ID。
- 支持当前真实邮箱中 `nnfp.jss.com.cn` 诺诺短链接：跟随服务端重定向，从 `printQrcode` 参数调用票据详情接口，再下载返回的 PDF URL。
- XML 下载链接、无票据参数的 `fp.nuonuo.com/#/` 平台首页和其他非文件网页不作为可报销原件。
- 不引入云端 AI、外部 Agent 或需要用户登录的新服务。

## 3. 方案选择

### 3.1 采用方案

在 Rust 后端新增独立 `InvoiceLinkDownloader` 边界。生产实现使用受限 HTTP 客户端，同步服务只处理三种结果：下载到发票文件、确定为可忽略链接、可隔离的下载失败。测试通过 trait 注入确定性假实现。

### 3.2 未采用方案

- 在前端 WebView 中隐藏打开所有链接：难以稳定拦截下载，并扩大了第三方脚本权限。
- 直接依赖系统浏览器或 Playwright：会引入大型运行时，不适合本地 macOS 安装包。
- 对任意 HTML 执行 JavaScript：安全边界不可控，且无法保证供应商页面稳定。

## 4. 组件与数据流

1. `sync` 从 HTML 邮件正文中继续收集可能的 HTTPS 发票链接。
2. 处理前查询邮箱 part 幂等键。已是真实文件时不重复联网；旧 `.url` 占位项则允许重试。
3. `SecureInvoiceLinkDownloader` 完成安全请求、受限重定向、诺诺短链接解析和文件签名分类。
4. 直接文件以原 link part ID 导入；ZIP 内部文件使用 `<link-part>.zip.<index>`。
5. `ImportService` 对旧 `.url` 占位项使用同一 item ID 原位替换，保留邮箱来源字段，并将识别状态重置为 pending。
6. 同步服务立即调用现有 `RecognitionService`，后续批次自动化不需要新分支。

## 5. 安全与资源限制

- 仅允许 `https` URL，最多 5 次重定向。
- DNS 解析后拒绝 loopback、私网、link-local、multicast、documentation 和其他保留 IP；客户端使用已验证地址进行连接，减少 DNS rebinding 窗口。
- 连接超时 10 秒，整体请求超时 30 秒，单文件最大 50 MiB，响应头和实际流式字节都校验上限。
- 不信任 URL 后缀或 `Content-Type`；PDF、JPEG、PNG、ZIP 必须通过实际 magic bytes 验证。HTML、XML 和未知类型不会进入 OCR。
- 错误信息不写入完整签名 URL 或查询参数；仅失败 `.url` 原件本身保留链接，以便用户手工处理。

## 6. 错误与重试

- 单个链接的网络、TLS、超时、大小或格式失败不中断同一邮箱的其他票据。
- 失败项保存为 `recognition_failed` 的 `.url` 原件，错误说明只包含安全摘要。
- 用户重新运行同一月份批次时，范围扫描会重试该链接；成功后原位替换失败占位项。
- 确定为 XML 或平台首页的链接不记为失败；若存在旧占位项，在重新扫描时安全删除。

## 7. 验证标准

- 下载器的 URL/IP 安全策略、文件签名分类、大小限制和诺诺响应解析有单元测试。
- 同步集成测试覆盖 PDF 下载识别、ZIP 展开、旧占位替换、失败隔离、重试恢复、忽略 XML/首页和幂等重扫。
- 前端无新交互分支；现有原件预览、异常列表和批次摘要通过回归测试。
- 完整运行 Vitest、ESLint、TypeScript/Vite build、Rust fmt/clippy/test 和三尺寸 Playwright。
- 构建 Apple Silicon DMG，验证应用、OCR sidecar、签名、挂载和 SHA-256。
