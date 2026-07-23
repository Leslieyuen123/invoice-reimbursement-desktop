import { readFile } from "node:fs/promises";
import { join, sep } from "node:path";

import { getDocument } from "pdfjs-dist/legacy/build/pdf.mjs";
import { describe, expect, it } from "vitest";

import quickStart from "../../docs/user-guide/invoice-reimbursement-quick-start.md?raw";
import userManual from "../../docs/user-guide/invoice-reimbursement-user-manual.md?raw";
import ciWorkflow from "../../.github/workflows/ci.yml?raw";
import packageLockSource from "../../package-lock.json?raw";
import packageSource from "../../package.json?raw";
import diagramBuilder from "../../scripts/build_user_guide_diagrams.py?raw";
import pdfBuilder from "../../scripts/build_user_guide_pdfs.py?raw";
import cargoLock from "../../src-tauri/Cargo.lock?raw";
import cargoManifest from "../../src-tauri/Cargo.toml?raw";
import tauriConfigSource from "../../src-tauri/tauri.conf.json?raw";

const EXPECTED_VERSION = "0.2.2";
const EXPECTED_DMG = "invoice-reimbursement-0.2.2-macos-arm64.dmg";
const EXPECTED_DOCUMENT_DATE = "2026-07-23";
const REPO_ROOT = process.cwd();
const STANDARD_FONT_DATA_URL =
  join(REPO_ROOT, "node_modules/pdfjs-dist/standard_fonts") + sep;

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

function cargoManifestPackageVersion(source: string): string {
  const lines = source.split(/\r?\n/);
  const packageStart = lines.findIndex((line) => line.trim() === "[package]");
  if (packageStart < 0) {
    throw new Error("missing Cargo.toml [package] section");
  }

  const followingSection = lines
    .slice(packageStart + 1)
    .findIndex((line) => /^\s*\[[^\]]+\]\s*$/.test(line));
  const packageEnd =
    followingSection < 0 ? lines.length : packageStart + 1 + followingSection;
  const packageSection = lines.slice(packageStart + 1, packageEnd).join("\n");
  const name = packageSection.match(/^\s*name\s*=\s*"([^"]+)"\s*(?:#.*)?$/m)?.[1];
  const version = packageSection.match(
    /^\s*version\s*=\s*"([^"]+)"\s*(?:#.*)?$/m,
  )?.[1];
  if (name !== "invoice-reimbursement" || !version) {
    throw new Error("missing invoice-reimbursement Cargo.toml package version");
  }

  return version;
}

function namedWorkflowStep(source: string, name: string): string {
  const lines = source.split(/\r?\n/);
  const start = lines.findIndex((line) => line.trim() === `- name: ${name}`);
  if (start < 0) return "";

  let end = start + 1;
  while (end < lines.length && !/^\s{6}-\s/.test(lines[end])) end += 1;
  return lines.slice(start, end).join("\n");
}

async function pdfText(relativePath: string): Promise<string> {
  const loadingTask = getDocument({
    data: new Uint8Array(await readFile(join(REPO_ROOT, relativePath))),
    standardFontDataUrl: STANDARD_FONT_DATA_URL,
  });

  try {
    const document = await loadingTask.promise;
    try {
      const pages: string[] = [];
      for (let pageNumber = 1; pageNumber <= document.numPages; pageNumber += 1) {
        const page = await document.getPage(pageNumber);
        try {
          const content = await page.getTextContent();
          pages.push(
            content.items
              .flatMap((item) => ("str" in item ? [item.str] : []))
              .join(" "),
          );
        } finally {
          page.cleanup();
        }
      }
      return pages.join("\n");
    } finally {
      await document.cleanup();
      await document.destroy();
    }
  } finally {
    await loadingTask.destroy();
  }
}

async function pdfPageCount(relativePath: string): Promise<number> {
  const loadingTask = getDocument({
    data: new Uint8Array(await readFile(join(REPO_ROOT, relativePath))),
    standardFontDataUrl: STANDARD_FONT_DATA_URL,
  });

  try {
    const document = await loadingTask.promise;
    try {
      return document.numPages;
    } finally {
      await document.cleanup();
      await document.destroy();
    }
  } finally {
    await loadingTask.destroy();
  }
}

describe("release version alignment", () => {
  it("reads the Cargo package version when another field separates it from the name", () => {
    const manifest = `[package]
name = "invoice-reimbursement"
description = "Desktop invoice workflow"
version = "${EXPECTED_VERSION}"
edition = "2024"

[lib]
name = "invoice_reimbursement"`;

    expect(cargoManifestPackageVersion(manifest)).toBe(EXPECTED_VERSION);
  });

  it("keeps every release-facing source at v0.2.2", () => {
    const packageJson = JSON.parse(packageSource) as { version: string };
    const packageLock = JSON.parse(packageLockSource) as {
      version: string;
      packages: { "": { version: string } };
    };
    const tauriConfig = JSON.parse(tauriConfigSource) as { version: string };
    const cargoVersion = cargoManifestPackageVersion(cargoManifest);

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
      expect.soft(guide).toContain(EXPECTED_DOCUMENT_DATE);
      expect.soft(guide).not.toContain("v0.2.0");
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

  it("documents the automatic batch workflow and its exception loop", () => {
    const mainStages = ["创建自动批次", "范围扫描/识别", "安全筛选", "归属/导出"];
    const stageOffsets = mainStages.map((stage) => diagramBuilder.indexOf(stage));

    expect.soft(stageOffsets.every((offset) => offset >= 0)).toBe(true);
    expect.soft(stageOffsets).toEqual([...stageOffsets].sort((left, right) => left - right));
    expect.soft(diagramBuilder).toContain("待处理池");
    expect.soft(diagramBuilder).toContain("人工修正");
    expect.soft(diagramBuilder).toContain("再次运行");
    expect.soft(diagramBuilder).not.toContain("立即同步 -> 待处理池");
    expect.soft(diagramBuilder).not.toContain("导出前仍需要人工");
  });

  it("documents verified HTTPS invoice originals and visible portal exceptions", () => {
    const requiredConcepts = [
      "HTTPS 链接",
      "PDF、JPEG、PNG 或 ZIP",
      "自动下载",
      "诺诺短链接",
      "真实 PDF",
      "XML 配套链接",
      "fp.nuonuo.com/#/",
      "临时失败",
      "重新运行原批次，或新建覆盖邮件日期的批次",
      "不会重复导入已成功下载的原件",
      "需要登录",
      "依赖脚本",
      "不会中断后续发票或邮箱",
      "并非支持所有发票门户",
      "所有处理都在《发票报销》App 内完成",
    ];

    for (const guide of [quickStart, userManual]) {
      for (const concept of requiredConcepts) {
        expect.soft(guide).toContain(concept);
      }
      expect.soft(guide).not.toContain("只支持邮件附件");
    }
  });

  it("publishes one canonical DMG with a verified checksum", () => {
    const prepare = namedWorkflowStep(ciWorkflow, "Prepare canonical release artifacts");
    const upload = namedWorkflowStep(ciWorkflow, "Upload canonical release artifacts");

    expect.soft(prepare).toContain(
      'dmg_files=("$PWD"/src-tauri/target/release/bundle/dmg/*_aarch64.dmg)',
    );
    expect.soft(prepare).toContain('test "${#dmg_files[@]}" -eq 1');
    expect.soft(prepare).toContain(`canonical_name="${EXPECTED_DMG}"`);
    expect.soft(prepare).toContain('cp "${dmg_files[0]}" "$artifact_dir/$canonical_name"');
    expect.soft(prepare).toContain('shasum -a 256 "$canonical_name" > "$canonical_name.sha256"');
    expect.soft(prepare).toContain('shasum -a 256 -c "$canonical_name.sha256"');

    expect.soft(upload).toContain(`release-artifacts/${EXPECTED_DMG}`);
    expect.soft(upload).toContain(`release-artifacts/${EXPECTED_DMG}.sha256`);
    expect.soft(upload).not.toContain("*.dmg");
  });

  it("pins the user-guide generator and renders the current document date", async () => {
    const requirements = await readFile(
      join(REPO_ROOT, "scripts/user-guide-requirements.txt"),
      "utf-8",
    ).catch(() => "");

    expect.soft(requirements).toMatch(/^reportlab==\d+\.\d+\.\d+$/mu);
    expect.soft(requirements).toMatch(/^Pillow==\d+\.\d+\.\d+$/mu);
    expect.soft(pdfBuilder).toContain(`DOCUMENT_DATE = "${EXPECTED_DOCUMENT_DATE}"`);
    expect.soft(pdfBuilder).toContain('Paragraph(DOCUMENT_DATE, style_map["table"])');
    expect.soft(pdfBuilder).toContain(
      "uv run --with-requirements scripts/user-guide-requirements.txt",
    );
    expect.soft(pdfBuilder).not.toContain('Paragraph("2026-07-21"');
    expect.soft(pdfBuilder).not.toContain('Paragraph("2026-07-22"');
  });

  it("keeps both committed user-guide PDFs aligned with the release", async () => {
    const normalizedReleaseConcepts = [
      "HTTPS",
      "诺诺短链接",
      "XML配套链接",
      "并非支持所有发票门户",
      "重新运行原批次，或新建覆盖邮件日期的批次",
    ];
    const guides = [
      {
        path: "output/pdf/invoice-reimbursement-user-manual-zh-cn.pdf",
        pageCount: 18,
        identity: "发票报销完整用户手册",
        distinctContent: ["9. 导出报销材料", "附录 C：导出文件对照表"],
      },
      {
        path: "output/pdf/invoice-reimbursement-quick-start-zh-cn.pdf",
        pageCount: 3,
        identity: "发票报销快速入门",
        distinctContent: ["3. 批次与导出", "更多说明请查看"],
      },
    ];

    for (const guide of guides) {
      expect.soft(await pdfPageCount(guide.path)).toBe(guide.pageCount);
      const text = (await pdfText(guide.path)).replace(/\s+/gu, "");
      expect.soft(text).toContain(guide.identity.replace(/\s+/gu, ""));
      expect.soft(text).toContain(`v${EXPECTED_VERSION}`);
      expect.soft(text).toContain(EXPECTED_DOCUMENT_DATE);
      expect.soft(text).not.toContain("v0.2.0");
      expect.soft(text).not.toContain("v0.1.0");
      expect.soft(text).toContain("立即同步");
      expect.soft(text).toContain("保存并确认");
      for (const concept of normalizedReleaseConcepts) {
        expect.soft(text).toContain(concept);
      }
      for (const expected of guide.distinctContent) {
        expect.soft(text).toContain(expected.replace(/\s+/gu, ""));
      }
    }
  });
});
