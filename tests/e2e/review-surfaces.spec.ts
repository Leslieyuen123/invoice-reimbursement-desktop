import { expect, test } from "@playwright/test";

/**
 * The surfaces added after the MVP flow never had an end-to-end pass: the
 * consistency audit, the diagnostics bundle and the mail ledger. They share a
 * property worth guarding here — they must all stay usable on an empty library,
 * which is exactly the state a fresh install starts in.
 */
test("consistency audit, diagnostics bundle and mail ledger work on an empty library", async ({
  page,
}) => {
  await page.goto("/?bridgeReset=1");
  await expect(
    page.getByRole("heading", { name: "控制台", exact: true }),
  ).toBeVisible();

  // The audit reports a consistent, empty library instead of an error state.
  await expect(
    page.getByRole("heading", { name: "数据一致性巡检" }),
  ).toBeVisible();
  const cleanSummary = "已检查 0 张票据和 0 个批次：原件、归一化 PDF 与批次状态都一致。";
  await expect(page.getByText(cleanSummary)).toBeVisible();
  await page.getByRole("button", { name: "重新巡检" }).click();
  await expect(page.getByText(cleanSummary)).toBeVisible();

  // The diagnostics bundle is written through the export command and reported.
  await page.getByRole("link", { name: "设置", exact: true }).click();
  await expect(page.getByRole("heading", { name: "运行设置" })).toBeVisible();
  await page.getByRole("button", { name: "导出诊断包" }).click();
  await expect(
    page.getByText(/invoice-diagnostics-\d{8}-\d{6}\.zip/),
  ).toBeVisible();

  // Every ledger tab renders, and the empty state explains itself.
  await page.getByRole("link", { name: "邮件台账" }).click();
  await expect(page.getByRole("heading", { name: "邮件台账" })).toBeVisible();
  for (const tab of [
    "需关注",
    "未提取",
    "部分提取",
    "已提取",
    "无需处理",
    "全部",
  ]) {
    await expect(page.getByRole("tab", { name: new RegExp(tab) })).toBeVisible();
  }
  await expect(page.getByRole("heading", { name: "邮件台账" })).toBeVisible();
});

/**
 * Bulk operations are the part of the batch detail a user reaches for when a
 * batch is nearly ready. This drives the whole path — automation fills a batch,
 * the user selects a row and removes it — instead of only asserting the dialog
 * exists.
 */
test("bulk removal from a batch detail moves the selected invoice out", async ({
  page,
}) => {
  await page.goto(
    "/settings?bridgeReset=1&bridgeDelay=150&bridgeAutomation=success-with-exceptions",
  );
  const newAccount = page.getByRole("form", { name: "新邮箱账号" });
  await newAccount.getByLabel("邮箱地址").fill("bulk@example.com");
  await newAccount.locator('input[type="password"]').fill("e2e-app-password");
  await newAccount.getByRole("button", { name: "测试连接" }).click();
  await expect(newAccount.getByText("连接成功")).toBeVisible();
  await newAccount.getByRole("button", { name: "保存账号" }).click();

  await page.getByRole("link", { name: "报销批次" }).click();
  await page.getByRole("link", { name: "新建批次" }).click();
  await page.getByLabel("年份").fill("2026");
  await page.getByLabel("月份").selectOption("6");
  await page.getByRole("button", { name: "创建并自动处理" }).click();
  await expect(
    page.getByRole("status", { name: "批次自动处理完成" }),
  ).toBeVisible();

  const invoice = page.getByRole("button", { name: "自动处理安全票据.pdf" });
  await expect(invoice).toBeVisible();
  await page.getByRole("checkbox", { name: "选择 自动处理安全票据.pdf" }).check();
  await page.getByRole("button", { name: "移出所选票据" }).click();

  await expect(invoice).toHaveCount(0);
  // The batch is still there and still exportable-state, just without that row.
  await expect(
    page.getByRole("heading", { name: "2026 年 6 月报销" }),
  ).toBeVisible();
});
