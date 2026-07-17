import { describe, expect, it } from "vitest";

import workflow from "../../.github/workflows/ci.yml?raw";

describe("release workflow", () => {
  it("fails closed unless both macOS jobs run on Apple Silicon", () => {
    expect(workflow.match(/name: Verify Apple Silicon runner/g)).toHaveLength(2);
    expect(workflow.match(/test "\$\(uname -m\)" = "arm64"/g)).toHaveLength(2);
  });

  it("verifies the standalone app and mounted DMG payload before upload", () => {
    expect(workflow).toContain("name: Verify Apple Silicon bundles");
    expect(workflow).toContain('lipo -archs "$bundle_app/Contents/MacOS/invoice-reimbursement"');
    expect(workflow).toContain('lipo -archs "$bundle_app/Contents/MacOS/invoice-ocr"');
    expect(workflow).toContain('hdiutil verify "${dmg_files[0]}"');
    expect(workflow).toContain('hdiutil attach "${dmg_files[0]}"');
    expect(workflow).toContain('lipo -archs "$mounted_app/Contents/MacOS/invoice-reimbursement"');
    expect(workflow).toContain('lipo -archs "$mounted_app/Contents/MacOS/invoice-ocr"');
  });
});
