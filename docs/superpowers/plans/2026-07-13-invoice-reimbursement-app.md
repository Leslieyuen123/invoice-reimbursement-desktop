# 发票报销桌面 App MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付一款可安装、可后台同步 QQ 邮箱和 Gmail、可手动导入票据、可人工校正并按报销批次导出完整报销包的本地优先桌面 App。

**Architecture:** 使用 Tauri 2 提供跨平台桌面壳与后台生命周期，React/TypeScript 提供操作界面，Rust 应用层负责 IMAP、识别、分类、批次和导出。SQLite 保存业务状态，原始文件和标准化文件保存在本地数据目录，邮箱密码只进入系统钥匙串；所有外部能力都通过 trait 隔离，以便单元测试使用内存替身。

**Tech Stack:** Tauri 2、Rust 2024、React 19、TypeScript、Vite、SQLite/sqlx、keyring、imap/mail-parser、RapidOCR/ONNX + pypdfium2 本地 sidecar、lopdf、rust_xlsxwriter、zip、Vitest/Testing Library、pytest、Playwright。

---

## 0. 实施边界与架构决策

当前目录只有设计文档，不是 Git 仓库，也没有既有代码或技术栈。本计划因此按绿地项目编写；Task 1 会初始化仓库。执行时不要提前加入 Web 服务、用户登录、多人协作、企业审批、云同步或更多费用分类。

MVP 的关键取舍如下：

- 桌面端是唯一正式入口；前端不可直接访问 SQLite、钥匙串或原始文件。
- Rust 端暴露窄 Tauri command，业务规则保留在 service/repository 层。
- 金额一律使用整数分 `amount_cents: Option<i64>`，币种 MVP 固定为 `CNY`。
- 日期一律存 ISO 8601 字符串；票据建议归属用 `YYYY-MM`，批次范围用 `YYYY-MM-DD`。
- 正式分类只有 `transport | dining | accommodation | hospitality`，不能可靠判断时 `final_category = null` 且进入待确认。
- 数据库分别保存识别、确认、去重三个正交状态；界面通过固定优先级派生 `pending_recognition | pending_confirmation | recognition_failed | suspected_duplicate | ready`，避免疑似重复覆盖识别结果。
- 邮件增量游标以“账号 + mailbox + UIDVALIDITY + last UID”保存；UIDVALIDITY 变化时重新扫描并依靠内容哈希去重。
- 原件去重键为 SHA-256；邮件来源幂等键为 `account_id + mailbox + uid + part_id`。疑似重复保留记录和原件，不静默删除。
- 第一版识别 PDF 文本与 JPG/PNG OCR；其他格式、压缩包和损坏文件保留原件并进入待确认或识别失败。
- 批次导出前必须消除待确认项；每次导出生成 `merged.pdf`、`reimbursement.xlsx`、`originals.zip` 和 `manifest.json`。

## 1. 目标文件结构

```text
发票报销/
├── .github/workflows/ci.yml                 # 前后端测试与构建检查
├── docs/superpowers/specs/...               # 已确认产品设计
├── docs/superpowers/plans/...               # 本实施计划
├── package.json                             # 前端、Tauri、质量命令
├── vite.config.ts
├── src/
│   ├── app/App.tsx                          # 路由和全局 QueryClient
│   ├── app/AppShell.tsx                     # 桌面导航和全局状态区
│   ├── components/                          # 通用表格、空态、对话框
│   ├── features/dashboard/                  # 首页控制台
│   ├── features/inbox/                      # 待处理池和票据详情
│   ├── features/batches/                    # 批次列表、编辑、导出
│   ├── features/settings/                   # 邮箱、同步、存储、导出设置
│   ├── lib/api.ts                           # 唯一 Tauri command 调用层
│   ├── lib/queryKeys.ts
│   ├── test/                                # 前端测试环境和 command mock
│   └── types.ts                             # 与 Rust DTO 对齐的前端类型
├── tests/e2e/                               # 浏览器壳关键流程测试
└── src-tauri/
    ├── capabilities/default.json            # 最小 Tauri 权限
    ├── migrations/0001_init.sql             # 完整初始数据库结构
    ├── binaries/                            # 构建生成的按平台 OCR sidecar
    ├── src/
    │   ├── lib.rs                           # Tauri builder 和 command 注册
    │   ├── state.rs                         # AppState 与依赖装配
    │   ├── domain/model.rs                  # 稳定领域枚举和实体
    │   ├── domain/error.rs                  # 可序列化应用错误
    │   ├── db/mod.rs                        # 连接池、迁移、事务入口
    │   ├── db/items.rs                      # 票据持久化
    │   ├── db/batches.rs                    # 批次持久化
    │   ├── db/accounts.rs                   # 邮箱与同步游标持久化
    │   ├── services/import.rs               # 文件落盘、哈希和导入
    │   ├── services/recognition.rs          # 文本提取、OCR 和字段识别
    │   ├── services/sync.rs                 # IMAP 增量同步编排
    │   ├── services/batches.rs              # 批次规则和票据归属
    │   ├── services/export.rs               # 报销包生成
    │   ├── services/scheduler.rs            # 后台定时同步和重试
    │   ├── infra/files.rs                   # 本地目录和原子写入
    │   ├── infra/credentials.rs             # 系统钥匙串
    │   ├── infra/imap.rs                    # QQ/Gmail IMAP 适配器
    │   ├── infra/extraction.rs              # PDF 文本、图片 OCR
    │   ├── infra/exporters.rs               # PDF/XLSX/ZIP 实现
    │   └── commands/                        # dashboard/items/batches/settings/sync/export
    └── tests/                               # Rust 跨模块集成测试
├── sidecars/ocr/
│   ├── pyproject.toml                       # 锁定 RapidOCR/ONNX 运行环境
│   ├── main.py                              # stdin/stdout JSON OCR 进程
│   └── test_main.py                         # 中文票据 OCR 契约测试
└── scripts/build-ocr-sidecar.sh             # 生成 Tauri externalBin
```

## 2. 稳定数据契约

先固定这些值，后续任务不得自行改名：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    PendingRecognition,
    PendingConfirmation,
    RecognitionFailed,
    SuspectedDuplicate,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Category { Transport, Dining, Accommodation, Hospitality }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SourceType { Email, ManualUpload }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus { Draft, Exported }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RecognitionStatus { Pending, Succeeded, Failed }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationStatus { Pending, Confirmed }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DedupeStatus { Unique, SuspectedDuplicate, Resolved }

pub fn derive_item_status(
    recognition: RecognitionStatus,
    confirmation: ConfirmationStatus,
    dedupe: DedupeStatus,
) -> ItemStatus {
    if dedupe == DedupeStatus::SuspectedDuplicate {
        ItemStatus::SuspectedDuplicate
    } else if recognition == RecognitionStatus::Failed {
        ItemStatus::RecognitionFailed
    } else if recognition == RecognitionStatus::Pending {
        ItemStatus::PendingRecognition
    } else if confirmation == ConfirmationStatus::Pending {
        ItemStatus::PendingConfirmation
    } else {
        ItemStatus::Ready
    }
}
```

前后端共享 DTO 使用 camelCase JSON：

```ts
export type ItemStatus =
  | "pending_recognition"
  | "pending_confirmation"
  | "recognition_failed"
  | "suspected_duplicate"
  | "ready";
export type Category = "transport" | "dining" | "accommodation" | "hospitality";
export type SourceType = "email" | "manual_upload";
export type BatchStatus = "draft" | "exported";
export type RecognitionStatus = "pending" | "succeeded" | "failed";
export type ConfirmationStatus = "pending" | "confirmed";
export type DedupeStatus = "unique" | "suspected_duplicate" | "resolved";

export interface InvoiceItemDto {
  id: string;
  originalName: string;
  previewUrl: string;
  sourceType: SourceType;
  sourceAccountId: string | null;
  fetchedAt: string;
  invoiceDate: string | null;
  suggestedPeriod: string | null;
  batchId: string | null;
  suggestedCategory: Category | null;
  finalCategory: Category | null;
  amountCents: number | null;
  currency: "CNY";
  city: string | null;
  company: string | null;
  status: ItemStatus;
  recognitionStatus: RecognitionStatus;
  confirmationStatus: ConfirmationStatus;
  dedupeStatus: DedupeStatus;
  note: string | null;
  eventTag: string | null;
  projectTag: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface BatchDto {
  id: string;
  name: string;
  startDate: string;
  endDate: string;
  status: BatchStatus;
  itemCount: number;
  totalAmountCents: number;
  unconfirmedCount: number;
  note: string | null;
  createdAt: string;
  updatedAt: string;
  lastExportedAt: string | null;
}

export interface MailboxAccountDto {
  id: string;
  provider: "gmail" | "qq";
  email: string;
  imapHost: string;
  imapPort: number;
  enabled: boolean;
  syncIntervalMinutes: number;
  lastSyncedAt: string | null;
  lastError: string | null;
}

export interface DashboardDto {
  mailboxAccounts: MailboxAccountDto[];
  recentlyAddedCount: number;
  pendingConfirmationCount: number;
  recognitionFailedCount: number;
  suspectedDuplicateCount: number;
  recentBatches: BatchDto[];
}

export interface ItemFilter {
  status?: ItemStatus;
  suggestedPeriod?: string;
  category?: Category;
  sourceType?: SourceType;
  batchId?: string;
  query?: string;
}

export interface PreferencesDto {
  backgroundSyncEnabled: boolean;
  exportDirectory: string;
  batchDirectoryPattern: "{batchName}-{timestamp}";
}
```

---

### Task 1: 初始化可测试的 Tauri 工作区

**Files:**
- Create: `package.json`
- Create: `vite.config.ts`
- Create: `src/main.tsx`
- Create: `src/app/App.tsx`
- Create: `src/test/setup.ts`
- Create: `src/app/App.test.tsx`
- Create: `src-tauri/Cargo.toml`
- Create: `src-tauri/tauri.conf.json`
- Create: `src-tauri/src/main.rs`
- Create: `src-tauri/src/lib.rs`
- Create: `.gitignore`

- [ ] **Step 1: 初始化 Git 并创建前端清单**

Run: `git init && npm init -y`

将 `package.json` 的 scripts 固定为：

```json
{
  "name": "invoice-reimbursement-desktop",
  "private": true,
  "version": "0.1.0",
  "type": "module",
  "scripts": {
    "dev": "vite",
    "build": "tsc -b && vite build",
    "test": "vitest run",
    "test:watch": "vitest",
    "test:e2e": "playwright test",
    "lint": "eslint . --max-warnings=0",
    "tauri": "tauri"
  }
}
```

Run:

```bash
npm install react react-dom react-router-dom @tanstack/react-query @tauri-apps/api @tauri-apps/plugin-dialog @tauri-apps/plugin-opener lucide-react
npm install -D typescript vite @vitejs/plugin-react vitest jsdom @testing-library/react @testing-library/jest-dom @testing-library/user-event eslint @eslint/js typescript-eslint @tauri-apps/cli @playwright/test
```

Expected: 生成 `package-lock.json`，`npm audit` 不出现 critical 漏洞。

- [ ] **Step 2: 写前端失败冒烟测试**

```tsx
// src/app/App.test.tsx
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { App } from "./App";

describe("App", () => {
  it("renders the desktop application title", () => {
    render(<App />);
    expect(screen.getByRole("heading", { name: "发票报销" })).toBeInTheDocument();
  });
});
```

Run: `npm test -- src/app/App.test.tsx`

Expected: FAIL，提示无法找到 `./App` 或标题不存在。

- [ ] **Step 3: 添加最小前端和 Vitest 配置**

```tsx
// src/app/App.tsx
export function App() {
  return <h1>发票报销</h1>;
}
```

```ts
// vite.config.ts
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  test: { environment: "jsdom", setupFiles: ["./src/test/setup.ts"] },
});
```

```ts
// src/test/setup.ts
import "@testing-library/jest-dom/vitest";
```

Run: `npm test -- src/app/App.test.tsx && npm run build`

Expected: 两条命令均通过。

- [ ] **Step 4: 初始化 Tauri 2 并添加 Rust 冒烟测试**

Run: `npm run tauri init -- --ci --app-name "发票报销" --window-title "发票报销" --frontend-dist ../dist --dev-url http://localhost:1420 --before-dev-command "npm run dev -- --port 1420" --before-build-command "npm run build"`

把生成的 `src-tauri/Cargo.toml` 包名固定为 `invoice-reimbursement`，把 `tauri.conf.json` 的 identifier 固定为 `com.invoice-desk.app`；后续数据库目录、钥匙串 service 和升级路径都依赖这两个值，不能改名。

随后在 Rust crate 中加入并锁定 MVP 依赖：

```bash
cd src-tauri
cargo add tauri@2 --features tray-icon
cargo add tauri-plugin-dialog@2 tauri-plugin-opener@2 tauri-plugin-shell@2
cargo add tokio@1 --features macros,rt-multi-thread,fs,time,io-util,sync
cargo add serde@1 --features derive
cargo add serde_json@1 thiserror@2 async-trait@0.1
cargo add chrono@0.4 --features serde
cargo add uuid@1 --features v4,serde
cargo add sqlx@0.8 --features runtime-tokio-rustls,sqlite,migrate,chrono,uuid
cargo add keyring@3 sha2@0.10 infer@0.19 rust_decimal@1
cargo add imap@3.0.0-alpha.15 native-tls@0.2 mail-parser@0.11
cargo add pdf-extract@0.9 printpdf@0.8 lopdf@0.36 image@0.25
cargo add rust_xlsxwriter@0.90 zip@4 sanitize-filename@0.6
cargo add tracing@0.1 tracing-subscriber@0.3 dashmap@6
cargo add --dev tempfile@3 pretty_assertions@1
cd ..
```

Expected: 生成 `src-tauri/Cargo.lock`；如任一主版本的 API 已发生不兼容，只能在本 Task 内升级并重新生成锁文件，后续 Task 不得漂移依赖。

在 `src-tauri/src/lib.rs` 中加入：

```rust
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("failed to run invoice reimbursement app");
}

#[cfg(test)]
mod tests {
    #[test]
    fn application_identity_is_stable() {
        assert_eq!(env!("CARGO_PKG_NAME"), "invoice-reimbursement");
    }
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml`

Expected: PASS。

- [ ] **Step 5: 提交工作区骨架**

```bash
git add .gitignore package.json package-lock.json vite.config.ts src src-tauri
git commit -m "chore: bootstrap invoice reimbursement desktop app"
```

---

### Task 2: 固定领域模型和可序列化错误

**Files:**
- Create: `src-tauri/src/domain/mod.rs`
- Create: `src-tauri/src/domain/model.rs`
- Create: `src-tauri/src/domain/error.rs`
- Modify: `src-tauri/src/lib.rs`
- Create: `src/types.ts`
- Test: `src-tauri/src/domain/model.rs`

- [ ] **Step 1: 写枚举序列化和批次日期失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_contract_values_as_snake_case() {
        assert_eq!(serde_json::to_string(&Category::Transport).unwrap(), "\"transport\"");
        assert_eq!(serde_json::to_string(&ItemStatus::PendingConfirmation).unwrap(), "\"pending_confirmation\"");
    }

    #[test]
    fn rejects_reversed_batch_range() {
        let result = NewBatch::try_new("6-7 月报销", "2026-07-31", "2026-06-01", None);
        assert!(matches!(result, Err(AppError::Validation { field, .. }) if field == "dateRange"));
    }

    #[test]
    fn derives_work_queue_without_losing_orthogonal_state() {
        assert_eq!(
            derive_item_status(
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Confirmed,
                DedupeStatus::SuspectedDuplicate,
            ),
            ItemStatus::SuspectedDuplicate,
        );
    }
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml domain::model::tests`

Expected: FAIL，类型尚未定义。

- [ ] **Step 2: 实现稳定领域类型**

在 `model.rs` 定义第 2 节四个枚举，并补充：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBatch {
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub note: Option<String>,
}

impl NewBatch {
    pub fn try_new(name: &str, start: &str, end: &str, note: Option<String>) -> Result<Self, AppError> {
        let start_date = NaiveDate::parse_from_str(start, "%Y-%m-%d")
            .map_err(|_| AppError::validation("startDate", "必须是 YYYY-MM-DD"))?;
        let end_date = NaiveDate::parse_from_str(end, "%Y-%m-%d")
            .map_err(|_| AppError::validation("endDate", "必须是 YYYY-MM-DD"))?;
        if name.trim().is_empty() {
            return Err(AppError::validation("name", "批次名称不能为空"));
        }
        if start_date > end_date {
            return Err(AppError::validation("dateRange", "开始日期不能晚于结束日期"));
        }
        Ok(Self { name: name.trim().into(), start_date, end_date, note })
    }
}
```

在 `error.rs` 定义：

```rust
#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AppError {
    #[error("{message}")]
    Validation { field: String, message: String },
    #[error("{message}")]
    NotFound { entity: String, message: String },
    #[error("{message}")]
    Conflict { message: String },
    #[error("{message}")]
    External { service: String, retryable: bool, message: String },
    #[error("{message}")]
    Internal { message: String },
}

impl AppError {
    pub fn validation(field: &str, message: &str) -> Self {
        Self::Validation { field: field.into(), message: message.into() }
    }
}
```

- [ ] **Step 3: 添加完全对应的 TypeScript DTO**

将第 2 节 TypeScript 代码块原样写入 `src/types.ts`。Rust DTO 使用 `#[serde(rename_all = "camelCase")]` 并逐字段对齐；金额字段只用 `amountCents`，时间字段只用 `...At` 或 `...Date` 后缀。

Run: `cargo test --manifest-path src-tauri/Cargo.toml && npm run build`

Expected: PASS，TypeScript 无隐式 `any`。

- [ ] **Step 4: 提交领域契约**

```bash
git add src-tauri/src/domain src-tauri/src/lib.rs src/types.ts src-tauri/Cargo.toml
git commit -m "feat: define invoice reimbursement domain contracts"
```

---

### Task 3: 建立 SQLite 模型、迁移和仓储

**Files:**
- Create: `src-tauri/migrations/0001_init.sql`
- Create: `src-tauri/src/db/mod.rs`
- Create: `src-tauri/src/db/items.rs`
- Create: `src-tauri/src/db/batches.rs`
- Create: `src-tauri/src/db/accounts.rs`
- Create: `src-tauri/tests/database.rs`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: 写失败的迁移集成测试**

```rust
#[tokio::test]
async fn migration_creates_all_mvp_tables() {
    let pool = invoice_reimbursement::db::connect("sqlite::memory:").await.unwrap();
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    ).fetch_all(&pool).await.unwrap();
    for required in ["batches", "items", "mailbox_accounts", "sync_cursors", "sync_runs", "settings"] {
        assert!(names.contains(&required.to_string()), "missing table {required}");
    }
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test database`

Expected: FAIL，`db::connect` 不存在。

- [ ] **Step 2: 写初始迁移**

`0001_init.sql` 必须包含以下约束，而不只是列名：

```sql
PRAGMA foreign_keys = ON;

CREATE TABLE batches (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL CHECK(length(trim(name)) > 0),
  start_date TEXT NOT NULL,
  end_date TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('draft','exported')) DEFAULT 'draft',
  note TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  last_exported_at TEXT,
  CHECK(start_date <= end_date)
);

CREATE TABLE items (
  id TEXT PRIMARY KEY,
  original_name TEXT NOT NULL,
  original_path TEXT NOT NULL UNIQUE,
  normalized_pdf_path TEXT,
  sha256 TEXT NOT NULL,
  mime_type TEXT NOT NULL,
  source_type TEXT NOT NULL CHECK(source_type IN ('email','manual_upload')),
  source_account_id TEXT,
  source_mailbox TEXT,
  source_uid INTEGER,
  source_message_id TEXT,
  source_part_id TEXT,
  fetched_at TEXT NOT NULL,
  invoice_date TEXT,
  suggested_period TEXT,
  batch_id TEXT REFERENCES batches(id) ON DELETE SET NULL,
  suggested_category TEXT,
  final_category TEXT,
  amount_cents INTEGER CHECK(amount_cents IS NULL OR amount_cents >= 0),
  currency TEXT NOT NULL DEFAULT 'CNY',
  city TEXT,
  company TEXT,
  recognition_status TEXT NOT NULL CHECK(recognition_status IN ('pending','succeeded','failed')),
  confirmation_status TEXT NOT NULL CHECK(confirmation_status IN ('pending','confirmed')),
  dedupe_status TEXT NOT NULL CHECK(dedupe_status IN ('unique','suspected_duplicate','resolved')),
  duplicate_of_id TEXT REFERENCES items(id) ON DELETE SET NULL,
  note TEXT,
  event_tag TEXT,
  project_tag TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX idx_items_work_queue ON items(dedupe_status, recognition_status, confirmation_status);
CREATE INDEX idx_items_period ON items(suggested_period);
CREATE INDEX idx_items_batch ON items(batch_id);
CREATE INDEX idx_items_hash ON items(sha256);
CREATE UNIQUE INDEX idx_items_email_part
  ON items(source_account_id, source_mailbox, source_uid, source_part_id)
  WHERE source_type = 'email';

CREATE TABLE mailbox_accounts (
  id TEXT PRIMARY KEY,
  provider TEXT NOT NULL CHECK(provider IN ('gmail','qq')),
  email TEXT NOT NULL UNIQUE,
  imap_host TEXT NOT NULL,
  imap_port INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  sync_interval_minutes INTEGER NOT NULL CHECK(sync_interval_minutes BETWEEN 5 AND 1440),
  last_synced_at TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE sync_cursors (
  account_id TEXT NOT NULL REFERENCES mailbox_accounts(id) ON DELETE CASCADE,
  mailbox TEXT NOT NULL,
  uid_validity INTEGER NOT NULL,
  last_uid INTEGER NOT NULL,
  PRIMARY KEY(account_id, mailbox)
);
CREATE TABLE sync_runs (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES mailbox_accounts(id) ON DELETE CASCADE,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL CHECK(status IN ('running','succeeded','failed')),
  imported_count INTEGER NOT NULL DEFAULT 0,
  error_message TEXT
);
CREATE TABLE settings (key TEXT PRIMARY KEY, value_json TEXT NOT NULL, updated_at TEXT NOT NULL);
```

- [ ] **Step 3: 实现连接、迁移和仓储最小接口**

```rust
// src-tauri/src/db/mod.rs
pub async fn connect(url: &str) -> Result<SqlitePool, AppError> {
    let options = SqliteConnectOptions::from_str(url)
        .map_err(|e| AppError::Internal { message: e.to_string() })?
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new().max_connections(5).connect_with(options).await
        .map_err(internal)?;
    sqlx::migrate!("./migrations").run(&pool).await.map_err(internal)?;
    Ok(pool)
}
```

仓储必须提供并测试这些准确方法：

```rust
impl ItemRepository {
    pub async fn insert(&self, item: &NewItemRecord) -> Result<InvoiceItem, AppError>;
    pub async fn find_by_hash(&self, sha256: &str) -> Result<Option<InvoiceItem>, AppError>;
    pub async fn list(&self, filter: ItemFilter) -> Result<Vec<InvoiceItem>, AppError>;
    pub async fn update_fields(&self, id: Uuid, patch: ItemPatch) -> Result<InvoiceItem, AppError>;
}
impl BatchRepository {
    pub async fn create(&self, batch: NewBatch) -> Result<Batch, AppError>;
    pub async fn get(&self, id: Uuid) -> Result<Batch, AppError>;
    pub async fn list(&self) -> Result<Vec<BatchSummary>, AppError>;
}
```

- [ ] **Step 4: 验证外键、唯一键和汇总查询**

在 `database.rs` 增加三项测试：重复邮件 part 被拒绝；删除批次后 item 的 `batch_id` 变为 null；批次汇总金额只统计已纳入票据。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test database`

Expected: 4 个数据库测试全部 PASS。

- [ ] **Step 5: 提交持久化层**

```bash
git add src-tauri/migrations src-tauri/src/db src-tauri/tests/database.rs src-tauri/src/lib.rs src-tauri/Cargo.toml
git commit -m "feat: add local sqlite persistence"
```

---

### Task 4: 管理本地目录和邮箱凭据

**Files:**
- Create: `src-tauri/src/infra/files.rs`
- Create: `src-tauri/src/infra/credentials.rs`
- Create: `src-tauri/src/state.rs`
- Test: `src-tauri/tests/storage.rs`

- [ ] **Step 1: 写目录隔离和凭据替身失败测试**

```rust
#[test]
fn creates_stable_local_directory_tree() {
    let temp = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(temp.path()).unwrap();
    assert!(paths.originals.is_dir());
    assert!(paths.normalized.is_dir());
    assert!(paths.exports.is_dir());
    assert!(paths.staging.is_dir());
}

#[test]
fn memory_credentials_are_scoped_by_account() {
    let store = MemoryCredentialStore::default();
    store.set("account-a", "secret-a").unwrap();
    assert_eq!(store.get("account-a").unwrap().as_deref(), Some("secret-a"));
    assert_eq!(store.get("account-b").unwrap(), None);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test storage`

Expected: FAIL，类型不存在。

- [ ] **Step 2: 实现目录与原子落盘**

```rust
pub struct AppPaths {
    pub root: PathBuf,
    pub originals: PathBuf,
    pub normalized: PathBuf,
    pub exports: PathBuf,
    pub staging: PathBuf,
}

impl AppPaths {
    pub fn create(root: &Path) -> Result<Self, AppError> {
        let paths = Self {
            root: root.to_path_buf(),
            originals: root.join("originals"),
            normalized: root.join("normalized"),
            exports: root.join("exports"),
            staging: root.join("staging"),
        };
        for path in [&paths.originals, &paths.normalized, &paths.exports, &paths.staging] {
            std::fs::create_dir_all(path).map_err(internal)?;
        }
        Ok(paths)
    }
}
```

所有导入先写 `staging/<uuid>.part`，`sync_all` 后再 `rename` 到 `originals/<yyyy>/<mm>/<uuid>.<ext>`；失败时删除 `.part`。

- [ ] **Step 3: 实现系统钥匙串抽象**

```rust
pub trait CredentialStore: Send + Sync {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError>;
    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError>;
    fn delete(&self, account_id: &str) -> Result<(), AppError>;
}

pub struct KeyringCredentialStore;

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError> {
        let entry = keyring::Entry::new("com.invoice-desk.credentials", account_id).map_err(internal)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(external("keyring", false, error)),
        }
    }
    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError> {
        keyring::Entry::new("com.invoice-desk.credentials", account_id)
            .and_then(|entry| entry.set_password(secret)).map_err(internal)
    }
    fn delete(&self, account_id: &str) -> Result<(), AppError> {
        let entry = keyring::Entry::new("com.invoice-desk.credentials", account_id).map_err(internal)?;
        match entry.delete_credential() { Ok(()) | Err(keyring::Error::NoEntry) => Ok(()), Err(e) => Err(internal(e)) }
    }
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test storage`

Expected: PASS，测试只用 `MemoryCredentialStore`，不访问真实钥匙串。

- [ ] **Step 4: 提交本地基础设施**

```bash
git add src-tauri/src/infra src-tauri/src/state.rs src-tauri/tests/storage.rs src-tauri/Cargo.toml
git commit -m "feat: add local files and secure credential storage"
```

---

### Task 5: 实现手动上传、哈希去重和待处理池入库

**Files:**
- Create: `src-tauri/src/services/import.rs`
- Create: `src-tauri/tests/import_service.rs`
- Modify: `src-tauri/src/db/items.rs`
- Modify: `src-tauri/src/infra/files.rs`

- [ ] **Step 1: 写首次导入和重复导入失败测试**

```rust
#[tokio::test]
async fn imports_manual_file_and_flags_second_copy() {
    let fixture = TestApp::new().await;
    let source = fixture.write_source("taxi.pdf", b"same invoice bytes");
    let first = fixture.imports.import_manual(&source).await.unwrap();
    let second = fixture.imports.import_manual(&source).await.unwrap();
    assert_eq!(first.status, ItemStatus::PendingRecognition);
    assert_eq!(first.dedupe_status, DedupeStatus::Unique);
    assert_eq!(second.status, ItemStatus::SuspectedDuplicate);
    assert_eq!(second.dedupe_status, DedupeStatus::SuspectedDuplicate);
    assert_eq!(second.duplicate_of_id, Some(first.id));
    assert_ne!(second.original_path, first.original_path);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test import_service`

Expected: FAIL，`ImportService` 尚未实现。

- [ ] **Step 2: 实现流式 SHA-256 和 MIME 白名单**

支持 PDF、JPG、JPEG、PNG、DOC、DOCX、XLS、XLSX、ZIP；扩展名和 `infer` 检测不一致时保留文件但设为 `pending_confirmation`，`mime_type` 使用实际检测结果。单文件上限 50 MiB，超限返回 `Validation { field: "file", ... }`。

```rust
pub async fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file = tokio::fs::File::open(path).await.map_err(internal)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(internal)?;
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
```

- [ ] **Step 3: 实现导入事务**

`ImportService::import_manual` 的固定顺序是：校验大小 → staging 复制 → 哈希 → 检测 MIME → 查询重复 → 原子移入 originals → 插入 item。数据库插入失败时必须删除已落盘文件。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test import_service`

Expected: 首次导入、重复导入、超限、扩展名伪装四项测试 PASS。

- [ ] **Step 4: 提交手动导入闭环**

```bash
git add src-tauri/src/services/import.rs src-tauri/src/db/items.rs src-tauri/src/infra/files.rs src-tauri/tests/import_service.rs
git commit -m "feat: import and deduplicate local invoice files"
```

---

### Task 6: 提取票据文本并生成标准化 PDF

**Files:**
- Create: `src-tauri/src/infra/extraction.rs`
- Create: `sidecars/ocr/pyproject.toml`
- Create: `sidecars/ocr/uv.lock`
- Create: `sidecars/ocr/main.py`
- Create: `sidecars/ocr/test_main.py`
- Create: `scripts/build-ocr-sidecar.sh`
- Create: `src-tauri/tests/extraction.rs`
- Create: `src-tauri/tests/fixtures/text-invoice.pdf`
- Create: `src-tauri/tests/fixtures/image-invoice.png`
- Modify: `src-tauri/tauri.conf.json`

- [ ] **Step 1: 写 PDF 文本和图片 OCR 失败测试**

```rust
#[test]
fn extracts_invoice_fields_source_text_from_pdf() {
    let extractor = LocalExtractor::for_test().unwrap();
    let result = extractor.extract(fixture("text-invoice.pdf")).unwrap();
    assert!(result.text.contains("开票日期：2026年06月18日"));
    assert!(result.text.contains("价税合计（小写）¥128.50"));
    assert!(result.normalized_pdf.is_some());
}

#[test]
fn ocrs_image_and_creates_normalized_pdf() {
    let extractor = LocalExtractor::with_ocr(FakeOcr::returning("北京 出租车 价税合计 ¥128.50"));
    let result = extractor.extract(fixture("image-invoice.png")).unwrap();
    assert!(result.text.contains("北京"));
    assert!(result.normalized_pdf.is_some());
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test extraction`

Expected: FAIL，提取器和 OCR 网关不存在。

- [ ] **Step 2: 先写并验证中文 OCR sidecar 契约**

`sidecars/ocr/main.py` 每行读取一个 JSON 请求并每行输出一个 JSON 响应，禁止输出日志到 stdout：

```python
from __future__ import annotations
import json
import sys
from pathlib import Path
import numpy as np
import pypdfium2 as pdfium
from rapidocr_onnxruntime import RapidOCR

engine = RapidOCR()

def recognize(path: str) -> dict[str, object]:
    inputs: list[object]
    if Path(path).suffix.lower() == ".pdf":
        document = pdfium.PdfDocument(path)
        inputs = [np.asarray(page.render(scale=200 / 72).to_pil()) for page in document]
    else:
        inputs = [path]
    lines: list[str] = []
    for image in inputs:
        result, _ = engine(image)
        if result is not None:
            lines.extend(entry[1] for entry in result)
    return {"ok": True, "text": "\n".join(lines), "warnings": []}

for raw in sys.stdin:
    try:
        request = json.loads(raw)
        print(json.dumps(recognize(request["path"]), ensure_ascii=False), flush=True)
    except Exception as error:
        print(json.dumps({"ok": False, "error": str(error)}, ensure_ascii=False), flush=True)
```

`test_main.py` 使用 `image-invoice.png` 断言输出同时包含 `北京`、`128.50`。`pyproject.toml` 使用以下准确依赖并提交由 `uv lock` 生成的 `uv.lock`：

```toml
[project]
name = "invoice-ocr-sidecar"
version = "0.1.0"
requires-python = ">=3.11,<3.13"
dependencies = [
  "numpy==2.3.1",
  "onnxruntime==1.22.1",
  "pillow==11.3.0",
  "pypdfium2==4.30.1",
  "rapidocr-onnxruntime==1.4.4",
]

[dependency-groups]
dev = ["pyinstaller==6.14.2", "pytest==8.4.1"]
```

Run: `uv run --project sidecars/ocr pytest sidecars/ocr/test_main.py -q`

Expected: PASS，中文和金额均可识别。

- [ ] **Step 3: 构建并注册按平台 sidecar**

`scripts/build-ocr-sidecar.sh` 必须执行 PyInstaller onefile 构建，读取 `rustc -Vv` 的 host triple，把产物复制为 `src-tauri/binaries/invoice-ocr-<target-triple>`；Windows 自动追加 `.exe`。`tauri.conf.json` 的 `bundle.externalBin` 只包含 `binaries/invoice-ocr`。模型必须由 PyInstaller spec 明确收进产物，运行时不得联网下载。

Run: `bash scripts/build-ocr-sidecar.sh && src-tauri/binaries/invoice-ocr-$(rustc -Vv | sed -n 's/^host: //p') <<<'{"path":"src-tauri/tests/fixtures/image-invoice.png"}'`

Expected: 输出单行 `{"ok":true,...}`，文本包含 `北京` 和 `128.50`。

- [ ] **Step 4: 实现 Rust OCR 网关和格式分派**

```rust
pub struct ExtractedDocument {
    pub text: String,
    pub normalized_pdf: Option<Vec<u8>>,
    pub warnings: Vec<String>,
}

pub trait DocumentExtractor: Send + Sync {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, AppError>;
}

pub trait OcrGateway: Send + Sync {
    fn recognize(&self, image_path: &Path) -> Result<String, AppError>;
}
```

- PDF：先使用 `pdf_extract` 获取文本；少于 20 个非空白字符时，把 PDF 路径交给 sidecar，由 `pypdfium2` 以 200 DPI 逐页渲染后 OCR；原 PDF 验证后复制为 normalized PDF。
- JPG/PNG：调用 `invoice-ocr` sidecar，再用 `printpdf` 以原图尺寸生成单页 normalized PDF。
- DOC/DOCX/XLS/XLSX/ZIP：返回 `ExtractedDocument { text: "", normalized_pdf: None, warnings: ["unsupported_for_recognition"] }`，不得丢弃原件。
- 损坏文件：返回 `External { service: "document_extractor", retryable: false, ... }`。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test extraction`

Expected: 文本 PDF、扫描 PDF、图片、未支持格式、损坏文件测试全部 PASS；Rust 测试使用 `FakeOcr`，不依赖 Python 环境。

- [ ] **Step 5: 提交本地提取能力**

```bash
git add scripts sidecars/ocr src-tauri/binaries/.gitkeep src-tauri/src/infra/extraction.rs src-tauri/tests/extraction.rs src-tauri/tests/fixtures src-tauri/tauri.conf.json src-tauri/Cargo.toml
git commit -m "feat: extract invoice text and normalize documents"
```

---

### Task 7: 识别字段、建议时间和四类分类

**Files:**
- Create: `src-tauri/src/services/recognition.rs`
- Create: `src-tauri/tests/recognition.rs`
- Modify: `src-tauri/src/db/items.rs`

- [ ] **Step 1: 写规则优先级失败测试**

```rust
#[test]
fn invoice_date_beats_received_date_for_suggested_period() {
    let result = recognize("开票日期：2026年06月18日 餐饮服务 价税合计 ¥128.50", date("2026-07-02"));
    assert_eq!(result.invoice_date.as_deref(), Some("2026-06-18"));
    assert_eq!(result.suggested_period, "2026-06");
    assert_eq!(result.suggested_category, Some(Category::Dining));
    assert_eq!(result.amount_cents, Some(12850));
}

#[test]
fn received_date_is_fallback_and_low_confidence_needs_confirmation() {
    let result = recognize("电子票据", date("2026-07-02"));
    assert_eq!(result.suggested_period, "2026-07");
    assert_eq!(result.suggested_category, None);
    assert_eq!(result.status, ItemStatus::PendingConfirmation);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test recognition`

Expected: FAIL。

- [ ] **Step 2: 实现确定性字段规则**

固定规则：

- 日期依次匹配 `开票日期`、`日期` 后的 `YYYY年MM月DD日` 或 `YYYY-MM-DD`；无命中用邮件接收/手动导入日期。
- 金额依次匹配 `价税合计（小写）`、`价税合计`、`合计` 后的人民币金额；转成分时使用 Decimal，不使用浮点数。
- 公司主体匹配 `购买方` 或 `销售方` 名称段；城市匹配行政区词典中的市名。
- 交通关键词：出租车、网约车、铁路、航空、客运、滴滴；餐饮：餐饮、食品、饭店、餐厅；住宿：住宿、酒店、宾馆；招待：招待、礼品、会务。
- 只有唯一最高分且分数至少 2 时产生 `suggested_category`；否则为空。
- 提取完成后 `recognition_status=succeeded`；日期、金额、类别都有值且不存在 warning 时 `confirmation_status=confirmed`，否则为 `pending`；提取异常设 `recognition_status=failed`。`dedupe_status` 不在识别流程中修改，界面状态统一调用 `derive_item_status` 计算。

- [ ] **Step 3: 串联导入后的识别持久化**

```rust
impl RecognitionService {
    pub async fn recognize_item(&self, item_id: Uuid) -> Result<InvoiceItem, AppError>;
    pub async fn retry(&self, item_id: Uuid) -> Result<InvoiceItem, AppError>;
}
```

`retry` 清理旧的自动字段但保留 `final_category`、`batch_id`、`note`、`event_tag`、`project_tag` 等人工字段。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test recognition`

Expected: 日期优先级、金额精度、四类关键词、冲突分类、重试保留人工字段全部 PASS。

- [ ] **Step 4: 提交识别管线**

```bash
git add src-tauri/src/services/recognition.rs src-tauri/src/db/items.rs src-tauri/tests/recognition.rs src-tauri/Cargo.toml
git commit -m "feat: recognize and classify invoice metadata"
```

---

### Task 8: 实现票据人工修正和状态流转

**Files:**
- Create: `src-tauri/src/services/items.rs`
- Create: `src-tauri/tests/item_review.rs`
- Modify: `src-tauri/src/db/items.rs`

- [ ] **Step 1: 写人工确认失败测试**

```rust
#[tokio::test]
async fn manual_review_can_override_period_category_and_metadata() {
    let app = TestApp::with_pending_item().await;
    let updated = app.items.review(ItemReview {
        id: app.item_id,
        invoice_date: Some("2026-06-20".into()),
        suggested_period: "2026-06".into(),
        final_category: Category::Hospitality,
        amount_cents: 32000,
        city: Some("上海".into()),
        company: Some("示例科技有限公司".into()),
        note: Some("客户晚餐".into()),
        event_tag: Some("客户接待".into()),
        project_tag: Some("P-2026-07".into()),
    }).await.unwrap();
    assert_eq!(updated.status, ItemStatus::Ready);
    assert_eq!(updated.final_category, Some(Category::Hospitality));
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test item_review`

Expected: FAIL。

- [ ] **Step 2: 实现校验和状态机**

```rust
pub struct ItemReview {
    pub id: Uuid,
    pub invoice_date: Option<String>,
    pub suggested_period: String,
    pub final_category: Category,
    pub amount_cents: i64,
    pub city: Option<String>,
    pub company: Option<String>,
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
}
```

校验：`suggested_period` 必须是合法 `YYYY-MM`；金额非负；文本 trim 后空值转 null；确认后设 `confirmation_status=confirmed`，派生状态变为 `ready`，但疑似重复项必须先调用 `resolve_duplicate(item_id, keep: bool)`。

- [ ] **Step 3: 实现重复项处置**

- `keep = true`：清空 `duplicate_of_id`，设 `dedupe_status=resolved`，派生状态按识别/确认字段变为 `ready`、`pending_confirmation` 或 `recognition_failed`。
- `keep = false`：删除数据库记录和该记录自己的原件/标准化文件，不删除被指向原件。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test item_review`

Expected: 修正、非法月份、保留重复、删除重复四项测试 PASS。

- [ ] **Step 4: 提交人工确认能力**

```bash
git add src-tauri/src/services/items.rs src-tauri/src/db/items.rs src-tauri/tests/item_review.rs
git commit -m "feat: review and correct invoice items"
```

---

### Task 9: 实现批次创建、推荐和归属

**Files:**
- Create: `src-tauri/src/services/batches.rs`
- Create: `src-tauri/tests/batches.rs`
- Modify: `src-tauri/src/db/batches.rs`
- Modify: `src-tauri/src/db/items.rs`

- [ ] **Step 1: 写月度和跨月批次失败测试**

```rust
#[tokio::test]
async fn creates_month_and_custom_range_batches() {
    let app = TestApp::new().await;
    let june = app.batches.create_month(2026, 6).await.unwrap();
    assert_eq!((june.name.as_str(), june.start_date.as_str(), june.end_date.as_str()),
               ("2026 年 6 月报销", "2026-06-01", "2026-06-30"));
    let range = app.batches.create(NewBatchInput {
        name: "6-8 月整理批次".into(), start_date: "2026-06-01".into(),
        end_date: "2026-08-31".into(), note: None,
    }).await.unwrap();
    assert_eq!(range.end_date, "2026-08-31");
}

#[tokio::test]
async fn recommends_unbatched_items_by_suggested_period() {
    let app = TestApp::with_items_in_periods(["2026-05", "2026-06", "2026-07", "2026-08"]).await;
    let ids = app.batches.recommend("2026-06-01", "2026-07-31").await.unwrap();
    assert_eq!(ids.len(), 2);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test batches`

Expected: FAIL。

- [ ] **Step 2: 实现批次服务**

```rust
impl BatchService {
    pub async fn create_month(&self, year: i32, month: u32) -> Result<Batch, AppError>;
    pub async fn create(&self, input: NewBatchInput) -> Result<Batch, AppError>;
    pub async fn recommend(&self, start: &str, end: &str) -> Result<Vec<InvoiceItem>, AppError>;
    pub async fn assign_items(&self, batch_id: Uuid, item_ids: &[Uuid]) -> Result<BatchDetail, AppError>;
    pub async fn remove_item(&self, batch_id: Uuid, item_id: Uuid) -> Result<BatchDetail, AppError>;
}
```

`assign_items` 允许跨月和补开票据；只拒绝不存在、疑似重复或识别失败的 item。票据日期不在批次范围内时允许加入，但在 `BatchDetail.warnings` 返回 `outside_date_range:<item_id>`。

- [ ] **Step 3: 实现汇总和状态回退**

批次汇总返回票据数、总金额、四类金额/数量、未确认数量。已导出批次发生名称、日期、票据或金额变化时，将状态自动回退为 `draft` 并保留 `last_exported_at`。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test batches`

Expected: 创建、推荐、人工跨期归属、非法项拒绝、汇总和导出状态回退测试 PASS。

- [ ] **Step 4: 提交批次领域闭环**

```bash
git add src-tauri/src/services/batches.rs src-tauri/src/db/batches.rs src-tauri/src/db/items.rs src-tauri/tests/batches.rs
git commit -m "feat: manage reimbursement batches"
```

---

### Task 10: 接入 QQ/Gmail IMAP 增量同步

**Files:**
- Create: `src-tauri/src/infra/imap.rs`
- Create: `src-tauri/src/services/sync.rs`
- Create: `src-tauri/tests/sync_service.rs`
- Create: `src-tauri/tests/fixtures/mail/attachment.eml`
- Create: `src-tauri/tests/fixtures/mail/inline-image.eml`
- Create: `src-tauri/tests/fixtures/mail/download-link.eml`
- Modify: `src-tauri/src/db/accounts.rs`
- Modify: `src-tauri/src/services/import.rs`

- [ ] **Step 1: 写增量与幂等失败测试**

```rust
#[tokio::test]
async fn imports_attachment_and_inline_image_once_and_advances_cursor() {
    let gateway = FakeImapGateway::messages(vec![fixture_mail("attachment.eml"), fixture_mail("inline-image.eml")]);
    let app = TestApp::with_gateway(gateway).await;
    let first = app.sync.run(account()).await.unwrap();
    let second = app.sync.run(account()).await.unwrap();
    assert_eq!(first.imported_count, 2);
    assert_eq!(second.imported_count, 0);
    assert_eq!(app.cursor().await.last_uid, 102);
}

#[tokio::test]
async fn changed_uid_validity_rescans_without_duplicate_items() {
    let app = TestApp::with_uid_validity_sequence([10, 11]).await;
    app.sync.run(account()).await.unwrap();
    app.sync.run(account()).await.unwrap();
    assert_eq!(app.items.count().await, 1);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test sync_service`

Expected: FAIL。

- [ ] **Step 2: 定义可替换网关**

```rust
#[async_trait]
pub trait ImapGateway: Send + Sync {
    async fn test_connection(&self, config: &ImapConfig, secret: &str) -> Result<(), AppError>;
    async fn fetch_since(&self, config: &ImapConfig, secret: &str, cursor: Option<SyncCursor>)
        -> Result<MailboxDelta, AppError>;
}

pub struct MailboxDelta {
    pub uid_validity: u32,
    pub messages: Vec<RawMessage>,
    pub highest_uid: u32,
}
```

提供商默认值固定为 Gmail `imap.gmail.com:993`、QQ `imap.qq.com:993`、TLS 必开；Gmail/QQ 都使用用户生成的应用专用密码/授权码，不实现 OAuth。

- [ ] **Step 3: 解析邮件材料**

使用 `mail-parser`，只导入以下部分：

- disposition 为 attachment 的任意支持文件；
- 有 filename 或 content-id 的 `image/jpeg`、`image/png` 内嵌 part；
- HTML 正文中的 `https` 下载链接仅保存为 item 元数据并进入 `pending_confirmation`，不得在后台自动访问第三方链接。

每个 part 同时写入 `source_mailbox`、IMAP UID、RFC Message-ID 和 MIME part 序号；幂等键使用账号 + mailbox + UID + part 序号，RFC Message-ID 只作为可追踪元数据。邮件接收日期传给识别服务作为时间兜底。

- [ ] **Step 4: 实现同步事务与错误记录**

每次同步先插入 `sync_runs.running`；逐 part 导入成功后才更新游标；连接/鉴权失败记录 `mailbox_accounts.last_error` 和 `sync_runs.failed`，游标不前移；单个损坏 part 记录 item 为 `recognition_failed` 后继续其他 part。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test sync_service`

Expected: 附件、内嵌图、链接占位、幂等、UIDVALIDITY、鉴权失败、单 part 失败共 7 项测试 PASS。

- [ ] **Step 5: 提交邮箱同步**

```bash
git add src-tauri/src/infra/imap.rs src-tauri/src/services/sync.rs src-tauri/src/db/accounts.rs src-tauri/src/services/import.rs src-tauri/tests/sync_service.rs src-tauri/tests/fixtures/mail src-tauri/Cargo.toml
git commit -m "feat: sync invoice materials from imap"
```

---

### Task 11: 实现邮箱设置、连接测试和后台调度

**Files:**
- Create: `src-tauri/src/services/settings.rs`
- Create: `src-tauri/src/services/scheduler.rs`
- Create: `src-tauri/tests/settings_scheduler.rs`
- Modify: `src-tauri/src/state.rs`
- Modify: `src-tauri/src/db/accounts.rs`

- [ ] **Step 1: 写凭据不入库和到期调度失败测试**

```rust
#[tokio::test]
async fn saves_account_metadata_but_secret_only_in_credential_store() {
    let app = TestApp::new().await;
    let saved = app.settings.save_account(gmail_input("app-secret")).await.unwrap();
    assert_eq!(app.credentials.get(&saved.id.to_string()).unwrap().as_deref(), Some("app-secret"));
    let persisted: String = sqlx::query_scalar(
        "SELECT email || imap_host || coalesce(last_error, '') FROM mailbox_accounts WHERE id = ?"
    ).bind(saved.id.to_string()).fetch_one(&app.pool).await.unwrap();
    assert!(!persisted.contains("app-secret"));
}

#[tokio::test]
async fn scheduler_only_runs_enabled_due_accounts() {
    let app = TestApp::with_due_and_disabled_accounts().await;
    app.scheduler.tick(Utc::now()).await;
    assert_eq!(app.gateway.synced_account_ids(), vec![app.due_account_id]);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test settings_scheduler`

Expected: FAIL。

- [ ] **Step 2: 实现邮箱配置生命周期**

```rust
impl SettingsService {
    pub async fn save_account(&self, input: SaveAccountInput) -> Result<MailboxAccount, AppError>;
    pub async fn test_account(&self, input: TestAccountInput) -> Result<(), AppError>;
    pub async fn delete_account(&self, account_id: Uuid) -> Result<(), AppError>;
    pub async fn save_preferences(&self, input: PreferencesInput) -> Result<Preferences, AppError>;
}
```

`save_account` 必须先测试 IMAP 连接，再在数据库事务中写元数据，最后写钥匙串；钥匙串失败要回滚元数据。删除时先停用，再删凭据和数据库记录。

- [ ] **Step 3: 实现后台调度和并发保护**

应用启动后每 60 秒 tick 一次；按 `last_synced_at + sync_interval_minutes` 判断到期；同一账号通过 `DashMap<Uuid, Mutex<()>>` 禁止并发同步；网络类失败按 1、5、15 分钟退避，鉴权失败不自动重试并在控制台显示。

应用窗口关闭时默认隐藏到托盘，选择“退出”才停止调度器；设置里的“后台自动抓取”关闭时不启动新任务，当前任务允许完成。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test settings_scheduler`

Expected: 凭据隔离、连接失败不保存、到期判断、禁用账号、并发保护、退避测试 PASS。

- [ ] **Step 4: 提交设置和后台任务**

```bash
git add src-tauri/src/services/settings.rs src-tauri/src/services/scheduler.rs src-tauri/src/state.rs src-tauri/src/db/accounts.rs src-tauri/tests/settings_scheduler.rs
git commit -m "feat: configure mailboxes and schedule background sync"
```

---

### Task 12: 生成完整报销包

**Files:**
- Create: `src-tauri/src/services/export.rs`
- Create: `src-tauri/src/infra/exporters.rs`
- Create: `src-tauri/tests/export_package.rs`
- Modify: `src-tauri/src/db/batches.rs`

- [ ] **Step 1: 写导出阻断和完整产物失败测试**

```rust
#[tokio::test]
async fn refuses_export_when_batch_contains_unconfirmed_item() {
    let app = TestApp::with_batch_item(ItemStatus::PendingConfirmation).await;
    let error = app.exports.export(app.batch_id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict { message } if message.contains("1 张票据待确认")));
}

#[tokio::test]
async fn exports_pdf_xlsx_originals_and_manifest() {
    let app = TestApp::with_exportable_batch().await;
    let result = app.exports.export(app.batch_id).await.unwrap();
    for name in ["merged.pdf", "reimbursement.xlsx", "originals.zip", "manifest.json"] {
        assert!(result.directory.join(name).is_file(), "missing {name}");
    }
    assert_eq!(result.item_count, 2);
    assert_eq!(result.total_amount_cents, 44850);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test export_package`

Expected: FAIL。

- [ ] **Step 2: 实现导出前检查和固定命名**

导出目录为 `<exports>/<批次名称>-<YYYYMMDD-HHmmss>/`，批次名称需替换路径非法字符。阻断条件：空批次、待确认/识别失败/疑似重复项、缺少 normalized PDF、原件丢失。

- [ ] **Step 3: 实现四个原子产物**

- `merged.pdf`：按开票日期、创建时间、item id 排序合并 normalized PDF；页尺寸保留原件。
- `reimbursement.xlsx`：列顺序固定为开票日期、建议归属时间、最终归属批次、分类、金额、城市、公司主体、来源、备注、事项标签、项目标签；金额写数值并使用 `¥#,##0.00`。
- `originals.zip`：文件名使用 `<序号>-<item-id前8位>-<清理后的原名>`，冲突不会覆盖。
- `manifest.json`：写批次 id、导出时间、item id/sha256/文件名、应用版本，以及 `merged.pdf`、`reimbursement.xlsx`、`originals.zip` 三个产物的 SHA-256；manifest 不对自身做循环哈希。

先在 `staging/export-<uuid>` 生成并验证，再原子 rename 到最终目录；失败删除 staging，批次状态保持 draft。成功后一次事务更新 `status=exported,last_exported_at=now`。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test export_package`

Expected: 阻断条件、字段顺序、金额精度、ZIP 原件、manifest 哈希、失败清理测试 PASS。

- [ ] **Step 4: 提交导出闭环**

```bash
git add src-tauri/src/services/export.rs src-tauri/src/infra/exporters.rs src-tauri/src/db/batches.rs src-tauri/tests/export_package.rs src-tauri/Cargo.toml
git commit -m "feat: export complete reimbursement packages"
```

---

### Task 13: 暴露窄 Tauri Commands 和预览协议

**Files:**
- Create: `src-tauri/src/commands/mod.rs`
- Create: `src-tauri/src/commands/dashboard.rs`
- Create: `src-tauri/src/commands/items.rs`
- Create: `src-tauri/src/commands/batches.rs`
- Create: `src-tauri/src/commands/settings.rs`
- Create: `src-tauri/src/commands/sync.rs`
- Create: `src-tauri/src/commands/export.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/capabilities/default.json`
- Create: `src-tauri/tests/commands.rs`

- [ ] **Step 1: 写 dashboard DTO 和 command 失败测试**

```rust
#[tokio::test]
async fn dashboard_counts_are_derived_from_persisted_state() {
    let app = TestApp::with_dashboard_fixture().await;
    let dto = dashboard::load(&app.state).await.unwrap();
    assert_eq!(dto.pending_confirmation_count, 2);
    assert_eq!(dto.recognition_failed_count, 1);
    assert_eq!(dto.suspected_duplicate_count, 1);
    assert_eq!(dto.recently_added_count, 3);
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test commands`

Expected: FAIL。

- [ ] **Step 2: 实现 command 清单**

注册以下准确名称，前端 `api.ts` 只能调用这些名称：

```text
get_dashboard
list_items
get_item
import_manual_files
review_item
resolve_duplicate
retry_recognition
list_batches
get_batch
create_month_batch
create_custom_batch
assign_items_to_batch
remove_item_from_batch
export_batch
list_mailbox_accounts
save_mailbox_account
test_mailbox_account
delete_mailbox_account
get_preferences
save_preferences
sync_account_now
```

所有 command 只做 DTO 转换和 service 调用；错误直接返回 `Result<T, AppError>`。`sync_account_now` 在运行时返回 `Conflict`，避免重复任务。

- [ ] **Step 3: 添加受限预览协议**

注册 `invoice-file://item/<uuid>` 自定义协议。处理器必须先用 item id 查数据库，只允许读取该 item 的 original/normalized 路径；canonicalize 后验证路径位于 AppPaths 根目录下，拒绝任意用户传入路径。CSP 只允许 `self`、`invoice-file:`，禁用远程脚本。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test commands`

Expected: DTO 汇总、错误序列化、允许预览、路径穿越拒绝测试 PASS。

- [ ] **Step 4: 提交桌面 API 边界**

```bash
git add src-tauri/src/commands src-tauri/src/lib.rs src-tauri/capabilities/default.json src-tauri/tests/commands.rs
git commit -m "feat: expose secure desktop application commands"
```

---

### Task 14: 构建前端 API、应用壳和控制台

**Files:**
- Create: `src/lib/api.ts`
- Create: `src/lib/queryKeys.ts`
- Create: `src/test/mockApi.ts`
- Create: `src/app/AppShell.tsx`
- Create: `src/app/AppShell.css`
- Create: `src/features/dashboard/DashboardPage.tsx`
- Create: `src/features/dashboard/DashboardPage.test.tsx`
- Modify: `src/app/App.tsx`
- Modify: `src/types.ts`

- [ ] **Step 1: 写控制台失败测试**

```tsx
it("shows synchronization health and work queues", async () => {
  mockCommand("get_dashboard", dashboardFixture({
    pendingConfirmationCount: 4,
    recognitionFailedCount: 1,
    suspectedDuplicateCount: 2,
    recentlyAddedCount: 7,
  }));
  renderAppAt("/");
  expect(await screen.findByText("4")).toBeInTheDocument();
  expect(screen.getByText("待确认")).toBeInTheDocument();
  expect(screen.getByText("识别失败")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "立即同步" })).toBeEnabled();
  expect(screen.getByRole("button", { name: "新建批次" })).toBeEnabled();
});
```

Run: `npm test -- src/features/dashboard/DashboardPage.test.tsx`

Expected: FAIL。

- [ ] **Step 2: 实现类型安全 API 层**

```ts
import { invoke } from "@tauri-apps/api/core";
import type { DashboardDto, ItemFilter, InvoiceItemDto } from "../types";

export const api = {
  getDashboard: () => invoke<DashboardDto>("get_dashboard"),
  listItems: (filter: ItemFilter) => invoke<InvoiceItemDto[]>("list_items", { filter }),
  importManualFiles: (paths: string[]) => invoke<InvoiceItemDto[]>("import_manual_files", { paths }),
  syncAccountNow: (accountId: string) => invoke("sync_account_now", { accountId }),
};
```

为 Task 13 的每个 command 都提供一个命名一致的方法，不允许页面直接 import `invoke`。

- [ ] **Step 3: 实现安静、密集的桌面工作台壳**

左侧固定导航：控制台、待处理池、报销批次、设置；顶部状态区显示“同步中/正常/需处理”。使用 Lucide 图标和 tooltip，窗口宽度低于 900px 时收窄为图标导航。不要做营销 hero、渐变背景或嵌套卡片。

控制台包含：邮箱状态与上次同步、最近新增、待确认、识别失败、疑似重复、最近批次、立即同步、新建月度/自定义批次入口。所有计数可点击跳到已带 filter 的待处理池。

Run: `npm test -- src/features/dashboard/DashboardPage.test.tsx && npm run build`

Expected: 测试和生产构建 PASS。

- [ ] **Step 4: 提交应用壳与控制台**

```bash
git add src/app src/features/dashboard src/lib src/test src/types.ts
git commit -m "feat: add desktop shell and operations dashboard"
```

---

### Task 15: 构建待处理池、手动上传和票据抽屉

**Files:**
- Create: `src/features/inbox/InboxPage.tsx`
- Create: `src/features/inbox/InboxTable.tsx`
- Create: `src/features/inbox/ItemDrawer.tsx`
- Create: `src/features/inbox/ItemPreview.tsx`
- Create: `src/features/inbox/InboxPage.test.tsx`
- Create: `src/components/FileDropZone.tsx`

- [ ] **Step 1: 写筛选和修正失败测试**

```tsx
it("filters pending items and saves a manual correction", async () => {
  const user = userEvent.setup();
  mockCommand("list_items", [pendingInvoiceFixture()]);
  renderAppAt("/inbox?status=pending_confirmation");
  await user.click(await screen.findByText("出租车电子发票.pdf"));
  await user.click(screen.getByRole("radio", { name: "交通" }));
  await user.clear(screen.getByLabelText("金额"));
  await user.type(screen.getByLabelText("金额"), "128.50");
  await user.click(screen.getByRole("button", { name: "保存并确认" }));
  expect(commandCalls("review_item")[0].amountCents).toBe(12850);
});
```

Run: `npm test -- src/features/inbox/InboxPage.test.tsx`

Expected: FAIL。

- [ ] **Step 2: 实现稳定表格和状态筛选**

表格列：原件、开票日期、建议月份、分类、金额、来源、状态、批次；状态 tabs 精确对应五个 ItemStatus，另有“全部”。行高固定，加载、空态、错误、重试不改变表头布局。支持按建议月份、分类、来源和文本过滤。

- [ ] **Step 3: 实现文件选择/拖放和导入反馈**

使用 Tauri dialog 选择多文件，并支持把系统拖入的路径传给 `import_manual_files`。逐文件显示导入结果；重复项跳转“疑似重复”，失败项保留错误信息和重试入口。

- [ ] **Step 4: 实现票据详情抽屉**

抽屉左侧预览原件/normalized PDF，右侧显示来源、自动值与最终值。分类用四项 segmented control；时间、金额、城市、主体、备注、事项和项目可编辑；批次归属独立显示。提供重新识别、确认保留/删除重复、保存并确认操作。

Run: `npm test -- src/features/inbox/InboxPage.test.tsx && npm run build`

Expected: 筛选、上传成功/失败、金额转分、手动修正、重复处置、重识别测试 PASS。

- [ ] **Step 5: 提交待处理工作流**

```bash
git add src/features/inbox src/components/FileDropZone.tsx src/types.ts src/lib/api.ts
git commit -m "feat: add invoice inbox and manual review workflow"
```

---

### Task 16: 构建报销批次列表、详情和导出界面

**Files:**
- Create: `src/features/batches/BatchListPage.tsx`
- Create: `src/features/batches/CreateBatchDialog.tsx`
- Create: `src/features/batches/BatchDetailPage.tsx`
- Create: `src/features/batches/AssignItemsDialog.tsx`
- Create: `src/features/batches/BatchDetailPage.test.tsx`

- [ ] **Step 1: 写跨月创建和导出失败测试**

```tsx
it("creates a custom range, assigns recommendations and exports", async () => {
  const user = userEvent.setup();
  mockBatchCommands();
  renderAppAt("/batches/new");
  await user.type(screen.getByLabelText("批次名称"), "6-8 月整理批次");
  await user.type(screen.getByLabelText("开始日期"), "2026-06-01");
  await user.type(screen.getByLabelText("结束日期"), "2026-08-31");
  await user.click(screen.getByRole("button", { name: "创建批次" }));
  expect(commandCalls("create_custom_batch")[0].endDate).toBe("2026-08-31");
  await user.click(screen.getByRole("button", { name: "加入推荐票据" }));
  await user.click(screen.getByRole("button", { name: "导出报销包" }));
  expect(await screen.findByText(/merged\.pdf/)).toBeInTheDocument();
});
```

Run: `npm test -- src/features/batches/BatchDetailPage.test.tsx`

Expected: FAIL。

- [ ] **Step 2: 实现批次创建和列表**

创建对话框首选“按月”模式，年月默认当前月；切换“自定义范围”显示名称和起止日期。列表按 updatedAt 降序，展示范围、状态、数量、总金额、未确认数、最后导出时间。

- [ ] **Step 3: 实现批次详情和归属操作**

详情顶部固定展示时间范围、票据数量、总金额和导出按钮；中部展示四类统计；下方是已纳入票据表。加入票据对话框默认加载时间范围内未归属 item，但允许搜索范围外 item，并明确显示“日期超出批次范围”警告。移除操作需确认。

- [ ] **Step 4: 实现导出状态反馈**

有待确认项时禁用导出并提供跳转过滤器；导出运行时显示确定性 progress state，成功后展示四个文件名和“在文件夹中显示”，失败后显示后端可重试信息，不伪造成功状态。

Run: `npm test -- src/features/batches/BatchDetailPage.test.tsx && npm run build`

Expected: 月度/跨月创建、推荐、范围外警告、移除、导出阻断和成功反馈测试 PASS。

- [ ] **Step 5: 提交批次界面**

```bash
git add src/features/batches src/types.ts src/lib/api.ts
git commit -m "feat: add reimbursement batch workspace"
```

---

### Task 17: 构建设置页、托盘状态和可恢复错误提示

**Files:**
- Create: `src/features/settings/SettingsPage.tsx`
- Create: `src/features/settings/MailboxAccountForm.tsx`
- Create: `src/features/settings/SettingsPage.test.tsx`
- Create: `src/components/AppErrorBanner.tsx`
- Modify: `src/app/AppShell.tsx`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: 写邮箱配置和失败反馈测试**

```tsx
it("tests a Gmail connection before saving the account", async () => {
  const user = userEvent.setup();
  mockCommand("test_mailbox_account", undefined);
  renderAppAt("/settings");
  await user.selectOptions(screen.getByLabelText("邮箱类型"), "gmail");
  await user.type(screen.getByLabelText("邮箱地址"), "person@gmail.com");
  await user.type(screen.getByLabelText("应用专用密码"), "secret");
  await user.click(screen.getByRole("button", { name: "测试连接" }));
  expect(await screen.findByText("连接成功")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "保存账号" }));
  expect(commandCalls("save_mailbox_account")).toHaveLength(1);
});
```

Run: `npm test -- src/features/settings/SettingsPage.test.tsx`

Expected: FAIL。

- [ ] **Step 2: 实现邮箱和同步设置**

QQ/Gmail 选择后自动填写 host/port，并显示如何获取授权码/应用密码的简短链接。高级区允许覆盖 host/port。同步频率用 5–1440 分钟数值输入，后台同步用 switch。保存前必须成功测试连接；编辑已有账号时空密码表示保留旧密码。

- [ ] **Step 3: 实现导出和存储设置**

展示当前数据目录、导出目录和可用空间；允许选择新导出目录，但不在 MVP 中迁移原件数据库。命名规则只开放批次目录格式预览，不允许任意脚本模板。

- [ ] **Step 4: 实现全局错误和托盘行为**

鉴权失效显示常驻 banner，按钮直达对应邮箱设置；网络失败显示上次失败时间和“立即重试”。托盘菜单精确包含“显示发票报销”“立即同步全部”“退出”，托盘 tooltip 显示待确认数。应用退出前等待当前事务完成，最长 10 秒后记录中止状态。

Run: `npm test -- src/features/settings/SettingsPage.test.tsx && npm run build && cargo test --manifest-path src-tauri/Cargo.toml`

Expected: 前后端全部 PASS。

- [ ] **Step 5: 提交运行设置和错误恢复 UI**

```bash
git add src/features/settings src/components/AppErrorBanner.tsx src/app/AppShell.tsx src-tauri/src/lib.rs src/lib/api.ts
git commit -m "feat: add mailbox settings and recoverable app errors"
```

---

### Task 18: 完成端到端验收、CI 和桌面打包

**Files:**
- Create: `playwright.config.ts`
- Create: `tests/e2e/mvp-flow.spec.ts`
- Create: `src/test/browserCommandBridge.ts`
- Create: `.github/workflows/ci.yml`
- Create: `docs/operations/local-data-and-recovery.md`
- Create: `docs/operations/release-checklist.md`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `package.json`

- [ ] **Step 1: 写完整用户流 E2E**

```ts
test("manual invoice to exported reimbursement package", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("link", { name: "待处理池" }).click();
  await page.getByRole("button", { name: "上传票据" }).click();
  await page.setInputFiles('input[type="file"]', "src-tauri/tests/fixtures/text-invoice.pdf");
  await expect(page.getByText("text-invoice.pdf")).toBeVisible();
  await page.getByText("text-invoice.pdf").click();
  await page.getByRole("radio", { name: "餐饮" }).click();
  await page.getByRole("button", { name: "保存并确认" }).click();
  await page.getByRole("link", { name: "报销批次" }).click();
  await page.getByRole("button", { name: "新建批次" }).click();
  await page.getByRole("button", { name: "创建批次" }).click();
  await page.getByRole("button", { name: "加入推荐票据" }).click();
  await page.getByRole("button", { name: "导出报销包" }).click();
  await expect(page.getByText("reimbursement.xlsx")).toBeVisible();
});
```

浏览器测试通过 `browserCommandBridge.ts` 使用内存 command 实现；Rust 集成测试已覆盖真实文件和 SQLite，不能把浏览器 mock 当作后端验收替代品。

Run: `npm run test:e2e`

Expected: Chromium 桌面视口和 800×700 最小窗口视口均 PASS，无水平滚动、遮挡或文字溢出。

- [ ] **Step 2: 添加 CI 质量门**

CI 在 macOS runner 执行：

```bash
npm ci
uv sync --project sidecars/ocr --frozen
uv run --project sidecars/ocr pytest sidecars/ocr/test_main.py -q
bash scripts/build-ocr-sidecar.sh
npm run lint
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npm run test:e2e
```

Expected: 任一步失败都会阻止 release job。

- [ ] **Step 3: 写数据恢复和发布检查文档**

`local-data-and-recovery.md` 必须写清数据目录、数据库、原件、钥匙串不在备份包中的事实、备份/恢复顺序、异常关机后的 `.part` 清理。`release-checklist.md` 必须列 QQ/Gmail 实机测试、后台 2 小时同步、断网恢复、授权失效、重复邮件、跨月批次、四类票据、导出四文件、安装/卸载不误删数据。

- [ ] **Step 4: 运行完整验收和生产打包**

Run:

```bash
npm run lint
npm test
cargo test --manifest-path src-tauri/Cargo.toml
npm run test:e2e
npm run tauri build
```

Expected: 所有测试 PASS；`src-tauri/target/release/bundle/` 生成当前平台安装包；安装包启动后可完成手动上传到导出的完整流程。

- [ ] **Step 5: 执行人工验收**

按 `docs/operations/release-checklist.md` 使用一个真实 QQ 测试邮箱和一个真实 Gmail 测试邮箱。截图记录控制台、待处理池、跨月批次、票据详情和导出结果；用解压/Excel/PDF 阅读器分别打开四个导出文件，核对 item 数、总金额和 SHA-256。

- [ ] **Step 6: 提交发布门和文档**

```bash
git add .github playwright.config.ts tests/e2e src/test/browserCommandBridge.ts docs/operations package.json package-lock.json src-tauri/tauri.conf.json
git commit -m "test: add mvp release gates and desktop packaging"
```

---

## 3. 规格覆盖矩阵

| 设计要求 | 对应任务 | 验收证据 |
|---|---:|---|
| 桌面 App、本地优先 | 1、3、4、13、18 | 安装包；SQLite 和本地数据目录 |
| QQ/Gmail IMAP 自动抓取 | 10、11、17 | 两邮箱实机连接；增量/后台测试 |
| 附件、内嵌图片、链接、多格式保留 | 5、6、10 | 邮件 fixture 和提取集成测试 |
| 手动上传 | 5、15、18 | 导入服务测试和 E2E |
| 待处理五种状态 | 2、7、8、14、15 | 状态机测试和筛选 UI |
| 四类分类 + 待确认 | 7、8、15 | 规则测试和人工修正测试 |
| 建议时间与最终批次分离 | 2、7、9、15、16 | 字段契约与跨期归属测试 |
| 单月快捷批次与跨月批次 | 9、16 | 服务/UI 测试 |
| 错误可见、可追踪、可重试 | 10、11、13、17 | sync_runs、banner 和重试测试 |
| 合并 PDF、清单、原件归档 | 12、16、18 | 导出集成测试和人工开包验收 |
| 首页控制台与设置页 | 14、17 | Testing Library 测试 |
| 后台持续运行 | 11、17、18 | 调度器测试、托盘行为、2 小时实测 |

## 4. 完成定义

只有同时满足以下条件，MVP 才能标记完成：

- 所有自动化测试、lint、clippy 和生产构建通过。
- QQ/Gmail 各完成一次首次同步、增量同步、断网重试和授权失效验收。
- 手动上传与邮件抓取的同一文件被标为疑似重复，用户可以保留或删除。
- 用户能创建单月和跨月批次，手动覆盖票据建议月份和最终批次。
- 四类分类都至少有一张 fixture；低置信结果不会被强制分类。
- 待确认、识别失败、疑似重复项能从控制台定位并处理。
- 可导出批次生成四个可打开文件，金额/数量/哈希与数据库一致。
- 安装包在干净用户环境启动，关闭窗口后后台同步仍运行，明确退出后任务停止。
- 备份、恢复、升级和异常中断行为已有操作文档。

## 5. 推荐执行节奏

- Milestone A（Task 1–5）：本地数据骨架与手动导入可用。
- Milestone B（Task 6–9）：识别、人工确认和批次核心闭环可用。
- Milestone C（Task 10–13）：邮件后台抓取、导出和桌面 API 可用。
- Milestone D（Task 14–18）：完整 UI、错误恢复、打包和验收完成。

每完成一个 Task 就运行该 Task 的精确测试并提交；每个 Milestone 结束再运行一次 `npm test && cargo test --manifest-path src-tauri/Cargo.toml`，不要把跨任务失败积压到最后。
