# HTTPS Invoice Link Download Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Download HTTPS invoice links from email bodies and feed verified PDF, image, and ZIP originals into the existing automated reimbursement pipeline.

**Architecture:** Add an injected secure downloader at the infrastructure boundary, keep IMAP parsing and OCR independent, and let `ImportService` replace legacy `.url` placeholders in place. The production downloader follows pinned public HTTPS targets, understands the observed Nuonuo short-link flow, and rejects or ignores non-invoice content before persistence.

**Tech Stack:** Rust 2024, Tokio, reqwest/rustls, serde, SQLx/SQLite, existing Tauri services, Vitest, Playwright.

---

### Task 1: Secure invoice downloader contract and classification

**Files:**
- Create: `src-tauri/src/infra/invoice_download.rs`
- Modify: `src-tauri/src/infra/mod.rs`
- Modify: `src-tauri/Cargo.toml`

- [ ] **Step 1: Write failing tests for URL policy and file signatures**

Add unit tests that require HTTPS, reject local/private/reserved IPv4 and IPv6 targets, accept public addresses, identify PDF/JPEG/PNG/ZIP by magic bytes, reject HTML/XML, and classify `fp.nuonuo.com/#/` plus `.xml` paths as ignored.

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml invoice_download -- --nocapture`

Expected: FAIL because `infra::invoice_download` and its policy functions do not exist.

- [ ] **Step 3: Implement the contract and pure validation helpers**

Define:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedInvoice {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvoiceLinkDownload {
    Downloaded(DownloadedInvoice),
    Ignored,
}

#[async_trait]
pub trait InvoiceLinkDownloader: Send + Sync {
    async fn download(&self, source_url: &str) -> Result<InvoiceLinkDownload, AppError>;
}
```

Add the dependencies `reqwest` with rustls and streaming support, and Tokio `net`. Keep the 50 MiB limit aligned with `services::import::MAX_FILE_SIZE`.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test --manifest-path src-tauri/Cargo.toml invoice_download -- --nocapture`

Expected: all invoice download policy and classification tests PASS.

### Task 2: Public HTTPS fetching, redirects, and Nuonuo resolution

**Files:**
- Modify: `src-tauri/src/infra/invoice_download.rs`

- [ ] **Step 1: Write failing tests for response limits and Nuonuo parsing**

Add tests for rejecting oversized `Content-Length`, rejecting a streamed body beyond 50 MiB, extracting only the whitelisted `printQrcode` query fields, accepting a typed Nuonuo `status=0000` response with a PDF URL, and rejecting missing/failed detail responses without copying provider messages or signed URLs into `AppError`.

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml invoice_download -- --nocapture`

Expected: FAIL at the new response and Nuonuo assertions.

- [ ] **Step 3: Implement the production downloader**

Implement `SecureInvoiceLinkDownloader` with manual redirect handling, DNS resolution and public-IP validation, reqwest `resolve_to_addrs` pinning, 10-second connection timeout, 30-second request timeout, five redirects, bounded streamed reads, typed Nuonuo JSON, and a second secure fetch for the returned PDF URL. Do not log query strings or provider response bodies.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test --manifest-path src-tauri/Cargo.toml invoice_download -- --nocapture`

Expected: all downloader tests PASS.

### Task 3: Replace and discard legacy link placeholders safely

**Files:**
- Modify: `src-tauri/src/db/items.rs`
- Modify: `src-tauri/src/services/import.rs`

- [ ] **Step 1: Write failing import tests**

Add tests that first persist a legacy `.url` item and then require a downloaded PDF to reuse its item ID, replace its original path/name/hash/MIME, reset recognition to pending, preserve email source identity, calculate duplicate state against other items, delete the old URL file, and leave no staging residue. Add a separate test requiring an ignored unassigned placeholder to be removed while a manually confirmed or batch-assigned item is preserved.

- [ ] **Step 2: Run the import tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml services::import -- --nocapture`

Expected: FAIL because placeholder replacement and discard APIs do not exist.

- [ ] **Step 3: Implement repository transactions and import APIs**

Add narrow repository operations that only mutate email-backed `text/uri-list` `.url` items which are still pending confirmation and unassigned. In `ImportService`, expose existing-part lookup, downloaded-link import, failure placeholder import/update, and ignored-placeholder discard. Promote the verified file before committing its database reference, roll it back on database failure, and durably remove the replaced URL file after commit.

- [ ] **Step 4: Run import tests and verify GREEN**

Run: `cargo test --manifest-path src-tauri/Cargo.toml services::import -- --nocapture`

Expected: placeholder replacement, duplicate calculation, discard guard, cleanup, and existing import tests PASS.

### Task 4: Connect downloads to email sync and recognition

**Files:**
- Modify: `src-tauri/src/services/sync.rs`
- Modify: `src-tauri/src/services/sync/tests.rs`
- Modify: `src-tauri/tests/sync_service.rs`
- Modify: `src-tauri/src/state.rs`

- [ ] **Step 1: Replace the placeholder integration test with failing download tests**

Inject a deterministic fake `InvoiceLinkDownloader` and assert:

```rust
assert_eq!(downloaded.original_name, "invoice-103.pdf");
assert_eq!(downloaded.mime_type, "application/pdf");
assert_eq!(downloaded.recognition_status, RecognitionStatus::Succeeded);
assert!(!downloaded.original_path.ends_with(".url"));
```

Cover a downloaded ZIP with recognizable entries, an isolated retryable failure, a later successful replacement using the same item ID, an ignored XML/home-page link, and a second range scan that performs no HTTP request after a successful import.

- [ ] **Step 2: Run sync tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test sync_service -- --nocapture`

Expected: FAIL because sync still imports URL placeholders without invoking a downloader.

- [ ] **Step 3: Implement sync orchestration**

Give `SyncService::new` a production `SecureInvoiceLinkDownloader` and add a test-only/general `with_link_downloader` builder. Refactor repeated file import/recognition handling into a focused helper. For each link, skip network when a real source part already exists, retry legacy/failed placeholders, expand downloaded ZIPs with the existing aggregate budget, isolate downloader errors into failed placeholders, and discard ignored legacy placeholders.

- [ ] **Step 4: Run sync tests and verify GREEN**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test sync_service -- --nocapture`

Expected: all link download, retry, ignore, ZIP, idempotency, cursor, attachment, and range-sync tests PASS.

### Task 5: Full verification and release artifact

**Files:**
- Modify: `package.json`
- Modify: `package-lock.json`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `README.md`
- Modify: `docs/USER_MANUAL.md`

- [ ] **Step 1: Run every quality gate**

Run:

```bash
npm test
npm run lint
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npm run test:e2e
```

Expected: 0 failures and 0 lint/clippy warnings.

- [ ] **Step 2: Update version and documentation**

Set the npm, Cargo, and Tauri version to `0.2.1`. Document direct HTTPS PDF/image/ZIP support, Nuonuo short links, safe retry behavior, and the fact that authentication-only or unsupported scripted portals remain visible exceptions rather than blocking the batch.

- [ ] **Step 3: Build and inspect the Apple Silicon package**

Run: `npm run tauri -- build --target aarch64-apple-darwin`

Expected: an `.app` and `.dmg` under `src-tauri/target/aarch64-apple-darwin/release/bundle/`, with the OCR sidecar present.

Mount the DMG, verify the application and sidecar, run `codesign --verify --deep --strict`, launch the mounted app for a smoke check, unmount it, and calculate SHA-256.

- [ ] **Step 4: Commit and publish**

Commit only the feature, tests, docs, version files, and lockfiles. Push `main`, wait for GitHub Actions to pass, tag `v0.2.1`, and publish the arm64 DMG plus checksum in the GitHub Release.

