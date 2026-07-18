import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./app/App";

const rootElement = document.getElementById("root");

if (!rootElement) {
  throw new Error("Root element was not found");
}

export function renderBootstrapFailure(root: HTMLElement, _error: unknown) {
  void _error;
  const alert = document.createElement("div");
  alert.className = "bootstrap-fatal";
  alert.setAttribute("role", "alert");
  const title = document.createElement("strong");
  title.textContent = "应用无法启动";
  const guidance = document.createElement("span");
  guidance.textContent = "请退出后重新打开应用；如果问题持续，请保留当前版本信息。";
  alert.append(title, guidance);
  root.replaceChildren(alert);
}

async function bootstrap() {
  if (
    import.meta.env.DEV &&
    import.meta.env.VITE_BROWSER_COMMAND_BRIDGE === "1"
  ) {
    const { installBrowserCommandBridge } = await import(
      "./test/browserCommandBridge"
    );
    installBrowserCommandBridge();
  }

  createRoot(rootElement!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void bootstrap().catch((error: unknown) => {
  renderBootstrapFailure(rootElement, error);
});
