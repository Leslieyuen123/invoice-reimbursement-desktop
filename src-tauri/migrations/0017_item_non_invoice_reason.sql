-- Marks an invoice item as "not an invoice file" (a Word/Excel/archive original
-- that can never be recognized or exported).
--
-- Such files are imported on purpose (a mail may carry a reimbursement form
-- next to the invoice), but they used to sit in the work queue forever and made
-- the pending/failed counters look far worse than the real backlog. The mark is
-- persistent, keeps the row visible in the "全部" list, and only removes it from
-- the queue views, their counters and the bulk-confirm skip statistics.
ALTER TABLE items ADD COLUMN non_invoice_reason TEXT;

CREATE INDEX idx_items_non_invoice_reason ON items (non_invoice_reason);
