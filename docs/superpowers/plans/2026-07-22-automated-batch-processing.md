# Automated Batch Processing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户在 macOS 桌面 App 中创建月份或日期范围批次后，自动完成对应邮箱范围扫描、票据识别、安全审核、批次归属和报销包导出。

**Architecture:** 将日期范围作为显式 IMAP 查询参数，批次回扫使用临时 UID 游标且不改变日常增量游标。新增 Rust `BatchAutomationService` 作为单一业务编排入口，复用现有同步、识别、批次和事务化导出服务；React 只负责启动任务并呈现结构化结果。

**Tech Stack:** Tauri 2、Rust 2024、Tokio、SQLx/SQLite、IMAP、React 19、TypeScript、TanStack Query、Vitest、Playwright。

---

## File Map

- Modify: `src-tauri/src/infra/imap.rs` - 通用增量查询和日期范围 IMAP 查询。
- Modify: `src-tauri/src/services/sync.rs` - 提取 delta 处理并增加不写日常游标的范围扫描。
- Modify: `src-tauri/src/db/accounts.rs` - 记录范围同步成功但不更新 `sync_cursors`。
- Modify: `src-tauri/tests/sync_service.rs` - 范围分页、历史回扫和游标隔离测试。
- Modify: `src-tauri/src/services/recognition.rs` - 高置信自动确认时写入最终分类。
- Modify: `src-tauri/tests/recognition.rs` - 自动确认与最终分类持久化测试。
- Create: `src-tauri/src/services/batch_automation.rs` - 自动化审核、归属、导出和结果汇总。
- Create: `src-tauri/tests/batch_automation.rs` - 全自动编排集成测试。
- Modify: `src-tauri/src/services/mod.rs` - 导出自动化服务模块。
- Modify: `src-tauri/src/state.rs` - 复用同步服务并装配自动化服务和批次并发协调器。
- Modify: `src-tauri/src/commands/batches.rs` - 自动化结果 DTO 和业务适配器。
- Modify: `src-tauri/src/commands/mod.rs` - 注册 `run_batch_automation`。
- Modify: `src-tauri/tests/commands.rs` - 命令白名单、DTO 和 tracked operation 测试。
- Modify: `src/types.ts` - 自动化结果前端类型。
- Modify: `src/lib/api.ts` - 自动化命令 bridge。
- Modify: `src/lib/api.test.ts` - wire name 和参数测试。
- Modify: `src/features/batches/CreateBatchDialog.tsx` - 默认自动处理开关和创建后启动。
- Modify: `src/features/batches/BatchDetailPage.tsx` - 一键自动处理、运行态和结果摘要。
- Modify: `src/features/batches/BatchDetailPage.test.tsx` - 创建、运行、成功和失败 UI 测试。
- Modify: `src/features/batches/Batches.css` - 稳定的自动化状态布局。
- Modify: `package.json` - 版本更新为 `0.2.0`。
- Modify: `src-tauri/Cargo.toml` - 版本更新为 `0.2.0`。
- Modify: `src-tauri/tauri.conf.json` - 桌面包版本更新为 `0.2.0`。
- Modify: `docs/user-guide/invoice-reimbursement-user-manual.md` - 更新全自动批次说明。
- Modify: `docs/user-guide/invoice-reimbursement-quick-start.md` - 更新最短使用流程。

### Task 1: Generalize IMAP Queries for Historical Date Ranges

**Files:**
- Modify: `src-tauri/src/infra/imap.rs`
- Modify: `src-tauri/tests/sync_service.rs`
- Modify: `src-tauri/tests/commands.rs`
- Modify: `src-tauri/tests/settings_scheduler.rs`

- [ ] **Step 1: Write failing IMAP query tests**

Replace the fixed June/July assertion in `infra::imap::tests` with tests for a normal incremental query and explicit ranges:

```rust
#[test]
fn incremental_search_is_not_limited_to_a_hard_coded_month() {
    assert_eq!(uid_search_query(4073, None), "UID 4073:*");
}

#[test]
fn range_search_is_inclusive_at_start_and_exclusive_at_end() {
    let range = ImapDateRange::new(
        NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
    )
    .unwrap();
    assert_eq!(
        uid_search_query(1, Some(range)),
        "UID 1:* SINCE 1-May-2026 BEFORE 1-Jun-2026"
    );
}

#[test]
fn range_rejects_an_empty_or_reversed_window() {
    let day = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
    assert!(ImapDateRange::new(day, day).is_err());
    assert!(ImapDateRange::new(day, day.pred_opt().unwrap()).is_err());
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml infra::imap::tests::range_search -- --nocapture
```

Expected: compilation fails because `ImapDateRange` and `uid_search_query` do not exist.

- [ ] **Step 3: Add the range type and gateway method**

In `src-tauri/src/infra/imap.rs`, add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImapDateRange {
    pub start: NaiveDate,
    pub end_exclusive: NaiveDate,
}

impl ImapDateRange {
    pub fn new(start: NaiveDate, end_exclusive: NaiveDate) -> Result<Self, AppError> {
        if start >= end_exclusive {
            return Err(AppError::validation(
                "dateRange",
                "IMAP start date must be before end date",
            ));
        }
        Ok(Self { start, end_exclusive })
    }
}
```

Extend `ImapGateway`:

```rust
async fn fetch_range(
    &self,
    config: &ImapAccountConfig,
    secret: &str,
    cursor: Option<SyncCursor>,
    range: ImapDateRange,
) -> Result<MailboxDelta, AppError>;
```

Change the native implementation so both methods call one blocking function with `Option<ImapDateRange>`. Replace the fixed query with:

```rust
fn uid_search_query(start_uid: u32, range: Option<ImapDateRange>) -> String {
    let uid = format!("UID {start_uid}:*");
    match range {
        Some(range) => format!(
            "{uid} SINCE {} BEFORE {}",
            range.start.format("%-d-%b-%Y"),
            range.end_exclusive.format("%-d-%b-%Y")
        ),
        None => uid,
    }
}
```

- [ ] **Step 4: Update fake gateway implementations**

Every test `impl ImapGateway` must implement `fetch_range`. For gateways unrelated to range scanning, delegate to the same queued response or return the same stable result as `fetch_since`. Do not silently call a real network gateway.

- [ ] **Step 5: Run IMAP and compile-contract tests**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml infra::imap::tests -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --test sync_service --no-run
cargo test --manifest-path src-tauri/Cargo.toml --test commands --no-run
cargo test --manifest-path src-tauri/Cargo.toml --test settings_scheduler --no-run
```

Expected: all commands pass; no test still asserts the fixed 2026 June/July filter.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/infra/imap.rs src-tauri/tests/sync_service.rs \
  src-tauri/tests/commands.rs src-tauri/tests/settings_scheduler.rs
git commit -m "feat: support date ranged IMAP queries"
```

### Task 2: Add Range Sync Without Mutating the Daily Cursor

**Files:**
- Modify: `src-tauri/src/db/accounts.rs`
- Modify: `src-tauri/src/services/sync.rs`
- Modify: `src-tauri/tests/sync_service.rs`

- [ ] **Step 1: Write a failing historical range integration test**

Add a `RangeGateway` beside the existing `FakeImapGateway`. It records `(Option<SyncCursor>, ImapDateRange)` and pops queued deltas. Build the test with the same in-memory repository setup used by `incremental_sync_imports_each_mail_part_once`:

```rust
#[tokio::test]
async fn range_sync_pages_history_without_reading_or_overwriting_daily_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts.insert(NewMailboxAccount {
        provider: MailboxProvider::QQ,
        email: "history@example.com".to_owned(),
        imap_host: "imap.qq.com".to_owned(),
        imap_port: 993,
        enabled: true,
        sync_interval_minutes: 15,
    }).await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials.set(&account.id.to_string(), "auth-code").unwrap();
    accounts.upsert_cursor(
        account.id,
        "INBOX",
        SyncCursor { uid_validity: 9, last_uid: 900 },
    ).await.unwrap();
    let requested = Arc::new(Mutex::new(Vec::new()));
    let gateway = Arc::new(RangeGateway::new(
        requested.clone(),
        vec![
            MailboxDelta {
                rejected_messages: vec![],
                uid_validity: 9,
                highest_uid: 100,
                messages: vec![raw_message(
                    10,
                    include_bytes!("fixtures/mail/attachment.eml"),
                )],
            },
            MailboxDelta {
                rejected_messages: vec![],
                uid_validity: 9,
                highest_uid: 100,
                messages: vec![],
            },
        ],
    ));
    let service = SyncService::new(
        gateway,
        credentials,
        accounts.clone(),
        ImportService::new(
            items,
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(ItemRepository::new(pool), Arc::new(FakeExtractor)),
    );

    let result = service.run_range(
        account.id,
        NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        NaiveDate::from_ymd_opt(2026, 5, 31).unwrap(),
    ).await.unwrap();

    assert_eq!(result.imported_count, 1);
    assert_eq!(requested.lock().unwrap()[0].0, None);
    assert_eq!(requested.lock().unwrap()[0].1.start.to_string(), "2026-05-01");
    assert_eq!(requested.lock().unwrap()[0].1.end_exclusive.to_string(), "2026-06-01");
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor { uid_validity: 9, last_uid: 900 })
    );
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --test sync_service range_sync_pages_history_without_reading_or_overwriting_daily_cursor -- --nocapture
```

Expected: compilation fails because `SyncService::run_range` is absent.

- [ ] **Step 3: Add cursor-free sync completion persistence**

In `MailboxAccountRepository`, add a transactionally safe method:

```rust
pub async fn finish_range_sync_success(
    &self,
    run: &SyncRun,
    imported_count: u32,
) -> Result<(), AppError>
```

It must update the matching running `sync_runs` row to `succeeded`, set `finished_at` and `imported_count`, clear `last_error`/`last_error_at` on the account, and update `last_synced_at`. It must not insert or update `sync_cursors`. If the running row is missing, return the same stable conflict behavior as normal completion.

- [ ] **Step 4: Refactor delta processing and implement `run_range`**

Extract the import/recognition loops from `run_delta` into:

```rust
async fn process_delta(
    &self,
    account_id: Uuid,
    delta: &MailboxDelta,
    rescan: bool,
) -> Result<SyncResult, AppError>
```

Add:

```rust
pub async fn run_range(
    &self,
    account_id: Uuid,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> Result<SyncResult, AppError>
```

The method validates inclusive `start_date <= end_date`, computes `end_exclusive` with `succ_opt`, starts a sync run, loads account and credential, then repeatedly calls `fetch_range`. The first cursor is `None`; after each page it uses `(uid_validity, highest_uid)` as the temporary cursor. It stops only when the next temporary cursor equals the cursor sent to the gateway. Sum imported counts with checked arithmetic and call `finish_range_sync_success`. All failure messages must pass through the existing sanitization path.

- [ ] **Step 5: Add boundary and no-progress tests**

Add focused tests proving:

```rust
assert!(service.run_range(account_id, end, start).await.is_err());
assert!(service.run_range(account_id, NaiveDate::MAX, NaiveDate::MAX).await.is_err());
```

Also return the same `highest_uid` twice from a fake gateway and assert exactly two gateway calls, proving no infinite loop.

- [ ] **Step 6: Run range and existing incremental tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --test sync_service -- --nocapture
```

Expected: all sync tests pass, including existing cursor and UIDVALIDITY concurrency tests.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/db/accounts.rs src-tauri/src/services/sync.rs src-tauri/tests/sync_service.rs
git commit -m "feat: scan mailbox history by batch range"
```

### Task 3: Complete High-Confidence Automatic Review

**Files:**
- Modify: `src-tauri/src/services/recognition.rs`
- Modify: `src-tauri/tests/recognition.rs`

- [ ] **Step 1: Write the failing final-category test**

Add an integration test using the existing `sample_item` and `FakeExtractor` helpers:

```rust
#[tokio::test]
async fn confident_recognition_confirms_and_copies_the_suggested_category_to_final() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let record = sample_item(Uuid::new_v4());
    repository.insert(&record).await.unwrap();
    let extractor = Arc::new(FakeExtractor::new(vec![Ok(ExtractedDocument {
        text: "发票日期：2026-05-08\n价税合计：¥128.50\n出租车 客运服务".to_owned(),
        normalized_pdf: None,
        warnings: vec![],
    })]));
    let service = RecognitionService::new(repository, extractor);

    let recognized = service.recognize_item(record.id).await.unwrap();

    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(recognized.suggested_category, Some(Category::Transport));
    assert_eq!(recognized.final_category, Some(Category::Transport));
}
```

- [ ] **Step 2: Verify RED**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --test recognition confident_recognition_confirms_and_copies_the_suggested_category_to_final -- --nocapture
```

Expected: FAIL because `final_category` remains `None`.

- [ ] **Step 3: Persist final category only for automatic confirmation**

Before building the recognition patch:

```rust
let automatic_final_category =
    (recognized.confirmation_status == ConfirmationStatus::Confirmed)
        .then_some(recognized.category)
        .flatten();
```

Add `final_category: Some(automatic_final_category)` to the patch. Existing guarded recognition updates must continue preserving a previously manually confirmed final category.

- [ ] **Step 4: Add the warning regression assertion**

Extend the extraction warning test to assert:

```rust
assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
assert_eq!(recognized.final_category, None);
```

- [ ] **Step 5: Run recognition and review suites**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --test recognition -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --test item_review -- --nocapture
```

Expected: all tests pass and manual reviews are still protected from in-flight OCR.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/services/recognition.rs src-tauri/tests/recognition.rs
git commit -m "feat: finalize high confidence recognition"
```

### Task 4: Implement the Batch Automation Orchestrator

**Files:**
- Create: `src-tauri/src/services/batch_automation.rs`
- Create: `src-tauri/tests/batch_automation.rs`
- Modify: `src-tauri/src/services/mod.rs`
- Modify: `src-tauri/src/state.rs`

- [ ] **Step 1: Write failing end-to-end service tests**

Create `src-tauri/tests/batch_automation.rs` with fixtures for two enabled accounts, a range-capable fake gateway, a deterministic extractor, temporary AppPaths and an in-memory database. The primary test must assert:

```rust
let result = service.run(batch.id).await.unwrap();
assert_eq!(result.scanned_account_count, 2);
assert_eq!(result.failed_accounts, vec![]);
assert_eq!(result.imported_count, 3);
assert_eq!(result.assigned_count, 1);
assert_eq!(result.exception_count, 2);
let export = result.export.expect("one safe invoice must be exported");
assert_eq!(export.item_count, 1);
assert!(export.directory.join("merged.pdf").is_file());
assert!(export.directory.join("reimbursement.xlsx").is_file());
assert!(export.directory.join("originals.zip").is_file());
assert!(export.directory.join("manifest.json").is_file());
```

The three imported fixtures must be: one complete unique invoice, one incomplete invoice, and one duplicate. Add separate tests for one account failure with another succeeding, no enabled accounts, concurrent runs for the same batch, and rerunning without duplicate rows.

- [ ] **Step 2: Verify RED**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --test batch_automation --no-run
```

Expected: compilation fails because the module and service do not exist.

- [ ] **Step 3: Define automation result types and coordinator**

Create `src-tauri/src/services/batch_automation.rs` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountAutomationFailure {
    pub account_id: Uuid,
    pub email: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchAutomationResult {
    pub scanned_account_count: u32,
    pub failed_accounts: Vec<AccountAutomationFailure>,
    pub imported_count: u32,
    pub assigned_count: u32,
    pub exception_count: u32,
    pub export: Option<ExportResult>,
}

#[derive(Clone, Default)]
pub struct BatchAutomationCoordinator {
    active: Arc<DashMap<Uuid, ()>>,
}
```

Acquire the batch ID with a DashMap entry before work and remove it through an RAII guard on every exit path. A second run returns `AppError::Conflict` without starting any account sync.

- [ ] **Step 4: Implement the service sequence**

Construct `BatchAutomationService` with the database pool, `SyncService`, `BatchService`, `ExportService`, `AccountOperationCoordinator`, and `BatchAutomationCoordinator`.

`run(batch_id)` must:

1. Load the batch and reject ranges longer than 366 inclusive days.
2. List enabled accounts.
3. For each account, acquire its existing operation lock, call `run_range`, add imported counts, and collect a sanitized failure instead of stopping other accounts.
4. Page `BatchService::list_candidates` with `page_size = 200` until `next_cursor = None`.
5. Classify a candidate as safe only when it is `ItemStatus::Ready`, has invoice date in range, `final_category`, amount, suggested period and normalized PDF, and is not a suspected duplicate.
6. Call `assign_items` once with the safe IDs.
7. Count all remaining candidates as exceptions using checked conversions.
8. Export when the resulting batch contains at least one item; otherwise return `export: None`.

Do not resolve duplicates, invent missing fields, move items from other batches, or mutate the daily sync cursor.

- [ ] **Step 5: Wire the service through AppState**

Store the constructed `SyncService` and a shared `BatchAutomationCoordinator` in `AppState`. Add:

```rust
pub fn batch_automation_service(&self) -> BatchAutomationService
```

It must reuse `self.export_service()` so automatic and manual exports share the same `ExportCoordinator` and preference gate.

- [ ] **Step 6: Run automation and shutdown tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --test batch_automation -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --test shutdown -- --nocapture
```

Expected: all tests pass; automatic export is transactionally identical to manual export.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/services/batch_automation.rs src-tauri/src/services/mod.rs \
  src-tauri/src/state.rs src-tauri/tests/batch_automation.rs
git commit -m "feat: automate batch review and export"
```

### Task 5: Expose One Desktop Command and Typed Frontend API

**Files:**
- Modify: `src-tauri/src/commands/batches.rs`
- Modify: `src-tauri/src/commands/mod.rs`
- Modify: `src-tauri/tests/commands.rs`
- Modify: `src/types.ts`
- Modify: `src/lib/api.ts`
- Modify: `src/lib/api.test.ts`

- [ ] **Step 1: Add failing API bridge test**

Add this command case in `src/lib/api.test.ts`:

```typescript
{
  method: "runBatchAutomation",
  arguments_: ["batch-1"],
  command: "run_batch_automation",
  invokeArguments: { batchId: "batch-1" },
}
```

Extend the Rust command whitelist test to expect `run_batch_automation`.

- [ ] **Step 2: Verify RED on both sides**

```bash
npm test -- src/lib/api.test.ts
cargo test --manifest-path src-tauri/Cargo.toml --test commands desktop_api_exposes_only_the_planned_command_names -- --nocapture
```

Expected: both fail because the command is missing.

- [ ] **Step 3: Add Rust DTO and tracked command adapter**

In `commands/batches.rs`, add camelCase DTOs:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountAutomationFailureDto {
    pub account_id: String,
    pub email: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchAutomationResultDto {
    pub scanned_account_count: u32,
    pub failed_accounts: Vec<AccountAutomationFailureDto>,
    pub imported_count: u32,
    pub assigned_count: u32,
    pub exception_count: u32,
    pub export: Option<ExportResultDto>,
}
```

Add `run_automation(state, batch_id)` and `ipc::run_batch_automation`. The adapter must obtain `state.batch_automation_service()` and execute it inside `state.run_tracked_operation` before DTO conversion.

- [ ] **Step 4: Register the command**

Add `run_batch_automation` to `COMMAND_NAMES` and `tauri::generate_handler!` in `commands/mod.rs`.

- [ ] **Step 5: Add TypeScript types and bridge**

In `src/types.ts`:

```typescript
export interface AccountAutomationFailureDto {
  accountId: string;
  email: string;
  message: string;
}

export interface BatchAutomationResultDto {
  scannedAccountCount: number;
  failedAccounts: AccountAutomationFailureDto[];
  importedCount: number;
  assignedCount: number;
  exceptionCount: number;
  export: ExportResultDto | null;
}
```

Add the exact command name and method in `src/lib/api.ts`:

```typescript
runBatchAutomation: "run_batch_automation",
// ...
runBatchAutomation: (batchId: string) =>
  call<BatchAutomationResultDto>(API_COMMANDS.runBatchAutomation, { batchId }),
```

- [ ] **Step 6: Run bridge and command tests**

```bash
npm test -- src/lib/api.test.ts
cargo test --manifest-path src-tauri/Cargo.toml --test commands -- --nocapture
```

Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/commands/batches.rs src-tauri/src/commands/mod.rs \
  src-tauri/tests/commands.rs src/types.ts src/lib/api.ts src/lib/api.test.ts
git commit -m "feat: expose batch automation command"
```

### Task 6: Build the Full-Automation Desktop Workflow

**Files:**
- Modify: `src/features/batches/CreateBatchDialog.tsx`
- Modify: `src/features/batches/BatchDetailPage.tsx`
- Modify: `src/features/batches/BatchDetailPage.test.tsx`
- Modify: `src/features/batches/Batches.css`

- [ ] **Step 1: Write failing create-and-run UI tests**

In `BatchDetailPage.test.tsx`, mock `run_batch_automation` and add:

```typescript
it("automatically processes and exports a newly created monthly batch", async () => {
  const user = userEvent.setup();
  const created = batchFixture({
    id: "batch-may",
    name: "2026 年 5 月报销",
    startDate: "2026-05-01",
    endDate: "2026-05-31",
  });
  mockCommand("get_dashboard", new Promise(() => undefined));
  mockCommand("create_month_batch", created);
  mockCommand("get_batch", { ...detailFixture(), batch: created });
  mockCommand("run_batch_automation", {
    scannedAccountCount: 1,
    failedAccounts: [],
    importedCount: 1,
    assignedCount: 1,
    exceptionCount: 0,
    export: {
      directory: "/Users/finance/2026-5",
      itemCount: 1,
      totalAmountCents: 12_850,
    },
  });
  renderAppAt("/batches/new");

  expect(screen.getByRole("checkbox", { name: "创建后自动处理并导出" })).toBeChecked();
  await user.selectOptions(screen.getByLabelText("月份"), "5");
  await user.click(screen.getByRole("button", { name: "创建并自动处理" }));

  expect(await screen.findByText("自动处理完成")).toBeInTheDocument();
  expect(commandCalls("run_batch_automation")).toEqual([{ batchId: "batch-may" }]);
  expect(screen.getByText("1 张已自动纳入")).toBeInTheDocument();
});
```

Add a second test that unchecks the toggle, expects button text `创建批次`, and asserts no automation command call.

- [ ] **Step 2: Verify RED**

```bash
npm test -- src/features/batches/BatchDetailPage.test.tsx
```

Expected: tests fail because the checkbox and automatic command do not exist.

- [ ] **Step 3: Add default-on create behavior**

In `CreateBatchDialog.tsx`, track `automate` defaulting to `true`. After the create command resolves, navigate to `/batches/<id>` with router state `{ runAutomation: true }`. When disabled, use the original navigation without that state.

Use a semantic checkbox/toggle labeled exactly `创建后自动处理并导出`; primary button text is `创建并自动处理` when enabled and `创建批次` when disabled. The create request itself must not be repeated if automation later fails.

- [ ] **Step 4: Add failing result and retry tests**

Add tests for:

```typescript
expect(screen.getByRole("button", { name: "一键自动处理" })).toBeEnabled();
expect(screen.getByRole("status", { name: "正在自动处理批次" })).toBeInTheDocument();
expect(screen.getByText("2 个异常项")).toBeInTheDocument();
expect(screen.getByText("1 个邮箱失败")).toBeInTheDocument();
```

Reject the first automation call and resolve the retry; assert the batch page remains mounted, shows the error, and a retry invokes the command once more.

- [ ] **Step 5: Implement the batch-detail automation state**

Use a TanStack mutation in `BatchDetailPage.tsx`. On page load, consume the router state once with `navigate(location.pathname, { replace: true, state: null })`, then start the mutation. On success invalidate dashboard, items, batches and current batch queries.

Render an unframed `.batch-automation-strip` with fixed grid tracks for:

- idle: concise description and `一键自动处理` button;
- pending: loader/status text and disabled button;
- success: imported, assigned, exception and failed-account counts plus export directory when present;
- error: sanitized error and `重试自动处理` button.

Do not place a card inside the existing batch detail panel. Long export paths must wrap with `overflow-wrap: anywhere`.

- [ ] **Step 6: Add responsive styles and run UI tests**

```bash
npm test -- src/features/batches/BatchDetailPage.test.tsx
npm run lint
npm run build
```

Expected: all pass with no TypeScript or ESLint warnings.

- [ ] **Step 7: Commit**

```bash
git add src/features/batches/CreateBatchDialog.tsx \
  src/features/batches/BatchDetailPage.tsx \
  src/features/batches/BatchDetailPage.test.tsx \
  src/features/batches/Batches.css
git commit -m "feat: add one-click automated batch workflow"
```

### Task 7: Update Version, Guides, Visual QA, and Release Build

**Files:**
- Modify: `package.json`
- Modify: `package-lock.json`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `docs/user-guide/invoice-reimbursement-user-manual.md`
- Modify: `docs/user-guide/invoice-reimbursement-quick-start.md`
- Modify: `tests/e2e/mvp-flow.spec.ts`

- [ ] **Step 1: Add an E2E scenario before updating implementation metadata**

Extend the development command bridge fixture so a new May batch returns a successful automation result. Add a Playwright scenario that creates a May batch, observes the pending state, then verifies `自动处理完成`, the export summary, and the absence of horizontal scrolling.

- [ ] **Step 2: Run E2E and verify the new scenario fails if fixture support is absent**

```bash
npm run test:e2e -- --grep "automated May batch"
```

Expected before fixture completion: FAIL because the command or completion summary is missing; after adding the deterministic bridge result: PASS.

- [ ] **Step 3: Update user guides**

Replace the old mandatory manual-confirmation workflow with:

1. 设置并启用邮箱。
2. 创建月份批次并保留“创建后自动处理并导出”。
3. 等待完成摘要并打开导出目录。
4. 仅在异常计数不为零或成果不正确时进入待处理池人工修正后重跑。

Explicitly state that suspicious duplicates and incomplete OCR results are excluded rather than silently accepted.

- [ ] **Step 4: Bump all application versions to 0.2.0**

Run:

```bash
npm version 0.2.0 --no-git-tag-version
cargo set-version --manifest-path src-tauri/Cargo.toml 0.2.0
```

If `cargo set-version` is unavailable, edit only the package version in `src-tauri/Cargo.toml` with `apply_patch`, then run `cargo check` to update `Cargo.lock`. Update `src-tauri/tauri.conf.json` to `0.2.0`.

- [ ] **Step 5: Run complete static and automated verification**

```bash
npm test
npm run lint
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npm run test:e2e
```

Expected: all tests pass; only the explicitly environment-gated packaged OCR test may remain ignored.

- [ ] **Step 6: Perform multi-viewport visual verification**

Start the dev server on an unused port and use Playwright screenshots at `800x700`, `1440x900`, and `1728x1117`. Inspect creation, pending, success-with-exceptions, and failure/retry states. Assert `document.documentElement.scrollWidth <= window.innerWidth` and verify no text overlaps, truncated controls, blank panels, or unreadable status colors.

- [ ] **Step 7: Build and inspect the Apple Silicon DMG**

```bash
npm run tauri build -- --target aarch64-apple-darwin
```

Mount the generated DMG and verify:

```bash
test -x "/Volumes/发票报销/发票报销.app/Contents/MacOS/invoice-ocr"
lipo -archs "/Volumes/发票报销/发票报销.app/Contents/MacOS/invoice-ocr"
```

Expected: OCR sidecar exists and reports `arm64`. Launch the mounted App once and verify the main window renders.

- [ ] **Step 8: Commit the release-ready feature**

```bash
git add package.json package-lock.json src-tauri/Cargo.toml src-tauri/Cargo.lock \
  src-tauri/tauri.conf.json docs/user-guide tests/e2e
git commit -m "release: prepare automated batch processing v0.2.0"
```

- [ ] **Step 9: Request code review and finish the branch**

Use `superpowers:requesting-code-review`, address verified findings, rerun the complete verification from Step 5, then use `superpowers:finishing-a-development-branch`. Do not tag or publish a GitHub Release until the verified branch has been integrated into `main`.
