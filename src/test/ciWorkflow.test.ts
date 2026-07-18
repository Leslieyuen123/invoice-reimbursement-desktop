import { describe, expect, it } from "vitest";

import workflow from "../../.github/workflows/ci.yml?raw";
import tauriConfig from "../../src-tauri/tauri.conf.json?raw";
import recoveryGuide from "../../docs/operations/local-data-and-recovery.md?raw";
import playwrightConfig from "../../playwright.config.ts?raw";
import e2eFlow from "../../tests/e2e/mvp-flow.spec.ts?raw";

describe("release workflow", () => {
  it("fails closed unless both macOS jobs run on Apple Silicon", () => {
    expect(workflow.match(/runs-on: macos-15/g)).toHaveLength(2);
    expect(workflow).not.toContain("runs-on: macos-14");
    expect(workflow.match(/name: Verify Apple Silicon runner/g)).toHaveLength(2);
    expect(workflow.match(/test "\$\(uname -m\)" = "arm64"/g)).toHaveLength(2);
  });

  it("scans the production frontend for browser bridge markers", () => {
    expect(workflow).toContain("name: Reject browser bridge code in production");
    for (const marker of [
      "VITE_BROWSER_COMMAND_BRIDGE",
      "__INVOICE_COMMAND_BRIDGE__",
      "Simulated browser command failure",
    ]) {
      expect(workflow).toContain(marker);
    }
  });

  it("checks the packaged deployment target", () => {
    expect(workflow).toContain('otool -l "$bundle_app/Contents/MacOS/invoice-reimbursement"');
    expect(workflow).toContain('test "$minimum_system_version" = "11.0"');
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

describe("macOS application identity", () => {
  it("uses a stable non-app identifier and declares macOS 11", () => {
    const config = JSON.parse(tauriConfig) as {
      identifier: string;
      bundle: { macOS?: { minimumSystemVersion?: string } };
    };
    expect(config.identifier).toBe("com.invoice-desk.desktop");
    expect(config.identifier.endsWith(".app")).toBe(false);
    expect(config.bundle.macOS?.minimumSystemVersion).toBe("11.0");
  });

  it("documents the formal data root and fails closed on an unavailable old export root", () => {
    expect(recoveryGuide).toContain(
      "~/Library/Application Support/com.invoice-desk.desktop/",
    );
    expect(recoveryGuide).toContain("旧预发布目录不是正式数据源");
    expect(recoveryGuide).toContain("不能在设置页直接改用新的导出根目录");
  });
});

describe("browser acceptance harness", () => {
  it("starts its own bridge-enabled server by default", () => {
    expect(playwrightConfig).toContain("reuseExistingServer: false");
    expect(playwrightConfig).toContain('VITE_BROWSER_COMMAND_BRIDGE: "1"');
  });

  it("audits the internal application scroller and captures settings at the bottom", () => {
    expect(e2eFlow).toContain('.querySelector<HTMLElement>(".app-main")');
    expect(e2eFlow).toContain("appMain.scrollWidth");
    expect(e2eFlow).toContain("appMain.clientWidth");
    expect(e2eFlow).toContain("attachScrollableView(");
    expect(e2eFlow).toContain('"settings"');
    expect(e2eFlow).toContain("同步与存储");
  });
});
