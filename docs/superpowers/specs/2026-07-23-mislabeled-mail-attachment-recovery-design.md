# Mislabelled Mail Attachment Recovery Design

## Problem

Some invoice senders attach binary files while declaring every MIME part as `text/plain`.
`mail-parser` follows that declaration and converts decoded attachment bytes to UTF-8 text.
Invalid UTF-8 bytes are replaced, corrupting PDFs, images, and archives before they are saved.
The affected item then remains `recognition_failed`, and a later range scan returns the existing
record without replacing its corrupted original.

## Evidence

The affected QQ Mail message (UID 5454) declares the `.pdf` attachment as `text/plain` with
`Content-Transfer-Encoding: base64`. The stored file is 183,695 bytes and Poppler reports broken
Flate streams. Decoding the same base64 MIME body directly produces a valid 104,731-byte PDF whose
page renders normally.

## Design

Keep `mail-parser` as the source of MIME structure, headers, filenames, and normal text bodies.
For an attachment whose filename has a supported binary extension, do not trust a parsed text body.
Recover its body from the original message slice identified by the parser offsets and decode it
according to `Content-Transfer-Encoding`. Base64 and quoted-printable are decoded with bounded,
binary-safe decoders; unencoded bodies are copied unchanged. Correctly typed binary attachments
continue to use the parser's decoded contents.

During a date-range rescan, a newly decoded attachment may replace the existing record only when all
of these conditions hold:

- it is the same account, mailbox, UIDVALIDITY, UID, and MIME part;
- the existing bytes have a different SHA-256;
- recognition previously failed;
- confirmation is still pending;
- the item is not assigned to a batch.

Replacement retains the item ID and source identity, atomically swaps the original path and file
metadata, clears derived recognition fields and normalized output, recalculates duplicate status,
and returns the item as pending so the normal sync pipeline recognizes it immediately. Confirmed or
batch-assigned records are never overwritten.

## Recognition and Duplicate Handling

The recognized company is the seller that issued the invoice, not the buyer being reimbursed. The
recognizer supports labeled seller fields, parallel buyer/seller OCR columns, fragmented headings,
and the flattened tail layout produced by the affected April PDF. City is inferred from the seller
name first, then from explicit address and location context.

When a ZIP safely expands into one or more supported PDFs or images, only the expanded invoice files
are imported. The ZIP container is retained only when expansion fails or produces no recognizable
invoice file, so the same document is not imported once as an archive and again as its contents.

Files with different byte hashes can still be the same invoice, for example a PDF attachment, a URL
download, and a copy inside a ZIP. After successful recognition, the app builds a SHA-256 fingerprint
only when invoice number, seller tax identifier, amount in cents, and city are all available. The
database stores only this irreversible fingerprint. The first recognized item is canonical; a later
matching item is marked `suspected_duplicate`, links to the canonical item, and is excluded from
automatic batch assignment by the existing safety rules.

Semantic duplicate marking runs only when recognition began on an unconfirmed, unassigned item whose
dedupe state was still `unique`. A timestamp guard prevents a concurrent review or assignment from
being overwritten. Items already assigned to a batch, manually confirmed, or retained through the
duplicate-resolution action keep their existing state.

## Batch Workflow

The batch detail action `一键自动处理` remains the primary desktop workflow. It scans the selected
date range, recognizes new or repaired files, assigns safe confirmed items, and creates the export
package. Manual fallback remains `加入推荐票据` followed by `导出报销包`; export stays blocked while
assigned items are unconfirmed. Automation feedback must continue to show whether an export was
created and provide `在文件夹中显示` when it was.

## Failure Handling

Malformed base64 or quoted-printable in a supported binary attachment is rejected as a mailbox
parse error rather than saved as a corrupt file. Replacement uses staged and promoted files, rolls
back on database failure, and leaves startup recovery to clean an unreferenced old file if physical
deletion fails after a committed swap.

## Verification

Unit tests cover binary preservation for a base64 PDF mislabelled as text and rejection of malformed
transfer encoding. Import and repository tests cover recovery of a failed unassigned item plus guards
for confirmed and batch-assigned items. Recognition tests cover seller company/city extraction and
semantic dedupe between labeled and flattened OCR representations, including guards for confirmed,
batch-assigned, and explicitly retained items. Existing sync, recognition, batch automation, frontend,
lint, Rust formatting, Clippy, and production build checks must remain green. The repaired real PDF
must render normally and the April batch must complete through the desktop automation workflow.
