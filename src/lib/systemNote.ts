/**
 * The backend stores machine reasons in the same `note` column the item drawer
 * exposes as a free-text remark ("备注"). This module is the single place that
 * recognises those reasons, so the UI can show them as explanations instead of
 * pretending the user wrote them.
 */
const RECOGNITION_PREFIX = "识别失败：";
const LINK_FAILURE_NOTE = "invoice link download failed";
const REJECTED_PREFIX = "mailbox message rejected:";

export function systemNote(note: string | null | undefined): string | null {
  const text = note?.trim();
  if (!text) {
    return null;
  }
  if (text.startsWith(RECOGNITION_PREFIX)) {
    return text.slice(RECOGNITION_PREFIX.length).trim();
  }
  if (text === LINK_FAILURE_NOTE) {
    return "邮件里的下载链接没能取到 PDF，请打开邮件手动下载后重新导入这张票。";
  }
  if (text.startsWith(REJECTED_PREFIX)) {
    const reason = text.slice(REJECTED_PREFIX.length).trim();
    return reason
      ? `邮箱没有接收这封邮件（${reason}），请检查发件人或附件大小。`
      : "邮箱没有接收这封邮件，请检查发件人或附件大小。";
  }
  return null;
}

export function isSystemNote(note: string | null | undefined): boolean {
  return systemNote(note) !== null;
}
