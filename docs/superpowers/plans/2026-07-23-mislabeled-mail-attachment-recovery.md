# Mislabelled Mail Attachment Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve binary invoice attachments whose MIME type is incorrectly declared as text, repair already-corrupted records, recognize seller metadata, and collapse duplicate invoice representations before batch automation.

**Architecture:** `services::sync` continues to use `mail-parser` for MIME structure but recovers supported binary attachment bodies from original message offsets when the parsed body is textual. `ImportService` stages the corrected bytes and asks `ItemRepository` to atomically replace only a failed, unconfirmed, unassigned email item before recognition runs again. Recognition extracts seller identity and city, then stores an irreversible semantic fingerprint so different file formats of the same invoice share the existing duplicate workflow.

**Tech Stack:** Rust 2024, `mail-parser`, `base64`, `sqlx`/SQLite, Tokio, Tauri 2, Vitest/React Testing Library.

---

### Task 1: Preserve Mislabelled Binary MIME Parts

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/services/sync.rs`
- Test: `src-tauri/src/services/sync/tests.rs`

- [x] **Step 1: Write a failing parser regression test**

Construct an RFC 822 message with `Content-Type: text/plain`, a `.pdf` filename, base64 transfer
encoding, and PDF bytes containing invalid UTF-8. Assert that `parse_invoice_parts` returns exactly
the original byte vector.

- [x] **Step 2: Verify the test fails for the corruption symptom**

Run: `cargo test --manifest-path src-tauri/Cargo.toml services::sync::tests::mislabeled_text_pdf_preserves_binary_bytes -- --exact`

Expected: FAIL because the returned bytes contain UTF-8 replacement sequences and differ from the
input PDF bytes.

- [x] **Step 3: Implement bounded raw-body recovery**

Add a focused helper that accepts the original message, `MessagePart`, and filename. If a supported
binary extension was parsed as `PartType::Text` or `PartType::Html`, slice
`raw.raw[part.offset_body..part.offset_end]` and decode `Encoding::Base64`,
`Encoding::QuotedPrintable`, or copy `Encoding::None`. Return a mailbox parsing error for invalid
offsets or malformed transfer encoding. Keep `part.contents()` for real binary parts.

- [x] **Step 4: Verify parser tests pass**

Run: `cargo test --manifest-path src-tauri/Cargo.toml services::sync::tests`

Expected: all sync unit tests pass.

### Task 2: Recover Failed Existing Email Attachments

**Files:**
- Modify: `src-tauri/src/db/items.rs`
- Modify: `src-tauri/src/services/import.rs`
- Test: `src-tauri/src/services/import.rs`

- [x] **Step 1: Write failing recovery and guard tests**

Import corrupted bytes for a rescan source, mark the item failed, then import corrected bytes for the
same source with `rescan: true`. Assert the same item ID now points to corrected bytes and returns to
pending. Add cases proving confirmed and batch-assigned items retain their existing file.

- [x] **Step 2: Verify the recovery test fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml services::import::tests::rescan_replaces_failed_unreviewed_email_attachment -- --exact`

Expected: FAIL because `import_email_payload` currently returns the existing corrupt record before
hashing the corrected payload.

- [x] **Step 3: Implement guarded atomic replacement**

Generalize replacement metadata in `db/items.rs` and add a transaction guarded by
`recognition_status = failed`, `confirmation_status = pending`, and `batch_id IS NULL`. Recalculate
dedupe status, reset derived fields, and atomically update the row after the corrected staged file is
promoted. In `ImportService`, attempt this path only during rescan and only when the hashes differ;
delete the old managed original and normalized PDF after a committed replacement.

- [x] **Step 4: Verify recovery and import tests pass**

Run: `cargo test --manifest-path src-tauri/Cargo.toml services::import::tests`

Expected: all import service tests pass with no staging residue.

### Task 3: Deduplicate Multiple Invoice Representations

**Files:**
- Modify: `src-tauri/src/services/sync.rs`
- Modify: `src-tauri/src/services/recognition.rs`
- Modify: `src-tauri/src/db/items.rs`
- Create: `src-tauri/migrations/0010_item_semantic_identities.sql`
- Test: `src-tauri/src/services/sync/tests.rs`
- Test: `src-tauri/tests/recognition.rs`

- [x] **Step 1: Avoid importing successful ZIP containers alongside their contents**

Keep only expanded PDFs/images after successful safe extraction. Retain the ZIP as an exception
material when extraction fails or produces no supported invoice.

- [x] **Step 2: Write and observe a failing cross-format semantic dedupe test**

Recognize two byte-distinct records whose labeled and flattened OCR forms share invoice number,
seller tax identifier, amount, and city. Verify the later item initially remains incorrectly unique.

- [x] **Step 3: Persist an irreversible semantic identity and reuse duplicate state**

Hash the four normalized identity fields with SHA-256, store only the fingerprint, and atomically mark
the later safe item `suspected_duplicate` with `duplicate_of_id` pointing to the canonical item.

- [x] **Step 4: Protect reviewed workflow state**

Add regression cases proving semantic dedupe does not override a manually confirmed item, a
batch-assigned item, or an item the user explicitly retained from duplicate review.

### Task 4: Validate the Existing Batch Export Workflow

**Files:**
- Test: `src-tauri/tests/batch_automation.rs`
- Test: `src/features/batches/BatchDetailPage.test.tsx`
- Modify only if a gap is found: `src/features/batches/BatchDetailPage.tsx`

- [x] **Step 1: Run the focused automation tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test batch_automation`

Expected: safe recognized items are assigned and an export is produced; exception items remain
unassigned.

- [x] **Step 2: Run the focused batch UI tests**

Run: `npm test -- --run src/features/batches/BatchDetailPage.test.tsx`

Expected: `一键自动处理` exposes completed counts and export location, while manual export remains
disabled for unconfirmed assigned items.

- [x] **Step 3: Add a failing UI test only if the next action is absent**

If automation completion can produce an export without an actionable reveal control, add a test for
`在文件夹中显示`, observe failure, then make the smallest matching UI change. Do not add a second
workflow or instructional wizard.

### Task 5: Full Verification and Local Repair

**Files:**
- Update: `Cargo.lock` if a direct decoding dependency is added
- Runtime data: the installed app's failed April attachment, through normal app rescan only

- [x] **Step 1: Run repository verification**

Run: `npm test -- --run && npm run lint && npm run build`

Run: `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`

Run: `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings`

Run: `cargo test --manifest-path src-tauri/Cargo.toml --all-targets --all-features`

Expected: every command exits 0 with no test failures or lint errors.

- [x] **Step 2: Build and install the macOS app**

Run the repository's documented macOS bundle command, replace `/Applications/发票报销.app` with the
new verified bundle, and relaunch it.

- [x] **Step 3: Repair and verify April through the normal workflow**

Open `2026 年 4 月报销`, run `一键自动处理`, verify the affected item retains its ID but now has a
valid normalized PDF, successful recognition fields, and an April suggested period. Confirm the batch
assigns safe items and presents the generated export directory.

- [x] **Step 4: Render the repaired PDF**

Run: `pdftoppm -f 1 -l 1 -png -r 150 <repaired-original.pdf> tmp/pdfs/repaired-april`

Expected: the full invoice page is legible and Poppler reports no Flate-stream errors.
