import { describe, expect, it } from "vitest";
import { isSystemNote, systemNote } from "./systemNote";

describe("systemNote", () => {
  it("explains a recognition failure without the internal prefix", () => {
    expect(systemNote("识别失败：原件格式 .docx 不支持自动识别")).toBe(
      "原件格式 .docx 不支持自动识别",
    );
  });

  it("explains a failed link download", () => {
    expect(systemNote("invoice link download failed")).toContain("手动下载");
  });

  it("explains a rejected mailbox message", () => {
    expect(systemNote("mailbox message rejected: 550 size")).toContain("550 size");
  });

  it("treats a user remark as a remark", () => {
    expect(systemNote("张三的餐费")).toBeNull();
    expect(isSystemNote("张三的餐费")).toBe(false);
  });

  it("ignores empty and missing notes", () => {
    expect(systemNote(null)).toBeNull();
    expect(systemNote("   ")).toBeNull();
    expect(systemNote(undefined)).toBeNull();
  });
});
