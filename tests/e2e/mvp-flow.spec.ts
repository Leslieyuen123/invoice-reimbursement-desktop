import { expect, test, type Page, type TestInfo } from "@playwright/test";

const fixture = "src-tauri/tests/fixtures/text-invoice.pdf";

async function attachView(page: Page, testInfo: TestInfo, name: string) {
  const path = testInfo.outputPath(`${name}.png`);
  const viewport = page.viewportSize();
  if (viewport) await page.mouse.move(viewport.width - 8, 8);
  await page.screenshot({ path, fullPage: true, animations: "disabled" });
  await testInfo.attach(name, { path, contentType: "image/png" });
}

async function auditCurrentView(page: Page) {
  const issues = await page.evaluate(() => {
    const visible = (element: Element) => {
      const node = element as HTMLElement;
      const style = getComputedStyle(node);
      const rect = node.getBoundingClientRect();
      return (
        style.display !== "none" &&
        style.visibility !== "hidden" &&
        rect.width > 0 &&
        rect.height > 0
      );
    };
    const documentElement = document.documentElement;
    const horizontalOverflow =
      documentElement.scrollWidth > documentElement.clientWidth + 1
        ? [`document ${documentElement.scrollWidth}px > ${documentElement.clientWidth}px`]
        : [];
    const wrappedButtons = Array.from(document.querySelectorAll("button, a.button"))
      .filter(visible)
      .flatMap((element) => {
        const textNodes = Array.from(element.childNodes).filter(
          (node) => node.nodeType === Node.TEXT_NODE && node.textContent?.trim(),
        );
        const lines = textNodes.flatMap((node) => {
          const range = document.createRange();
          range.selectNodeContents(node);
          return Array.from(range.getClientRects());
        });
        const distinctTops = new Set(lines.map((rect) => Math.round(rect.top)));
        return distinctTops.size > 1
          ? [`wrapped button: ${(element.textContent ?? "").trim()}`]
          : [];
      });
    return [...horizontalOverflow, ...wrappedButtons];
  });
  expect(issues).toEqual([]);
}

async function expectKeyboardFocusVisible(page: Page) {
  await page.locator("body").click({ position: { x: 1, y: 1 } });
  await page.keyboard.press("Tab");
  const focus = await page.evaluate(() => {
    const element = document.activeElement as HTMLElement | null;
    if (!element) return { tag: "none", visible: false };
    const style = getComputedStyle(element);
    return {
      tag: element.tagName,
      visible:
        (style.outlineStyle !== "none" && Number.parseFloat(style.outlineWidth) > 0) ||
        style.boxShadow !== "none",
    };
  });
  expect(focus.tag).not.toBe("BODY");
  expect(focus.visible).toBe(true);
}

test("manual invoice to exported reimbursement package", async ({ page }, testInfo) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "控制台", exact: true })).toBeVisible();
  await auditCurrentView(page);
  await attachView(page, testInfo, "dashboard");
  await expectKeyboardFocusVisible(page);

  await page.getByRole("link", { name: "待处理池" }).click();
  await expect(page.getByRole("heading", { name: "待处理池" })).toBeVisible();
  await auditCurrentView(page);
  await attachView(page, testInfo, "inbox");

  await page.getByLabel("选择票据文件").setInputFiles(fixture);
  await expect(page.getByText("已导入：text-invoice.pdf")).toBeVisible();
  const invoiceRow = page.getByRole("button", { name: "text-invoice.pdf" });
  await expect(invoiceRow).toBeVisible();
  const previewResponse = page.waitForResponse(
    (response) =>
      response.url().endsWith("/src-tauri/tests/fixtures/image-invoice.png") &&
      response.headers()["content-type"]?.startsWith("image/png"),
  );
  await invoiceRow.click();
  expect((await previewResponse).ok()).toBe(true);
  await expect(page.getByRole("dialog", { name: "票据详情" })).toBeVisible();
  await expect(page.getByTitle("票据预览")).toHaveAttribute(
    "src",
    "/src-tauri/tests/fixtures/image-invoice.png",
  );
  await page.getByRole("radio", { name: "餐饮" }).check();
  const amountInput = page.getByLabel("金额", { exact: true });
  await amountInput.focus();
  await expect
    .poll(() =>
      amountInput.evaluate((element) => {
        const style = getComputedStyle(element);
        return style.outlineStyle !== "none" && Number.parseFloat(style.outlineWidth) > 0;
      }),
    )
    .toBe(true);
  await auditCurrentView(page);
  await attachView(page, testInfo, "item-detail");
  await page.getByRole("button", { name: "保存并确认" }).click();
  await expect(
    invoiceRow.locator("xpath=ancestor::tr").getByText("可纳入批次", { exact: true }),
  ).toBeVisible();

  await page.getByRole("link", { name: "报销批次" }).click();
  await page.getByRole("link", { name: "新建批次" }).click();
  await page.getByRole("button", { name: "创建批次" }).click();
  await expect(page.getByRole("heading", { name: /报销/ })).toBeVisible();
  await page.getByRole("button", { name: "调整票据" }).click();
  await expect(page.getByRole("dialog", { name: "调整票据归属" })).toBeVisible();
  await auditCurrentView(page);
  await page.getByRole("button", { name: "关闭调整票据" }).click();
  await page.getByRole("button", { name: "加入推荐票据" }).click();
  await expect(page.getByText("text-invoice.pdf")).toBeVisible();
  await page.getByRole("button", { name: "导出报销包" }).click();
  await expect(page.getByText("报销包已生成")).toBeVisible();
  for (const file of ["merged.pdf", "reimbursement.xlsx", "originals.zip", "manifest.json"]) {
    await expect(page.getByText(file, { exact: true })).toBeVisible();
  }
  await page.getByRole("button", { name: "在文件夹中显示" }).click();
  await expect(
    page.getByRole("alert").filter({ hasText: "无法在文件夹中显示" }),
  ).toBeVisible();
  await auditCurrentView(page);
  await page.getByRole("heading", { name: "导出", exact: true }).scrollIntoViewIfNeeded();
  await attachView(page, testInfo, "batch-export");
  await page.getByRole("heading", { name: /报销/ }).scrollIntoViewIfNeeded();
  await attachView(page, testInfo, "batch-detail");

  await page.getByRole("link", { name: "设置" }).click();
  await expect(page.getByRole("heading", { name: "运行设置" })).toBeVisible();
  await auditCurrentView(page);
  await attachView(page, testInfo, "settings");

  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.reload();
  await expect(page.getByRole("heading", { name: "运行设置" })).toBeVisible();
  const longAnimations = await page.evaluate(() =>
    document
      .getAnimations()
      .map((animation) => Number(animation.effect?.getTiming().duration ?? 0))
      .filter((duration) => duration > 20),
  );
  expect(longAnimations).toEqual([]);
});

test("loading, empty, and error states remain operable", async ({ page }) => {
  await page.goto("/inbox?bridgeDelay=400");
  await expect(page.getByRole("status", { name: "正在加载待处理池" })).toBeVisible();
  await auditCurrentView(page);

  await page.goto("/inbox");
  await expect(page.getByText("当前筛选下没有票据")).toBeVisible();
  await auditCurrentView(page);

  await page.goto("/inbox?bridgeError=list_items");
  await expect(page.getByRole("alert").filter({ hasText: "Simulated browser command failure" })).toBeVisible();
  await auditCurrentView(page);

  await page.goto("/batches?bridgeError=list_batches");
  await expect(
    page.getByRole("alert").filter({ hasText: "Simulated browser command failure" }),
  ).toBeVisible();
  await auditCurrentView(page);

  await page.goto("/settings?bridgeError=list_mailbox_accounts");
  await expect(page.getByRole("alert").filter({ hasText: "无法加载邮箱设置" })).toBeVisible();
  await auditCurrentView(page);

  await page.goto("/?bridgeError=get_dashboard");
  await expect(page.getByRole("heading", { name: "控制台无法加载" })).toBeVisible();
  await auditCurrentView(page);
});
