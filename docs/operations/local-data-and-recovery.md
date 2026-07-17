# 本地数据、备份与恢复

本文适用于 macOS 版本 `com.invoice-desk.app`。操作前先确认 Finder 中显示的是预期用户的主目录；不要在应用运行时移动、替换或编辑数据文件。

## 数据位置

默认应用数据目录：

```text
~/Library/Application Support/com.invoice-desk.app/
├── invoice.sqlite3
└── storage/
    ├── originals/
    ├── normalized/
    ├── exports/
    └── staging/
```

- `invoice.sqlite3`：票据、邮箱元数据、同步游标、批次、偏好、`pending_account_saves`、`pending_account_save_cleanups` 和 `pending_exports` 恢复日志。
- `storage/originals/YYYY/MM/`：导入后保留的原始附件或手动文件。
- `storage/normalized/`：预览和合并所需的规范化 PDF。
- `storage/exports/`：未改设置时的报销包输出目录。
- `storage/staging/`：原件写入和默认导出的临时区；未完成原件使用 `<uuid>.part`。
- 自定义导出目录：设置页“当前有效导出目录”显示真实路径，其临时区是该目录下 `.invoice-reimbursement-staging/`。
- 导出包固定包含 `merged.pdf`、`reimbursement.xlsx`、`originals.zip`、`manifest.json`。

导出事务同时使用 SQLite 的 `pending_exports` 和临时/最终目录里的 `.invoice-export-recovery.json` marker。两者必须和数据库一起保留，不能把 marker 当作无用隐藏文件删除。邮箱密码或授权码保存在 macOS 钥匙串 service `com.invoice-desk.credentials`，不在上述目录，也不会进入文件备份包。

## 备份顺序

1. 在菜单栏托盘选择“退出”，不要只点窗口关闭按钮；关闭窗口只会隐藏应用。
2. 最多等待 10 秒让同步、导出和数据库连接正常收口，确认“活动监视器”中没有“发票报销”进程。
3. 复制整个 `~/Library/Application Support/com.invoice-desk.app/`，不要只复制 SQLite 或 `originals/`。
4. 若设置页的导出目录在数据目录之外，单独复制整个自定义导出目录，包含隐藏的 `.invoice-reimbursement-staging/` 和 `.invoice-export-recovery.json`。
5. 对备份计算校验值并记录应用版本：`shasum -a 256 <备份归档>`。
6. 如需迁移邮箱，另行在受控流程中重新取得 QQ/Gmail 授权码；普通文件备份不包含钥匙串 secret。

不要在运行中的 SQLite 上直接复制单个 `invoice.sqlite3`。即使当前没有 `-wal` 文件，退出后整目录备份仍是唯一受支持的顺序。

## 恢复顺序

1. 安装相同版本或更新版本的应用，但先不要配置邮箱或导入新票据。
2. 退出应用并确认进程结束。
3. 将现有数据目录改名留存，例如 `com.invoice-desk.app.before-restore`，不要直接覆盖后删除唯一副本。
4. 把备份恢复到原路径，保持目录所有者为当前用户；自定义导出目录也恢复到数据库记录的绝对路径。
5. 启动应用。应用会先执行 SQLite migration，再恢复未完成的邮箱保存与导出事务。
6. 在设置页确认本地数据目录、当前有效导出目录、可用空间与“导出恢复失败”状态。
7. 抽查票据预览、批次金额和最近导出。钥匙串未随备份恢复时，逐个邮箱重新输入授权码并执行连接测试。

升级只允许应用执行顺序 migration；不要手工改 `user_version`、删 migration 表或用旧版本打开已升级数据库。降级不受支持。跨机器恢复前保留原始备份，完成抽查后再清理 `before-restore` 目录。

## 异常关机与导出恢复

- 原件 `<uuid>.part` 位于 `storage/staging/`。成功 promotion 后才会进入 `originals/YYYY/MM/`；异常中断时不要手工改名为正式原件。
- 单文件导出使用 `.<文件名>.part`，完整同步后再原子替换正式文件；孤立 `.part` 不是有效报销包。
- 整包先在 `export-<operation-id>` 临时目录生成，再发布到最终批次目录。启动恢复会依据 `pending_exports` 和 marker 判定：已确认提交的最终目录保留并清除 marker；未提交目录回滚；无法判定的状态保留并在设置页显示错误，等待重试或人工介入。
- 不要只删除 `pending_exports` 行、marker、临时目录中的任意一个来“解锁”。这会破坏提交结果判定。

## 外接盘不可用

自定义导出目录可能位于外接盘。盘未挂载、卷名变化或只读时，应用保留恢复状态并报告导出目录不可用，不会退回默认目录继续写入。恢复步骤：

1. 退出应用，重新挂载同一卷并确认原绝对路径存在且可写。
2. 确认目标是普通目录，不是 symlink、alias 或指向网络位置的替身。
3. 重新启动，在设置页选择“重试导出恢复”。
4. 原卷无法恢复时，先完整备份数据库和旧卷内容，再选择一个已创建、可写的普通绝对目录作为之后的导出目录；旧根上的 pending 恢复仍需保留并单独处理。

应用会 canonicalize 自定义导出根，拒绝相对路径、symlink 根和非普通目录。读取票据时逐层使用 no-follow 语义，拒绝 `..`、越过 `originals/normalized` 根的路径、symlink 与非普通文件。不要通过 symlink 搬迁数据目录或导出目录。

## 故障排查

| 现象 | 检查 | 处理 |
|---|---|---|
| 首启为空 | 当前 macOS 用户、bundle identifier、数据目录路径 | 退出后恢复到当前用户的 `Application Support/com.invoice-desk.app/` |
| 票据存在但预览失败 | `originals/`、`normalized/` 所有者和普通文件属性 | 从同一备份恢复缺失文件，不创建 symlink |
| 设置页显示导出恢复失败 | 外接盘挂载、可写空间、marker 与数据库是否成套 | 恢复原路径后点“重试导出恢复”；保留失败现场 |
| 邮箱提示授权失败 | 钥匙串 service、授权码是否失效 | 重新输入服务商授权码；不要把密码写入备份目录 |
| 发现 `.part` 或 `export-*` | 是否刚经历崩溃、`pending_exports` 是否仍有记录 | 先备份并重启让自动恢复处理，不手工重命名/删除 |
| 可用空间未知 | 导出卷未挂载或元数据读取失败 | 恢复卷连接和权限，再刷新设置页 |

自动恢复持续失败时，保留应用数据目录、自定义导出目录和应用版本信息的只读副本，再进行人工诊断；日志或截图中不得包含邮箱授权码。
