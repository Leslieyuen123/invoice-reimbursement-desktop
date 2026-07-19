# Invoice Reimbursement User Manual Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为《发票报销》v0.1.0 产出一套可直接分享的完整中文用户手册和 2–3 页快速入门，同时保留可维护的 Markdown 源文件与可重复生成的 PDF 构建工具。

**Architecture:** 以当前 React/Tauri 界面和自动化测试为事实来源，使用 Playwright 浏览器 bridge 生成稳定、脱敏的真实界面截图，使用 Pillow 生成流程和状态示意图。Markdown 是内容唯一事实源；ReportLab 构建器读取 Markdown 和图片产出两份 A4 PDF，再通过结构检查、文字抽取和逐页渲染完成验收。

**Tech Stack:** React 19 UI，Playwright 1.61，TypeScript，Python 3，ReportLab 4.4，Pillow 12.2，pypdf 6.10，Poppler `pdftoppm`/`pdfinfo`。

---

## File Map

- Modify: `.gitignore` - 忽略本地头脑风暴会话和 PDF/截图中间文件。
- Create: `scripts/__init__.py` - 允许文档构建工具作为 Python 模块被测试。
- Create: `scripts/tests/__init__.py` - Python 测试包标记。
- Create: `scripts/verify_user_guide.py` - 检查文档结构、关键界面文案、图片引用、版本与敏感信息。
- Create: `scripts/tests/test_verify_user_guide.py` - 文档检查器的单元测试。
- Create: `tests/e2e/user-guide-capture.spec.ts` - 仅在 `CAPTURE_USER_GUIDE=1` 时运行的脱敏截图流程。
- Create: `scripts/build_user_guide_diagrams.py` - 生成主工作流和票据状态示意图。
- Create: `scripts/tests/test_build_user_guide_diagrams.py` - 校验示意图尺寸、非空像素和输出名称。
- Create: `docs/user-guide/assets/*.png` - 脱敏 App 截图和流程示意图。
- Create: `docs/user-guide/invoice-reimbursement-quick-start.md` - 独立 2–3 页快速入门内容。
- Create: `docs/user-guide/invoice-reimbursement-user-manual.md` - 完整用户手册内容。
- Create: `scripts/user_guide_pdf.py` - Markdown 到 A4 PDF 的 ReportLab 构建器。
- Create: `scripts/tests/test_user_guide_pdf.py` - PDF 结构、字体嵌入、页码和图片的单元测试。
- Create: `output/pdf/invoice-reimbursement-user-manual-zh-cn.pdf` - 完整手册发布文件。
- Create: `output/pdf/invoice-reimbursement-quick-start-zh-cn.pdf` - 快速入门发布文件。

### Task 1: Repository Hygiene and User Guide Contract

**Files:**
- Modify: `.gitignore`
- Create: `scripts/__init__.py`
- Create: `scripts/tests/__init__.py`
- Create: `scripts/verify_user_guide.py`
- Create: `scripts/tests/test_verify_user_guide.py`

- [ ] **Step 1: Create the Python package markers and write the failing verifier tests**

Create empty `scripts/__init__.py` and `scripts/tests/__init__.py`, then create `scripts/tests/test_verify_user_guide.py` with tests that use `tempfile.TemporaryDirectory` and verify all of these cases:

```python
import tempfile
import unittest
from pathlib import Path

from scripts.verify_user_guide import verify_markdown


class VerifyUserGuideTest(unittest.TestCase):
    def write(self, root: Path, name: str, body: str) -> Path:
        path = root / name
        path.write_text(body, encoding="utf-8")
        return path

    def test_accepts_complete_quick_start(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "assets").mkdir()
            (root / "assets" / "workflow.png").write_bytes(b"png")
            path = self.write(
                root,
                "quick.md",
                "# 发票报销快速入门\n\n"
                "v0.1.0 | Apple Silicon | macOS 11\n\n"
                "## 1. 安装与绑定\n\n测试连接 保存账号\n\n"
                "## 2. 同步与确认\n\n立即同步 保存并确认\n\n"
                "## 3. 批次与导出\n\n"
                "merged.pdf reimbursement.xlsx originals.zip manifest.json\n\n"
                "![工作流](assets/workflow.png)\n",
            )
            self.assertEqual(verify_markdown(path, "quick"), [])

    def test_reports_missing_terms_images_and_sensitive_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = self.write(
                root,
                "manual.md",
                "# 手册\n\n/Users/leslie_yuen/private\n\n![不存在](assets/missing.png)\n",
            )
            errors = verify_markdown(path, "full")
            self.assertTrue(any("缺少必需内容" in error for error in errors))
            self.assertTrue(any("图片不存在" in error for error in errors))
            self.assertTrue(any("个人绝对路径" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the verifier tests and confirm the module is absent**

Run:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 -m unittest scripts.tests.test_verify_user_guide -v
```

Expected: FAIL with `ModuleNotFoundError: No module named 'scripts.verify_user_guide'`.

- [ ] **Step 3: Implement the guide verifier**

Implement `scripts/verify_user_guide.py` with this public contract:

```python
from __future__ import annotations

import argparse
import re
from pathlib import Path

REQUIRED = {
    "quick": (
        "v0.1.0", "Apple Silicon", "macOS 11", "安装与绑定",
        "同步与确认", "批次与导出", "测试连接",
        "保存账号", "立即同步", "保存并确认",
        "merged.pdf", "reimbursement.xlsx", "originals.zip", "manifest.json",
    ),
    "full": (
        "v0.1.0", "Apple Silicon", "macOS 11", "控制台", "待处理池",
        "报销批次", "运行设置", "Gmail", "QQ 邮箱", "测试连接",
        "连接成功", "保存账号", "立即同步", "重新识别",
        "保存并确认", "确认保留", "删除重复", "调整票据",
        "归属所选票据", "后台自动同步", "关闭主窗口", "merged.pdf",
        "reimbursement.xlsx", "originals.zip", "manifest.json",
        "~/Library/Application Support/com.invoice-desk.desktop/",
    ),
}
IMAGE_RE = re.compile(r"!\[[^\]]*\]\(([^)]+)\)")
FORBIDDEN = ("ghp_", "/Users/leslie_yuen/", "T" + "ODO", "T" + "BD")


def verify_markdown(path: Path, profile: str) -> list[str]:
    text = path.read_text(encoding="utf-8")
    errors = [f"缺少必需内容: {term}" for term in REQUIRED[profile] if term not in text]
    for marker in FORBIDDEN:
        if marker in text:
            errors.append(f"包含禁止内容: {marker}")
    if re.search(r"/Users/[^/\s]+/", text):
        errors.append("包含个人绝对路径")
    for target in IMAGE_RE.findall(text):
        if not target.startswith(("http://", "https://")) and not (path.parent / target).is_file():
            errors.append(f"图片不存在: {target}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("profile", choices=sorted(REQUIRED))
    parser.add_argument("path", type=Path)
    args = parser.parse_args()
    errors = verify_markdown(args.path, args.profile)
    for error in errors:
        print(error)
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 4: Add local artifact directories to `.gitignore`**

Append exactly these entries:

```gitignore
.superpowers/
tmp/pdfs/
tmp/user-guide/
```

Do not ignore `docs/user-guide/assets/` or `output/pdf/`; the reviewed source assets and final PDFs are deliverables.

- [ ] **Step 5: Run the verifier tests**

Run the unittest command from Step 2.

Expected: 2 tests PASS.

- [ ] **Step 6: Commit the contract and repository hygiene**

```bash
git add .gitignore scripts/__init__.py scripts/tests/__init__.py scripts/verify_user_guide.py scripts/tests/test_verify_user_guide.py
git commit -m "test: define user guide content contract"
```

### Task 2: Deterministic, Redacted App Screenshots

**Files:**
- Create: `tests/e2e/user-guide-capture.spec.ts`
- Create: `docs/user-guide/assets/01-mailbox-setup.png`
- Create: `docs/user-guide/assets/02-dashboard-sync.png`
- Create: `docs/user-guide/assets/03-inbox-import.png`
- Create: `docs/user-guide/assets/04-invoice-review.png`
- Create: `docs/user-guide/assets/05-batch-create.png`
- Create: `docs/user-guide/assets/06-batch-export.png`
- Create: `docs/user-guide/assets/07-storage-settings.png`

- [ ] **Step 1: Write the opt-in screenshot test**

Create a Playwright spec that begins with:

```typescript
import { expect, test, type Locator, type Page } from "@playwright/test";
import path from "node:path";

const fixture = "src-tauri/tests/fixtures/text-invoice.pdf";
const assets = path.resolve("docs/user-guide/assets");

test.skip(process.env.CAPTURE_USER_GUIDE !== "1", "documentation capture is opt-in");

async function capture(page: Page, name: string, frame: Locator, targets: Locator[] = []) {
  await frame.scrollIntoViewIfNeeded();
  const frameBox = await frame.boundingBox();
  if (!frameBox) throw new Error(`capture frame ${name} is not visible`);
  const markers: string[] = [];
  for (const [index, target] of targets.entries()) {
    const box = await target.boundingBox();
    if (!box) throw new Error(`capture target ${index + 1} is not visible`);
    markers.push(JSON.stringify({ index: index + 1, x: box.x + box.width - 10, y: box.y + 10 }));
  }
  await page.evaluate((serialized) => {
    for (const value of serialized) {
      const marker = JSON.parse(value) as { index: number; x: number; y: number };
      const node = document.createElement("span");
      node.dataset.guideMarker = "true";
      node.textContent = String(marker.index);
      Object.assign(node.style, {
        position: "fixed", left: `${marker.x}px`, top: `${marker.y}px`, zIndex: "99999",
        width: "24px", height: "24px", borderRadius: "50%", display: "grid",
        placeItems: "center", background: "#bc6849", color: "white",
        font: "600 13px system-ui", boxShadow: "0 0 0 3px rgba(13,13,15,.75)",
      });
      document.body.append(node);
    }
  }, markers);
  await page.mouse.move(4, 4);
  const padding = 20;
  await page.screenshot({
    path: path.join(assets, name),
    animations: "disabled",
    clip: {
      x: Math.max(0, frameBox.x - padding),
      y: Math.max(0, frameBox.y - padding),
      width: Math.min(1440 - Math.max(0, frameBox.x - padding), frameBox.width + padding * 2),
      height: Math.min(900 - Math.max(0, frameBox.y - padding), frameBox.height + padding * 2),
    },
  });
  await page.locator('[data-guide-marker="true"]').evaluateAll((nodes) => nodes.forEach((node) => node.remove()));
}
```

The single test must then execute this exact stateful flow at the `standard-1440x900` viewport:

1. Open `/?bridgeReset=1`, navigate to Settings, fill `guide@example.invalid` and `demo-app-password`, click `测试连接`, wait for `连接成功`, and capture the form with markers on email type, email, app password, test, and save.
2. Click `保存账号`, return to `控制台`, click `立即同步`, and capture the dashboard with a marker on the sync button.
3. Navigate to `待处理池`, import `fixture` through `browser-file-input`, and capture the row and import controls.
4. Open `text-invoice.pdf`, verify `票据详情`, and capture with markers on preview, category, amount, and `保存并确认`.
5. Save and close the drawer, navigate to batches, open `新建批次`, and capture the create panel before creating the July 2026 batch.
6. Create the batch, click `加入推荐票据`, click `导出报销包`, wait for `报销包已生成`, and capture the four output filenames.
7. Navigate back to Settings, scroll `同步与存储` into view, and capture the background-sync toggle, local data directory, and export directory.

Use only the browser bridge, the repository's test invoice, the reserved `.invalid` email domain, and the fixed `/tmp/invoice-reimbursement/e2e` paths. Never start a real IMAP connection.

- [ ] **Step 2: Run the capture spec before creating the assets directory**

Run:

```bash
CAPTURE_USER_GUIDE=1 npx playwright test tests/e2e/user-guide-capture.spec.ts --project=standard-1440x900
```

Expected: FAIL because `docs/user-guide/assets` does not exist.

- [ ] **Step 3: Create the assets directory and complete all capture steps**

Create `docs/user-guide/assets/`, finish the exact interactions above, and ensure each screenshot uses `expect(...).toBeVisible()` before capture. Keep the test skipped unless `CAPTURE_USER_GUIDE=1` so the normal three-viewport CI suite does not rewrite documentation assets.

- [ ] **Step 4: Generate the seven screenshots**

Run the command from Step 2.

Expected: 1 test PASS in `standard-1440x900`, with seven PNG files under `docs/user-guide/assets/`.

- [ ] **Step 5: Verify screenshot dimensions and deterministic privacy inputs**

Run:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 -c "from pathlib import Path; from PIL import Image,ImageStat; files=sorted(Path('docs/user-guide/assets').glob('0*.png')); assert len(files)==7; assert all(Image.open(p).width>=640 and Image.open(p).height>=360 for p in files); assert all(max(ImageStat.Stat(Image.open(p).convert('RGB')).var)>100 for p in files); print([(p.name,Image.open(p).size) for p in files])"
```

Expected: the seven filenames are printed and all assertions pass. Open every PNG and confirm no real email, authorization code, company, invoice number, username, or home-directory path appears.

- [ ] **Step 6: Commit the screenshot pipeline and images**

```bash
git add tests/e2e/user-guide-capture.spec.ts docs/user-guide/assets/0*.png
git commit -m "docs: capture redacted app workflow"
```

### Task 3: Workflow and Status Diagrams

**Files:**
- Create: `scripts/build_user_guide_diagrams.py`
- Create: `scripts/tests/test_build_user_guide_diagrams.py`
- Create: `docs/user-guide/assets/08-workflow.png`
- Create: `docs/user-guide/assets/09-item-status-flow.png`

- [ ] **Step 1: Write failing diagram tests**

The tests must call `build_all(output_dir)` and assert:

```python
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from PIL import Image, ImageStat

from scripts.build_user_guide_diagrams import build_all


class BuildUserGuideDiagramsTest(unittest.TestCase):
    def test_builds_nonblank_workflow_and_status_diagrams(self):
        with TemporaryDirectory() as directory:
            paths = build_all(Path(directory))
            self.assertEqual([path.name for path in paths], ["08-workflow.png", "09-item-status-flow.png"])
            for path in paths:
                image = Image.open(path).convert("RGB")
                self.assertGreaterEqual(image.width, 1600)
                self.assertGreaterEqual(image.height, 600)
                self.assertGreater(max(ImageStat.Stat(image).var), 100)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the diagram tests and verify failure**

Run:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 -m unittest scripts.tests.test_build_user_guide_diagrams -v
```

Expected: FAIL because `scripts.build_user_guide_diagrams` does not exist.

- [ ] **Step 3: Implement deterministic diagrams with Pillow**

Implement `build_all(output_dir: Path) -> list[Path]` using `/System/Library/Fonts/STHeiti Medium.ttc`, a neutral `#f4f4f5` canvas, dark `#171719` text, purple `#8d72dd`, orange `#bc6849`, and green `#7ea58e` accents.

`08-workflow.png` must render five labeled stages with directional connectors:

```text
同步邮箱或手动导入 -> 自动识别 -> 检查并确认 -> 归入报销批次 -> 导出报销包
```

`09-item-status-flow.png` must render the main path and two blocking branches:

```text
待识别 -> 待确认 -> 可纳入批次
待识别 -> 识别失败 -> 重新识别
待确认 -> 疑似重复 -> 确认保留 / 删除重复
```

Use rounded rectangles of at most 8 px radius, maintain at least 48 px between labels and connectors, and add a concise legend stating that recognition failures and suspected duplicates cannot enter a batch.

- [ ] **Step 4: Run tests and generate diagrams**

Run the test command from Step 2, then:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 scripts/build_user_guide_diagrams.py docs/user-guide/assets
```

Expected: tests PASS and both PNG files are created.

- [ ] **Step 5: Inspect both diagrams at original resolution**

Confirm every Chinese label is readable, no connector crosses text, the status branches are unambiguous, and the images do not contain empty or clipped regions.

- [ ] **Step 6: Commit diagrams and generator**

```bash
git add scripts/build_user_guide_diagrams.py scripts/tests/test_build_user_guide_diagrams.py docs/user-guide/assets/08-workflow.png docs/user-guide/assets/09-item-status-flow.png
git commit -m "docs: add invoice workflow diagrams"
```

### Task 4: Quick Start Markdown

**Files:**
- Create: `docs/user-guide/invoice-reimbursement-quick-start.md`

- [ ] **Step 1: Create the quick-start source with the exact three-part structure**

Write concise Chinese content under these headings:

```markdown
# 发票报销快速入门

> 适用版本：v0.1.0 · Apple Silicon Mac · macOS 11 或更高版本

## 先了解这件事
## 1. 安装与绑定
### 安装未签名 App
### 绑定 Gmail 或 QQ 邮箱
## 2. 同步与确认
### 立即同步
### 检查待处理池
## 3. 批次与导出
### 创建报销批次
### 导出四类报销文件
## 遇到问题
```

The opening callout must say that binding an email account only enables collection and recognition; the user still needs to review, confirm, assign to a batch, and export. Installation must use Finder's `右键 -> 打开` or System Settings' `隐私与安全性 -> 仍要打开`, never instruct users to disable Gatekeeper. The mailbox section must distinguish app-specific password/authorization code from the normal login password. The final section must list all four export filenames.

Place `01-mailbox-setup.png`, `08-workflow.png`, `04-invoice-review.png`, and `06-batch-export.png` where they directly support the corresponding steps. Use explicit `<!-- pagebreak -->` markers to keep installation/binding on page 1, synchronization/review on page 2, and batch/export on page 3. Each numbered marker in a screenshot must have a matching numbered explanation directly below it.

- [ ] **Step 2: Run the quick-start content verifier**

Run:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 scripts/verify_user_guide.py quick docs/user-guide/invoice-reimbursement-quick-start.md
```

Expected: exit 0 with no errors.

- [ ] **Step 3: Perform a novice-path copy edit**

Read the document from top to bottom and remove protocol details that are not needed to finish the first reimbursement. Every action sentence must name the page and visible control; every expected result must immediately follow the action. Keep all paragraphs at five Chinese sentences or fewer.

- [ ] **Step 4: Commit the quick start**

```bash
git add docs/user-guide/invoice-reimbursement-quick-start.md
git commit -m "docs: write invoice reimbursement quick start"
```

### Task 5: Complete User Manual Markdown

**Files:**
- Create: `docs/user-guide/invoice-reimbursement-user-manual.md`

- [ ] **Step 1: Write the full manual with the approved chapter sequence**

Use these exact top-level sections and cover every listed responsibility:

```markdown
# 发票报销完整用户手册
## 关于本手册
## 1. 开始之前
## 2. 下载、安装与首次打开
## 3. 认识发票报销 App
## 4. 绑定 Gmail 或 QQ 邮箱
## 5. 让 App 开始处理发票
## 6. 在待处理池检查发票
## 7. 处理识别失败和疑似重复
## 8. 创建和管理报销批次
## 9. 导出报销材料
## 10. 后台运行、数据目录与备份
## 11. 常见问题
## 附录 A：页面和常用按钮索引
## 附录 B：发票状态对照表
## 附录 C：导出文件对照表
```

Required factual statements:

- Only Apple Silicon, macOS 11+, v0.1.0, unsigned local-share build.
- DMG SHA-256: `221da96e640426824a3d6673d9e90080302f0edd4fce45fb856d5e74a8e111b3`.
- Gmail `imap.gmail.com:993`; QQ `imap.qq.com:993`; sync interval 5–1440 minutes; account enabled; test before save.
- Processing starts through `立即同步`, `后台自动同步`, or manual PDF/image import.
- State definitions for `待识别`, `待确认`, `识别失败`, `疑似重复`, and `可纳入批次`.
- Editable fields: category, invoice date, suggested month, amount, city, company/body, note, event tag, project tag.
- Recognition failures and suspected duplicates cannot be assigned; unconfirmed items block export.
- Output files and local data path exactly match the design spec.
- `关闭主窗口` only hides the App; tray actions reopen, synchronize, or quit.
- Troubleshooting gives symptom, cause check, and safe action for all nine approved scenarios.

Use all nine assets at least once. Keep screenshots close to the action they show and use the two diagrams for the workflow and status explanations. Include official Gmail and QQ authorization-help links already used by `MailboxAccountForm.tsx`.

- [ ] **Step 2: Run the full-manual verifier**

Run:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 scripts/verify_user_guide.py full docs/user-guide/invoice-reimbursement-user-manual.md
```

Expected: exit 0 with no errors.

- [ ] **Step 3: Cross-check UI labels and operational facts**

Run:

```bash
rg -n "立即同步|测试连接|连接成功|保存账号|重新识别|保存并确认|确认保留|删除重复|调整票据|归属所选票据" src
```

Compare the manual against `docs/operations/local-data-and-recovery.md`, `docs/operations/release-checklist.md`, `src-tauri/tauri.conf.json`, and `src-tauri/src/infra/exporters.rs`. Correct the manual immediately if any label, path, version, architecture, minimum system version, or output filename differs.

- [ ] **Step 4: Commit the complete manual**

```bash
git add docs/user-guide/invoice-reimbursement-user-manual.md
git commit -m "docs: write complete invoice reimbursement manual"
```

### Task 6: Reproducible ReportLab PDF Builder

**Files:**
- Create: `scripts/user_guide_pdf.py`
- Create: `scripts/tests/test_user_guide_pdf.py`

- [ ] **Step 1: Write failing PDF builder tests**

Tests must create a temporary Markdown document with a title, version line, headings, paragraph, ordered list, blockquote, pipe table, inline code, external link, local PNG, and `<!-- pagebreak -->`. Call `build_pdf(markdown_path, output_path, kind="quick")`, then verify:

```python
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from PIL import Image
from pypdf import PdfReader

from scripts.user_guide_pdf import build_pdf


class UserGuidePdfTest(unittest.TestCase):
    def test_builds_searchable_a4_pdf_with_embedded_chinese_font(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            image = root / "sample.png"
            Image.new("RGB", (800, 450), "#8d72dd").save(image)
            markdown = root / "guide.md"
            markdown.write_text(
                "# 发票报销快速入门\n\n> v0.1.0\n\n## 第一步\n\n"
                "点击 **立即同步**，查看 `merged.pdf`。\n\n"
                "1. 同步\n2. 确认\n\n| 状态 | 处理 |\n|---|---|\n| 待确认 | 保存并确认 |\n\n"
                "[帮助](https://example.invalid/help)\n\n![示例](sample.png)\n\n"
                "<!-- pagebreak -->\n\n## 完成\n",
                encoding="utf-8",
            )
            output = root / "guide.pdf"
            build_pdf(markdown, output, kind="quick")
            reader = PdfReader(output)
            self.assertGreaterEqual(len(reader.pages), 2)
            text = "\n".join(page.extract_text() or "" for page in reader.pages)
            self.assertIn("发票报销快速入门", text)
            self.assertIn("立即同步", text)
            embedded = False
            for page in reader.pages:
                for reference in page["/Resources"].get("/Font", {}).values():
                    font = reference.get_object()
                    descriptor_reference = font.get("/FontDescriptor")
                    if descriptor_reference is None:
                        continue
                    descriptor = descriptor_reference.get_object()
                    embedded = embedded or any(
                        key in descriptor for key in ("/FontFile", "/FontFile2", "/FontFile3")
                    )
            self.assertTrue(embedded)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the PDF tests and confirm failure**

Run:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 -m unittest scripts.tests.test_user_guide_pdf -v
```

Expected: FAIL because `scripts.user_guide_pdf` does not exist.

- [ ] **Step 3: Implement the PDF builder**

Implement `build_pdf(markdown_path: Path, output_path: Path, kind: Literal["quick", "full"]) -> None` with these exact responsibilities:

1. Register `/System/Library/Fonts/STHeiti Light.ttc` with `TTFont(..., subfontIndex=0)` and use it for all Chinese text so `/FontFile2` or `/FontFile3` is embedded.
2. Parse headings 1–4, paragraphs, blockquotes, ordered/unordered lists, fenced code blocks, pipe tables, Markdown images, horizontal rules, external links, bold text, and inline code. Reject an unknown local image path instead of silently omitting it.
3. Treat `<!-- pagebreak -->` as `PageBreak()`.
4. Use A4 pages with 18 mm left/right margins, 17 mm top margin, and 16 mm bottom margin.
5. Use neutral paper, `#171719` text, purple `#8d72dd` for chapter numbers/links, orange `#bc6849` for cautions, and green `#7ea58e` for successful outcomes. Keep card radii at 8 px or below.
6. Add a cover page, generated table of contents for the full manual, running title, `v0.1.0`, and `第 N 页` footer. The quick start omits the long table of contents but keeps version and page numbering.
7. Scale images proportionally to the content width and never upscale beyond their native pixel dimensions at 144 dpi.
8. Keep heading with its first paragraph, prevent table rows and screenshot captions from splitting, and repeat table headers across pages.
9. Set PDF metadata title, author `发票报销`, subject, and language-oriented keywords.
10. Provide a CLI with `--kind`, input Markdown path, and output PDF path; create the output directory if absent.

Use a `BaseDocTemplate` subclass and `afterFlowable` notifications to populate `TableOfContents`; use `multiBuild` so TOC page numbers stabilize before output.

- [ ] **Step 4: Run PDF unit tests**

Run the command from Step 2.

Expected: tests PASS and the temporary PDF has at least two pages with searchable Chinese text and an embedded font program.

- [ ] **Step 5: Commit the PDF builder**

```bash
git add scripts/user_guide_pdf.py scripts/tests/test_user_guide_pdf.py
git commit -m "feat: add reproducible user guide PDF builder"
```

### Task 7: Build and Visually Verify Both PDFs

**Files:**
- Create: `output/pdf/invoice-reimbursement-user-manual-zh-cn.pdf`
- Create: `output/pdf/invoice-reimbursement-quick-start-zh-cn.pdf`
- Create temporarily: `tmp/pdfs/manual-pages/*.png`
- Create temporarily: `tmp/pdfs/quick-start-pages/*.png`

- [ ] **Step 1: Build both PDFs from the reviewed Markdown sources**

Run:

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 scripts/user_guide_pdf.py --kind full docs/user-guide/invoice-reimbursement-user-manual.md output/pdf/invoice-reimbursement-user-manual-zh-cn.pdf
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 scripts/user_guide_pdf.py --kind quick docs/user-guide/invoice-reimbursement-quick-start.md output/pdf/invoice-reimbursement-quick-start-zh-cn.pdf
```

Expected: both commands exit 0 and create non-empty PDFs.

- [ ] **Step 2: Verify structure, metadata, page counts, and searchable text**

Run `pdfinfo` on both files and use pypdf to assert:

- Full manual contains at least 12 pages.
- Quick start contains 2 or 3 pages.
- Both contain `v0.1.0`, `Apple Silicon`, `立即同步`, and `保存并确认` in extracted text.
- Full manual contains all four export filenames and the local data directory.
- Every page has a media box equal to A4 within one point.
- Every PDF contains an embedded font file.

- [ ] **Step 3: Render every page to PNG**

Run:

```bash
mkdir -p tmp/pdfs/manual-pages tmp/pdfs/quick-start-pages
pdftoppm -png -r 144 output/pdf/invoice-reimbursement-user-manual-zh-cn.pdf tmp/pdfs/manual-pages/page
pdftoppm -png -r 144 output/pdf/invoice-reimbursement-quick-start-zh-cn.pdf tmp/pdfs/quick-start-pages/page
```

Expected: one PNG per PDF page with sequential filenames.

- [ ] **Step 4: Inspect every rendered page**

Inspect all images at readable scale, using contact sheets only as navigation. Check every individual page for Chinese missing glyphs, clipped headings, split captions, blurred screenshots, overlapping text, table overflow, orphan headings, missing page numbers, inconsistent headers/footers, large accidental blank regions, and incorrect image order. Fix Markdown or renderer styles and repeat Steps 1–4 until every page passes.

- [ ] **Step 5: Run final artifact content and privacy checks**

Run both Markdown verifiers, then search Markdown and extracted PDF text for `ghp_`, `/Users/leslie_yuen/`, credential-like example strings, unfinished placeholders, and internal bridge error text. Expected: no matches. Open all nine source images once more and confirm their visible data matches the fixed demo inputs.

- [ ] **Step 6: Commit final PDFs**

```bash
git add output/pdf/invoice-reimbursement-user-manual-zh-cn.pdf output/pdf/invoice-reimbursement-quick-start-zh-cn.pdf
git commit -m "docs: publish shareable invoice reimbursement manuals"
```

### Task 8: End-to-End Verification and Handoff

**Files:**
- Verify: all files created in Tasks 1–7

- [ ] **Step 1: Run documentation unit tests**

```bash
/Users/leslie_yuen/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3 -m unittest discover -s scripts/tests -v
```

Expected: all verifier, diagram, and PDF tests PASS.

- [ ] **Step 2: Re-run the deterministic screenshot flow**

```bash
CAPTURE_USER_GUIDE=1 npx playwright test tests/e2e/user-guide-capture.spec.ts --project=standard-1440x900
```

Expected: 1 test PASS and no unexpected image diffs after regeneration.

- [ ] **Step 3: Run existing frontend quality gates**

```bash
npm run lint
npm test
npm run build
```

Expected: lint exits 0, all Vitest tests pass, and the production build succeeds without browser bridge markers.

- [ ] **Step 4: Re-run both content verifiers and PDF structure checks**

Run the two `verify_user_guide.py` commands from Tasks 4 and 5, `pdfinfo` on both PDFs, and the pypdf assertions from Task 7. Expected: all commands exit 0.

- [ ] **Step 5: Check repository state and commit any final documentation corrections**

Run `git diff --check` and `git status --short`. Only intentional user-guide files may remain changed. If visual QA required corrections after the prior commits, commit those exact files:

```bash
git add docs/user-guide scripts tests/e2e/user-guide-capture.spec.ts output/pdf .gitignore
git commit -m "docs: finalize invoice reimbursement user guides"
```

- [ ] **Step 6: Deliver the manuals**

Report the two Markdown paths, the two PDF paths, full/quick page counts, verification commands and results, supported environment, and the unsigned-app first-open warning. Do not claim Intel Mac support, Apple signing, notarization, App Store availability, or automatic completion of reimbursement.
