import { describe, expect, it } from "vitest";

import workflow from "../../.github/workflows/ci.yml?raw";
import entitlements from "../../src-tauri/Entitlements.plist?raw";
import tauriConfig from "../../src-tauri/tauri.conf.json?raw";
import designSpec from "../../docs/superpowers/specs/2026-07-13-invoice-reimbursement-app-design.md?raw";
import implementationPlan from "../../docs/superpowers/plans/2026-07-13-invoice-reimbursement-app.md?raw";
import recoveryGuide from "../../docs/operations/local-data-and-recovery.md?raw";
import releaseChecklist from "../../docs/operations/release-checklist.md?raw";
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
    expect(workflow).toContain("Print :LSMinimumSystemVersion");
    expect(workflow).toContain('otool -l "$bundle_app/Contents/MacOS/invoice-reimbursement"');
    expect(workflow).toContain('otool -l "$bundle_app/Contents/MacOS/invoice-ocr"');
    expect(workflow).toContain('test "$plist_minimum_system_version" = "11.0"');
    expect(workflow).toContain('test "$main_minimum_system_version" = "11.0"');
    expect(workflow).toContain('test "$ocr_minimum_system_version" = "11.0"');
  });

  it("verifies the standalone app and mounted DMG payload before upload", () => {
    expect(workflow).toContain("name: Verify Apple Silicon bundles");
    expect(workflow).toContain('lipo -archs "$bundle_app/Contents/MacOS/invoice-reimbursement"');
    expect(workflow).toContain('lipo -archs "$bundle_app/Contents/MacOS/invoice-ocr"');
    expect(workflow).toContain('hdiutil verify "${dmg_files[0]}"');
    expect(workflow).toContain('hdiutil attach "${dmg_files[0]}"');
    expect(workflow).toContain('lipo -archs "$mounted_app/Contents/MacOS/invoice-reimbursement"');
    expect(workflow).toContain('lipo -archs "$mounted_app/Contents/MacOS/invoice-ocr"');
    expect(workflow).toContain("packaged_sidecar_runs_through_process_gateway_with_cold_start_margin");
    expect(workflow).toContain('INVOICE_OCR_BIN="$bundle_app/Contents/MacOS/invoice-ocr"');
    expect(workflow).toContain('INVOICE_OCR_BIN="$mounted_app/Contents/MacOS/invoice-ocr"');
  });
});

describe("macOS application identity", () => {
  it("uses a stable non-app identifier and declares macOS 11", () => {
    const config = JSON.parse(tauriConfig) as {
      identifier: string;
      bundle: {
        macOS?: {
          minimumSystemVersion?: string;
          hardenedRuntime?: boolean;
          signingIdentity?: string;
          entitlements?: string;
        };
      };
    };
    expect(config.identifier).toBe("com.invoice-desk.desktop");
    expect(config.identifier.endsWith(".app")).toBe(false);
    expect(config.bundle.macOS?.minimumSystemVersion).toBe("11.0");
    expect(config.bundle.macOS?.hardenedRuntime).toBe(true);
    expect(config.bundle.macOS?.signingIdentity).toBe("-");
    expect(config.bundle.macOS?.entitlements).toBe("Entitlements.plist");
    expect(entitlements).toContain("com.apple.security.cs.disable-library-validation");
    expect(entitlements).toMatch(
      /<key>com\.apple\.security\.cs\.disable-library-validation<\/key>\s*<true\/>/,
    );
  });

  it("records the pre-release identity correction without inventing a migration", () => {
    for (const document of [designSpec, releaseChecklist, recoveryGuide]) {
      expect(document).toContain("首发前 identity correction");
      expect(document).toContain("零既有用户");
      expect(document).toContain("com.invoice-desk.desktop");
    }
    expect(implementationPlan).toContain(
      "identifier 固定为 `com.invoice-desk.app`",
    );
    expect(implementationPlan).toContain("由首发身份决策显式取代");
    expect(recoveryGuide).toContain("不执行 bundle identity 数据迁移");
    expect(recoveryGuide).toContain(
      "~/Library/Application Support/com.invoice-desk.desktop/",
    );
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

  it("waits for the image element to finish decoding before measuring it", () => {
    expect(e2eFlow).toContain("expect.poll");
    expect(e2eFlow).toContain("image.complete");
    expect(e2eFlow).toContain("image.naturalWidth > 0");
    expect(e2eFlow).toContain("image.naturalHeight > 0");
  });

  it("parses the preview screenshot and rejects blank pixel evidence", () => {
    expect(e2eFlow).toContain('from "pngjs"');
    expect(e2eFlow).toContain("PNG.sync.read(previewScreenshot)");
    expect(e2eFlow).toContain("previewPixelRange");
    expect(e2eFlow).toContain("darkPixelCount");
    expect(e2eFlow).toContain("lightPixelCount");
  });
});
