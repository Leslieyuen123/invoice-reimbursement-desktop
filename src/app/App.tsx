import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useState } from "react";
import {
  BrowserRouter,
  Navigate,
  Route,
  Routes,
} from "react-router-dom";

import { DashboardPage } from "../features/dashboard/DashboardPage";
import { AppShell } from "./AppShell";

function PlaceholderPage({ title }: { title: string }) {
  return (
    <section className="placeholder-page">
      <h1>{title}</h1>
      <div className="placeholder-empty">暂无可显示内容</div>
    </section>
  );
}

export function App() {
  const [queryClient] = useState(
    () =>
      new QueryClient({
        defaultOptions: {
          queries: {
            retry: false,
            staleTime: 10_000,
          },
          mutations: { retry: false },
        },
      }),
  );

  return (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <Routes>
          <Route element={<AppShell />}>
            <Route index element={<DashboardPage />} />
            <Route path="inbox" element={<PlaceholderPage title="待处理池" />} />
            <Route
              path="batches"
              element={<PlaceholderPage title="报销批次" />}
            />
            <Route
              path="batches/new"
              element={<PlaceholderPage title="新建批次" />}
            />
            <Route
              path="batches/:batchId"
              element={<PlaceholderPage title="批次详情" />}
            />
            <Route path="settings" element={<PlaceholderPage title="设置" />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
