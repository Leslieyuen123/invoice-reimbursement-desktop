import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./app/App";

const rootElement = document.getElementById("root");

if (!rootElement) {
  throw new Error("Root element was not found");
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

void bootstrap();
