import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import Layout from "./components/Layout";
import { APP_VARIANT } from "./lib/api";
import { Login, SetupGate } from "./pages/Auth";
import ClientsPage from "./pages/Clients";
import DeploymentsPage from "./pages/Deployments";
import KeysPage from "./pages/Keys";
import ModelsPage from "./pages/Models";
import MonitoringPage from "./pages/Monitoring";
import OverviewPage from "./pages/Overview";
import ProjectsPage from "./pages/Projects";
import SchedulerPage from "./pages/Scheduler";
import SettingsPage from "./pages/Settings";
import UsersPage from "./pages/Users";
import BackendsPage from "./pages/client/Backends";
import ConnectionPage from "./pages/client/Connection";
import RequestsPage from "./pages/client/Requests";
import SharingPage from "./pages/client/Sharing";
import ClientStatusPage from "./pages/client/Status";

function ServerRoutes() {
  return (
    <SetupGate>
      <Routes>
        <Route path="/login" element={<Login />} />
        <Route element={<Layout />}>
          <Route index element={<OverviewPage />} />
          <Route path="clients" element={<ClientsPage />} />
          <Route path="models" element={<ModelsPage />} />
          <Route path="deployments" element={<DeploymentsPage />} />
          <Route path="scheduler" element={<SchedulerPage />} />
          <Route path="projects" element={<ProjectsPage />} />
          <Route path="keys" element={<KeysPage />} />
          <Route path="users" element={<UsersPage />} />
          <Route path="monitoring" element={<MonitoringPage />} />
          <Route path="settings" element={<SettingsPage />} />
        </Route>
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </SetupGate>
  );
}

function ClientRoutes() {
  return (
    <Routes>
      <Route element={<Layout />}>
        <Route index element={<ClientStatusPage />} />
        <Route path="backends" element={<BackendsPage />} />
        <Route path="sharing" element={<SharingPage />} />
        <Route path="requests" element={<RequestsPage />} />
        <Route path="connection" element={<ConnectionPage />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}

export default function App() {
  return (
    <BrowserRouter basename="/ui">
      {APP_VARIANT === "client" ? <ClientRoutes /> : <ServerRoutes />}
    </BrowserRouter>
  );
}
