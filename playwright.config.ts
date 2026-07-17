import { existsSync } from "node:fs";

import { defineConfig, devices } from "@playwright/test";

const systemChrome =
  process.platform === "darwin"
    ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    : undefined;
const localExecutable = process.env.CI
  ? undefined
  : process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ??
    (systemChrome && existsSync(systemChrome) ? systemChrome : undefined);

const viewports = [
  { name: "compact-800x700", width: 800, height: 700 },
  { name: "standard-1440x900", width: 1440, height: 900 },
  { name: "wide-1728x1117", width: 1728, height: 1117 },
] as const;

export default defineConfig({
  testDir: "./tests/e2e",
  outputDir: "test-results/playwright",
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["github"], ["list"]] : "list",
  use: {
    baseURL: "http://127.0.0.1:1420",
    colorScheme: "dark",
    locale: "zh-CN",
    launchOptions: localExecutable
      ? { executablePath: localExecutable }
      : undefined,
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
  },
  webServer: {
    command: "npm run dev -- --host 127.0.0.1 --port 1420",
    url: "http://127.0.0.1:1420",
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    env: {
      ...process.env,
      VITE_BROWSER_COMMAND_BRIDGE: "1",
    },
  },
  projects: viewports.map(({ name, width, height }) => ({
    name,
    use: {
      ...devices["Desktop Chrome"],
      viewport: { width, height },
    },
  })),
});
