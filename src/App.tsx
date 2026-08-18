import * as React from "react";
import { Group, Panel, Separator } from "react-resizable-panels";
import Sidebar from "./components/sidebar";
import NavTree from "./components/fileTree";
import Workspace, { type OpenTab } from "./components/workspace";
import { ConnectDialog } from "./components/connectDialog";
import { ThemeProvider } from "./lib/theme";
import { type DBConnection, type TableMeta, CONNECTIONS } from "./lib/mockData";

export default function App() {
  return (
    <ThemeProvider>
      <AppInner />
    </ThemeProvider>
  );
}

function AppInner() {
  const [connection, setConnection] = React.useState<DBConnection | null>(null);
  const [tabs, setTabs] = React.useState<OpenTab[]>([]);
  const [activeId, setActiveId] = React.useState<string | null>(null);
  const [refreshSignal, setRefreshSignal] = React.useState(0);

  // While no connection: show the connection wizard.
  if (!connection) {
    return (
      <div className="h-screen w-screen bg-[var(--background)] text-foreground relative overflow-hidden">
        <div className="absolute inset-0 flex items-center justify-center">
          <SplashText />
        </div>
        <ConnectDialog
          onConnect={(conn) => {
            const idx = CONNECTIONS.findIndex((c) => c.id === conn.id);
            if (idx >= 0) CONNECTIONS[idx] = { ...conn };
            else CONNECTIONS.push({ ...conn });
            setTabs([]);
            setActiveId(null);
            setConnection({ ...conn });
          }}
          onCancel={() => {
            /* stay on wizard until connect */
          }}
        />
      </div>
    );
  }

  function openTab(meta: TableMeta) {
    if (!connection) return;
    const id = `${connection.id}:${meta.schema}:${meta.name}`;
    const existing = tabs.find((t) => t.id === id);
    if (existing) {
      setActiveId(id);
      return;
    }
    setTabs((ts) => [
      ...ts,
      {
        id,
        connectionId: connection.id,
        schema: meta.schema,
        table: meta,
      },
    ]);
    setActiveId(id);
  }

  function closeTab(id: string) {
    setTabs((ts) => {
      const idx = ts.findIndex((t) => t.id === id);
      if (idx < 0) return ts;
      const next = ts.filter((t) => t.id !== id);
      if (activeId === id) {
        setActiveId(next[Math.min(idx, next.length - 1)]?.id ?? null);
      }
      return next;
    });
  }

  function terminate() {
    const conn = connection;
    if (!conn) return;
    const idx = CONNECTIONS.findIndex((c) => c.id === conn.id);
    if (idx >= 0) CONNECTIONS[idx] = { ...conn, status: "disconnected" };
    setConnection(null);
    setTabs([]);
    setActiveId(null);
  }

  const activeTab = tabs.find((t) => t.id === activeId) ?? null;
  const selectedPath = activeTab
    ? { schema: activeTab.schema, table: activeTab.table.name }
    : undefined;

  return (
    <div className="flex h-screen w-screen bg-[var(--background)] text-foreground overflow-hidden font-sans p-2">
      {/* narrow vertical toolbar */}
      <Sidebar
        connection={connection}
        onDisconnect={terminate}
        onNewConnection={terminate}
        onRefresh={() => setRefreshSignal((n) => n + 1)}
      />

      {/* horizontal resizable panes: nav tree | workspace */}
      <Group orientation="horizontal" className="flex flex-1 min-w-0">
        <Panel
          id="nav-tree"
          defaultSize={288}
          minSize={180}
          maxSize={560}
          collapsible
          collapsedSize={0}
          className="bg-[var(--sidebar)] rounded-r-2xl"
        >
          <NavTree
            connectionId={connection.id}
            selectedPath={selectedPath}
            onPickTable={openTab}
          />
        </Panel>
        <Separator className="w-1 mx-0.5 data-[separator-active]:bg-primary data-[separator-active]:w-1.5 transition-colors" />
        <Panel id="workspace" minSize={320} className="min-w-0 rounded-2xl">
          <Workspace
            tabs={tabs}
            activeId={activeId}
            onActivate={setActiveId}
            onClose={closeTab}
            connection={connection}
            refreshSignal={refreshSignal}
          />
        </Panel>
      </Group>
    </div>
  );
}

function SplashText() {
  return (
    <div className="flex flex-col items-center gap-2 select-none opacity-40">
      <div className="font-mono text-10px uppercase tracking-[0.3em] text-muted-foreground">
        DataBridge
      </div>
      <div className="h-px w-32 bg-[var(--jb-toolbar-border)]" />
      <div className="font-mono text-10px text-muted-foreground">
        Database Explorer
      </div>
    </div>
  );
}
