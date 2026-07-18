# macOS 发布检查表

每个候选版本都从干净 checkout 和干净 macOS 用户环境执行。记录版本、commit SHA、macOS/CPU、测试邮箱、时间范围和每项证据。真实 QQ/Gmail 凭据不得写入仓库、CI 日志、截图文件名或测试数据。

## 首发应用身份

2026-07-18 确认执行首发前 identity correction：产品此前从未公开发布且为零既有用户，正式首发 bundle identifier 为 `com.invoice-desk.desktop`，取代开发初始化阶段的 `com.invoice-desk.app`。本次不执行 bundle identity 数据迁移；正式数据根从第一次公开发布起固定为 `~/Library/Application Support/com.invoice-desk.desktop/`，钥匙串 service 保持 `com.invoice-desk.credentials`。

## 自动化质量门

- [ ] `npm ci`
- [ ] `uv sync --project sidecars/ocr --frozen`
- [ ] `uv run --project sidecars/ocr pytest sidecars/ocr/test_main.py -q`
- [ ] `bash scripts/build-ocr-sidecar.sh`
- [ ] `npm run lint`
- [ ] `npm test`
- [ ] `npm run build`
- [ ] `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`
- [ ] `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings`
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml --all-features`
- [ ] `npm run test:e2e` 在 800×700、1440×900、1728×1117 全部通过
- [ ] `npm run tauri build -- --bundles app,dmg`
- [ ] `.app`、`.dmg` 和内置 `invoice-ocr` 都是 arm64：`file <path>`
- [ ] `otool -l <app-binary>` 的 `LC_BUILD_VERSION minos` 为 `11.0`
- [ ] 生产 `dist/` 不含 `VITE_BROWSER_COMMAND_BRIDGE`、`__INVOICE_COMMAND_BRIDGE__` 或 `Simulated browser command failure`

## 安装、升级和数据生命周期

- [ ] 在干净用户下挂载 DMG，拖拽 `.app` 到临时 Applications 目录并首次启动
- [ ] 首启没有旧数据、测试 bridge、开发服务器地址或浏览器 console error
- [ ] 覆盖安装上一稳定版后启动，migration 完成，原票据、批次、邮箱元数据仍在
- [ ] 升级过程中强制中断一次，仅用专门测试副本验证下次启动可恢复
- [ ] 卸载 `.app` 不误删 `~/Library/Application Support/com.invoice-desk.desktop/`、外部导出目录或钥匙串；单独记录清理数据的人工步骤
- [ ] 安装包无签名/公证凭据时只标记“本地未签名 smoke”；不得标记签名或公证通过

## QQ 与 Gmail 实机协议

- [ ] QQ 邮箱首次授权、连接测试和首轮抓取成功
- [ ] Gmail 首次授权、连接测试和首轮抓取成功
- [ ] 两个服务商各自增量抓取只取得新增邮件，UID/UIDVALIDITY 游标正确推进
- [ ] 断网后错误可见；恢复网络后手动重试和下一周期自动恢复
- [ ] 撤销/替换授权码后显示授权失效；重新授权后恢复，不丢同步游标
- [ ] 后台运行 2 小时，窗口隐藏时按配置周期同步，CPU/内存无持续异常增长
- [ ] 同一邮件重复出现或重试抓取不产生第二张有效票据
- [ ] 同一文件先邮件后手动上传、先手动后邮件上传都进入疑似重复处理，不静默双计

QQ/Gmail 检查必须由持有真实测试账号凭据的人执行。没有凭据时保持这些条目未勾选，并在发布记录中列为人工待办。

## 票据与人工复核

- [ ] PDF 文本票据和扫描/图片票据均可导入、预览、识别或明确失败
- [ ] 交通、餐饮、住宿、招待四类各至少 1 张，分类与金额可人工更正
- [ ] 待确认、识别失败、疑似重复三种队列均显示正确状态、可重试/处置
- [ ] 手动上传与邮件抓取来源显示正确
- [ ] 单月批次按自然月推荐票据
- [ ] 跨月自定义批次按起止日期推荐，范围外票据有明确提示
- [ ] 未确认票据阻止导出，确认后立即解除阻止

## 导出包核验

- [ ] 导出反馈明确列出 `merged.pdf`、`reimbursement.xlsx`、`originals.zip`、`manifest.json`
- [ ] “在文件夹中显示”定位到实际导出目录
- [ ] 用 PDF 阅读器打开 `merged.pdf`，页数/顺序与批次票据一致
- [ ] 用 Excel/Numbers 打开 `reimbursement.xlsx`，列、金额、分类和合计正确
- [ ] 解压 `originals.zip`，文件数、原文件名映射和内容正确
- [ ] 解析 `manifest.json`，item 数、总金额、每项 ID/文件名/SHA-256 与其他三个文件交叉一致
- [ ] 独立执行 `shasum -a 256` 核对 manifest 记录，不只检查字段存在
- [ ] 导出中断后重启：已提交包保留，未提交包回滚，`.part` 不被当作正式文件
- [ ] 外接导出盘离线时不改写默认目录；重新挂载后可从设置页重试恢复

## 桌面生命周期与视觉

- [ ] 点窗口关闭按钮后窗口隐藏，托盘仍可“显示发票报销”
- [ ] 托盘“立即同步全部”可触发同步且状态反馈正确
- [ ] 托盘“退出”在 10 秒内完成；下次启动没有遗留运行中同步/导出状态
- [ ] 800×700、1440×900、1728×1117 检查控制台、待处理池、票据详情、批次详情、设置页
- [ ] loading、empty、error/retry、disabled、drawer/dialog 状态均无 page-level 水平滚动、重叠、不可达关闭按钮或按钮文字换行
- [ ] 键盘 Tab 焦点可见；drawer/dialog 焦点不逃逸；Escape 与关闭后焦点返回合理
- [ ] `prefers-reduced-motion: reduce` 下没有持续或装饰性动画
- [ ] 与 `docs/design/references/dark-three-pane-workbench.png` 对照：稠密石墨层级、紫/橙强调、圆角和 Lucide 图标一致，无营销页式大标题或卡片套卡片

## 发布结论

- [ ] 所有自动 gate 通过且 CI bundle job 由 `needs: gates` 阻止失败发布
- [ ] 所有人工项有执行人、日期和证据链接
- [ ] 未执行、失败或受凭据/签名限制的项目明确列为 blocker 或已批准例外
- [ ] release notes 明确包是否已签名、公证；本地未签名 smoke 不冒充正式分发验收
