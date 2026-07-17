import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useState } from "react";
import {
  BrowserRouter,
  Navigate,
  Route,
  Routes,
} from "react-router-dom";

import { DashboardPage } from "../features/dashboard/DashboardPage";
import { BatchDetailPage } from "../features/batches/BatchDetailPage";
import { BatchListPage } from "../features/batches/BatchListPage";
import { CreateBatchDialog } from "../features/batches/CreateBatchDialog";
import { InboxPage } from "../features/inbox/InboxPage";
import { SettingsPage } from "../features/settings/SettingsPage";
import { AppShell } from "./AppShell";

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
            <Route path="inbox" element={<InboxPage />} />
            <Route path="batches" element={<BatchListPage />} />
            <Route path="batches/new" element={<CreateBatchDialog />} />
            <Route path="batches/:batchId" element={<BatchDetailPage />} />
            <Route path="settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
