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

  it("checks the mounted DMG app deployment target before its OCR test", () => {
    expect(workflow).toContain(
      'mounted_plist_minimum_system_version="$(\n            /usr/libexec/PlistBuddy -c \'Print :LSMinimumSystemVersion\' \\\n              "$mounted_app/Contents/Info.plist"',
    );
    expect(workflow).toContain(
      'otool -l "$mounted_app/Contents/MacOS/invoice-reimbursement"',
    );
    expect(workflow).toContain(
      'otool -l "$mounted_app/Contents/MacOS/invoice-ocr"',
    );
    expect(workflow).toContain(
      'test "$mounted_plist_minimum_system_version" = "11.0"',
    );
    expect(workflow).toContain(
      'test "$mounted_main_minimum_system_version" = "11.0"',
    );
    expect(workflow).toContain(
      'test "$mounted_ocr_minimum_system_version" = "11.0"',
    );

    const mountedTargetCheck = workflow.indexOf(
      'mounted_plist_minimum_system_version="$(',
    );
    expect(mountedTargetCheck).toBeGreaterThan(
      workflow.indexOf('hdiutil attach "${dmg_files[0]}"'),
    );
    const mountedOcrTest = workflow.indexOf(
      'INVOICE_OCR_BIN="$mounted_app/Contents/MacOS/invoice-ocr"',
    );
    expect(mountedTargetCheck).toBeLessThan(mountedOcrTest);
    expect(
      workflow.indexOf(
        'test "$mounted_ocr_minimum_system_version" = "11.0"',
      ),
    ).toBeLessThan(mountedOcrTest);
  });

  it("keeps the DMG cleanup trap around mounted payload verification", () => {
    expect(workflow).toContain("trap cleanup EXIT");
    expect(workflow).toContain(
      'hdiutil detach "$mount_dir" -quiet || true',
    );
    expect(workflow).toContain("trap - EXIT");
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

  it("asserts the mounted app path is absolute before its OCR test", () => {
    const absolutePathCheck = workflow.indexOf('[[ "$mounted_app" = /* ]]');
    expect(absolutePathCheck).toBeGreaterThan(
      workflow.indexOf(
        'mounted_app="$(find "$mount_dir" -maxdepth 1 -type d -name \'*.app\' -print -quit)"',
      ),
    );
    expect(absolutePathCheck).toBeLessThan(
      workflow.indexOf(
        'INVOICE_OCR_BIN="$mounted_app/Contents/MacOS/invoice-ocr"',
      ),
    );
  });

  it("anchors packaged artifact paths before invoking Cargo integration tests", () => {
    expect(workflow).toContain(
      'bundle_app="$PWD/src-tauri/target/release/bundle/macos/发票报销.app"',
    );
    expect(workflow).toContain(
      'dmg_files=("$PWD"/src-tauri/target/release/bundle/dmg/*_aarch64.dmg)',
    );
  });

  it("keeps bundle creation gated and limited to app and DMG artifacts", () => {
    expect(workflow).toContain(
      "  bundle:\n    name: unsigned macOS bundle\n    needs: gates",
    );
    expect(workflow).toContain(
      "run: npm run tauri build -- --bundles app,dmg",
    );
  });

  it("verifies ad-hoc hardened-runtime signing for standalone and mounted apps", () => {
    expect(workflow.match(/verify_app_signature\(\) \(/g)).toHaveLength(1);
    expect(
      workflow.match(/codesign --verify --deep --strict "\$app_path"/g),
    ).toHaveLength(1);
    expect(workflow).toContain(
      'signature_details="$(codesign -dv --verbose=4 "$main_executable" 2>&1)"',
    );
    expect(workflow).toContain(
      "grep -qx 'Signature=adhoc' <<<\"$signature_details\"",
    );
    expect(workflow).toContain(
      "grep -Eq '^CodeDirectory .*flags=.*runtime' <<<\"$signature_details\"",
    );
    expect(workflow).toContain(
      'codesign -d --entitlements - --xml "$main_executable" >"$entitlements_plist"',
    );
    expect(workflow).toContain(
      "/usr/libexec/PlistBuddy -c 'Print :com.apple.security.cs.disable-library-validation'",
    );
    expect(workflow).toContain(
      'test "$disable_library_validation" = "true"',
    );
    expect(workflow).toContain('verify_app_signature "$bundle_app"');
    expect(workflow).toContain('verify_app_signature "$mounted_app"');
    for (const forbiddenMarker of [
      "Developer ID",
      "notarytool",
      "secrets.",
    ]) {
      expect(workflow).not.toContain(forbiddenMarker);
    }
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
