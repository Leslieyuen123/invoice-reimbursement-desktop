/// <reference types="vitest/config" />

import { readFileSync } from "node:fs";

import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The sidebar shows the version the user is running; reading it from
// package.json keeps it from drifting from the release metadata the
// alignment test already guards.
const packageVersion = (
  JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as {
    version: string;
  }
).version;

export default defineConfig({
  define: { __APP_VERSION__: JSON.stringify(packageVersion) },
  plugins: [react()],
  server: {
    port: 1420,
    strictPort: true,
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    setupFiles: ["./src/test/setup.ts"],
  },
});
