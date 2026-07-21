import { describe, expect, it } from "vitest";

import quickStart from "../../docs/user-guide/invoice-reimbursement-quick-start.md?raw";
import userManual from "../../docs/user-guide/invoice-reimbursement-user-manual.md?raw";
import packageLockSource from "../../package-lock.json?raw";
import packageSource from "../../package.json?raw";
import pdfBuilder from "../../scripts/build_user_guide_pdfs.py?raw";
import cargoLock from "../../src-tauri/Cargo.lock?raw";
import cargoManifest from "../../src-tauri/Cargo.toml?raw";
import tauriConfigSource from "../../src-tauri/tauri.conf.json?raw";

const EXPECTED_VERSION = "0.1.1";
const EXPECTED_DMG = "invoice-reimbursement-0.1.1-macos-arm64.dmg";

function cargoPackageVersion(source: string, packageName: string): string {
  const packageBlock = source
    .split("[[package]]")
    .find((block) => block.includes(`name = "${packageName}"`));
  const version = packageBlock?.match(/^version = "([^"]+)"$/m)?.[1];
  if (!version) {
    throw new Error(`missing Cargo.lock package version for ${packageName}`);
  }
  return version;
}

describe("release version alignment", () => {
  it("keeps every release-facing source at v0.1.1", () => {
    const packageJson = JSON.parse(packageSource) as { version: string };
    const packageLock = JSON.parse(packageLockSource) as {
      version: string;
      packages: { "": { version: string } };
    };
    const tauriConfig = JSON.parse(tauriConfigSource) as { version: string };
    const cargoVersion = cargoManifest.match(
      /^name = "invoice-reimbursement"\nversion = "([^"]+)"$/m,
    )?.[1];

    expect.soft(packageJson.version).toBe(EXPECTED_VERSION);
    expect.soft(packageLock.version).toBe(EXPECTED_VERSION);
    expect.soft(packageLock.packages[""].version).toBe(EXPECTED_VERSION);
    expect.soft(tauriConfig.version).toBe(EXPECTED_VERSION);
    expect.soft(cargoVersion).toBe(EXPECTED_VERSION);
    expect
      .soft(cargoPackageVersion(cargoLock, "invoice-reimbursement"))
      .toBe(EXPECTED_VERSION);

    for (const guide of [quickStart, userManual]) {
      expect.soft(guide).toContain(`v${EXPECTED_VERSION}`);
      expect.soft(guide).toContain(EXPECTED_DMG);
      expect.soft(guide).not.toContain("v0.1.0");
      expect.soft(guide).not.toContain("发票报销_0.1.0_aarch64.dmg");
      expect
        .soft(guide)
        .not.toContain(
          "221da96e640426824a3d6673d9e90080302f0edd4fce45fb856d5e74a8e111b3",
        );
    }

    expect.soft(pdfBuilder).toContain("APP_VERSION = json.loads(");
    expect.soft(pdfBuilder).toContain('REPO_ROOT / "package.json"');
    expect.soft(pdfBuilder).toContain('f"发票报销 v{APP_VERSION}"');
    expect.soft(pdfBuilder).toContain('f"v{APP_VERSION}"');
    expect.soft(pdfBuilder).not.toContain("v0.1.0");
    expect.soft(pdfBuilder).not.toContain("0.1.0");
  });
});
