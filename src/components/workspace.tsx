import * as React from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Group, Panel, Separator } from "react-resizable-panels";
import { HugeiconsIcon } from "@hugeicons/react";
import { cn } from "@/lib/utils";
import {
  Table01Icon,
  RefreshIcon,
  Cancel01Icon,
  Copy01Icon,
  FilterIcon,
  Download01Icon,
  ChevronLeftIcon,
  ChevronRightIcon,
  PlayIcon,
  SourceCodeIcon,
  Settings01Icon,
  GridViewIcon,
  InformationSquareIcon,
  AlertCircleIcon,
} from "@hugeicons/core-free-icons";
import { ThemeToggle } from "@/components/themeToggle";
import {
  fetchRows,
  statusLine,
  type DBConnection,
  type TableMeta,
  type ColumnMeta,
  type Row,
} from "@/lib/mockData";

export interface OpenTab {
  id: string;
  connectionId: string;
  schema: string;
  table: TableMeta;
}

interface Props {
  tabs: OpenTab[];
  activeId: string | null;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  connection: DBConnection;
  refreshSignal: number;
}

export default function Workspace({
  tabs,
  activeId,
  onActivate,
  onClose,
  connection,
  refreshSignal,
}: Props) {
  const active = tabs.find((t) => t.id === activeId) ?? null;

  return (
    <div className="flex h-full flex-1 flex-col bg-[var(--background)] text-foreground min-w-0">
      {/* Tab strip */}
      <div className="flex h-8 items-stretch jb-tabstrip">
        <div className="flex items-stretch">
          {tabs.map((t) => (
            <div
              key={t.id}
              onClick={() => onActivate(t.id)}
              data-active={t.id === activeId}
              className="jb-tab flex min-w-44 cursor-pointer items-center gap-2 px-3 text-[12px] font-mono select-none"
            >
              <HugeiconsIcon
                icon={Table01Icon}
                size={12}
                className={cn(
                  "shrink-0",
                  t.id === activeId
                    ? "text-[var(--primary)]"
                    : "text-muted-foreground",
                )}
              />
              <span className="truncate text-foreground">{t.table.name}</span>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  onClose(t.id);
                }}
                className="ml-auto inline-flex size-4 items-center justify-center rounded-[2px] text-muted-foreground hover:bg-[var(--jb-tree-hover)] hover:text-foreground"
                aria-label="Close tab"
              >
                <HugeiconsIcon icon={Cancel01Icon} size={11} />
              </button>
            </div>
          ))}
        </div>
        {/* Theme toggle lives in the far right of the tab strip so it's reachable always */}
        <div className="flex flex-1 items-center justify-end pr-2">
          <ThemeToggle size={13} />
        </div>
      </div>

      {/* Editor body */}
      {active ? (
        <RecordsView
          key={connection.id + ":" + active.id}
          tab={active}
          connection={connection}
          refreshSignal={refreshSignal}
        />
      ) : (
        <EmptyState />
      )}
    </div>
  );
}

/* ----------------- Records table view ----------------- */

function RecordsView({
  tab,
  connection,
  refreshSignal,
}: {
  tab: OpenTab;
  connection: DBConnection;
  refreshSignal: number;
}) {
  const { schema, table } = tab;
  const [rows, setRows] = React.useState<Row[]>([]);
  const [loading, setLoading] = React.useState(false);
  const [selectedRow, setSelectedRow] = React.useState<number | null>(null);
  const [showDetail, setShowDetail] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [page, setPage] = React.useState(0);
  const PAGE = 50;
  const [elapsed, setElapsed] = React.useState(0);
  const [totalCount, setTotalCount] = React.useState(table.rowCount);

  // Column widths — user-resizable, persisted per column name
  const [colWidths, setColWidths] = React.useState<Record<string, number>>({});
  React.useEffect(() => {
    const w: Record<string, number> = {};
    for (const c of table.columns) w[c.name] = columnMinWidth(c);
    setColWidths(w);
  }, [table.name, schema]);
  const colWidth = (c: ColumnMeta) => {
    const n = colWidths[c.name];
    return n == null ? columnMinWidth(c) : Math.max(48, n);
  };

  // Hand-rolled drag-resize using Pointer Events + setPointerCapture so the
  // drag keeps tracking even when the pointer leaves the handle's hit area
  // (which happens constantly in a virtualized, sticky-headered scroll area).
  const resizingCol = React.useRef<string | null>(null);
  const resizePointerId = React.useRef<number | null>(null);
  const resizingStartX = React.useRef(0);
  const resizingStartW = React.useRef(0);
  const colWidthRef = React.useRef(colWidth);
  colWidthRef.current = colWidth;

  // global listeners mounted once — handle in-flight drags even after rerender
  React.useEffect(() => {
    function onMove(e: PointerEvent) {
      if (!resizingCol.current || resizePointerId.current !== e.pointerId)
        return;
      e.preventDefault();
      const dx = e.clientX - resizingStartX.current;
      const name = resizingCol.current;
      setColWidths((m) => ({
        ...m,
        [name]: Math.max(48, resizingStartW.current + dx),
      }));
    }
    function onUp(e: PointerEvent) {
      if (resizePointerId.current !== e.pointerId) return;
      resizingCol.current = null;
      resizePointerId.current = null;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    }
    window.addEventListener("pointermove", onMove, { passive: false });
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, []);

  function startResize(c: ColumnMeta, e: React.PointerEvent) {
    e.preventDefault();
    e.stopPropagation();
    resizingCol.current = c.name;
    resizePointerId.current = e.pointerId;
    resizingStartX.current = e.clientX;
    resizingStartW.current = colWidthRef.current(c);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    // capture so we keep the move events even if pointer leaves the knob
    try {
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    } catch {}
  }
  function resetColWidth(c: ColumnMeta) {
    setColWidths((m) => ({ ...m, [c.name]: columnMinWidth(c) }));
  }

  async function load() {
    if (connection.status !== "connected") {
      setError("connection is not active");
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const r = await fetchRows(connection.id, schema, table.name, {
        offset: page * PAGE,
        limit: PAGE,
      });
      setRows(r.rows);
      setTotalCount(r.total);
      setElapsed(r.elapsedMs);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  React.useEffect(() => {
    setPage(0);
  }, [tab.id]);
  React.useEffect(() => {
    load();
  }, [tab.id, page, refreshSignal]); // eslint-disable-line react-hooks/exhaustive-deps
  React.useEffect(() => {
    setSelectedRow(null);
  }, [rows]);

  const parentRef = React.useRef<HTMLDivElement>(null);
  const v = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 24,
    overscan: 8,
    // Rows are fixed-height (24px) — skip dynamic measurement to avoid
    // per-row ResizeObserver overhead and the "Missing data-index" warning.
    measureElement: undefined,
  });

  return (
    <div className="flex flex-1 flex-col min-h-0">
      {/* Top toolbar */}
      <div className="flex h-8 items-center gap-1 border-b border-[var(--jb-toolbar-border)] bg-[var(--jb-toolbar)] px-2">
        <ToolbarBtn label="Run" onClick={load} disabled={loading}>
          <HugeiconsIcon
            icon={PlayIcon}
            size={14}
            className="text-[var(--jb-connection-ok)]"
          />
        </ToolbarBtn>
        <ToolbarBtn label="Stop" disabled={!loading}>
          <HugeiconsIcon icon={Cancel01Icon} size={14} />
        </ToolbarBtn>
        <ToolbarBtn label="Reload" onClick={load} disabled={loading}>
          <HugeiconsIcon
            icon={RefreshIcon}
            size={14}
            className={loading ? "animate-spin" : undefined}
          />
        </ToolbarBtn>
        <Sep />
        <ToolbarBtn label="View data">
          <HugeiconsIcon icon={GridViewIcon} size={14} />
        </ToolbarBtn>
        <ToolbarBtn label="SQL console">
          <HugeiconsIcon icon={SourceCodeIcon} size={14} />
        </ToolbarBtn>
        <Sep />
        <ToolbarBtn label="Filter">
          <HugeiconsIcon icon={FilterIcon} size={14} />
        </ToolbarBtn>
        <ToolbarBtn label="Export">
          <HugeiconsIcon icon={Download01Icon} size={14} />
        </ToolbarBtn>
        <div className="flex-1" />
        <ToolbarBtn
          label="Toggle detail"
          onClick={() => setShowDetail(!showDetail)}
        >
          <HugeiconsIcon
            icon={InformationSquareIcon}
            size={14}
            className={
              showDetail ? "text-[var(--primary)]" : "text-muted-foreground"
            }
          />
        </ToolbarBtn>
        <ToolbarBtn label="Settings">
          <HugeiconsIcon icon={Settings01Icon} size={14} />
        </ToolbarBtn>
      </div>

      {/* schema.table path */}
      <div className="flex h-6 items-center gap-1.5 border-b border-[var(--jb-toolbar-border)] bg-[var(--jb-table-header)] px-3 font-mono text-[11px] text-muted-foreground">
        <span>{connection.database}</span>
        <span>·</span>
        <span className="text-foreground/80">{schema}</span>
        <span>·</span>
        <span className="text-[var(--primary)]">{table.name}</span>
        <span className="ml-2 text-[10px]">
          ({table.columns.length} columns, ~
          {table.rowCount.toLocaleString("en-US")} total)
        </span>
      </div>

      {/* Body: vertically resizable — records (top) ↔ detail (bottom) */}
      <Group orientation="vertical" className="flex flex-1 min-h-0">
        <Panel id="records" minSize={120} className="flex min-h-0">
          {/* Records table — virtualized row-scroll, resizable columns */}
          <div
            ref={parentRef}
            className="relative flex-1 overflow-auto jb-scroll min-w-0"
          >
            {/* sticky header with per-column widths and resize grooves */}
            <div
              className="sticky top-0 z-10 flex bg-[var(--jb-table-header)] border-b border-[var(--jb-table-border)]"
              style={{ minWidth: totalColWidth(table, colWidth) }}
            >
              <div className="z-20 sticky left-0 w-10 shrink-0 border-r border-[var(--jb-table-border)] bg-[var(--jb-table-header)] px-2 py-1 text-right font-mono text-[10px] text-muted-foreground">
                #
              </div>
              {table.columns.map((c) => {
                const w = colWidth(c);
                return (
                  <div
                    key={c.name}
                    className="relative flex shrink-0 items-center gap-1 overflow-visible px-2 py-1 font-mono text-[11px] border-r border-[var(--jb-table-border)] last:border-r-0 bg-[var(--jb-table-header)]"
                    style={{ width: w }}
                  >
                    <span
                      className={cn(
                        "flex-1 truncate min-w-0",
                        c.primaryKey && "font-bold text-foreground",
                      )}
                    >
                      {c.name}
                    </span>
                    {c.primaryKey && (
                      <span className="text-[9px] px-1 bg-[var(--primary)]/20 text-[var(--primary)] rounded-[1px]">
                        PK
                      </span>
                    )}
                    <span className="shrink-0 text-[9px] text-muted-foreground">
                      {c.type}
                    </span>
                    {/* resize handle — wider hit-area, above everything, not clipped by truncate */}
                    <span
                      role="separator"
                      aria-orientation="vertical"
                      aria-label={`Resize ${c.name}`}
                      title="Drag to resize; double-click to reset"
                      onPointerDown={(e) => startResize(c, e)}
                      onDoubleClick={() => resetColWidth(c)}
                      className="group absolute top-0 right-0 h-full w-4 cursor-col-resize z-30"
                      style={{ transform: "translateX(50%)" }}
                    >
                      <span className="pointer-events-none absolute inset-y-0 left-1/2 -translate-x-1/2 w-[2px] bg-transparent group-hover:bg-[var(--primary)] group-active:bg-[var(--primary)] transition-colors" />
                    </span>
                  </div>
                );
              })}
            </div>

            {loading && rows.length === 0 && (
              <div className="flex items-center justify-center py-10 text-muted-foreground font-mono text-[12px]">
                <HugeiconsIcon
                  icon={RefreshIcon}
                  size={14}
                  className="mr-2 animate-spin"
                />{" "}
                Loading…
              </div>
            )}
            {error && (
              <div className="flex items-center gap-2 m-2 px-2 py-1.5 font-mono text-[12px] border border-[var(--jb-connection-err)]/40 bg-[var(--jb-connection-err)]/10 text-[var(--jb-connection-err)]">
                <HugeiconsIcon icon={AlertCircleIcon} size={14} /> {error}
              </div>
            )}

            <div
              style={{
                height: v.getTotalSize() + "px",
                position: "relative",
                minWidth: totalColWidth(table, colWidth),
              }}
            >
              {v.getVirtualItems().map((vi) => {
                const row = rows[vi.index];
                const isSel = selectedRow === vi.index;
                const isAlt = vi.index % 2 === 1;
                return (
                  <div
                    key={vi.key}
                    data-index={vi.index}
                    onClick={() => { setSelectedRow(vi.index); setShowDetail(true) }}
                    className={cn(
                      "absolute left-0 top-0 flex w-full cursor-default border-b border-[var(--jb-table-border)]",
                      isSel
                        ? "bg-[var(--jb-table-selected)]"
                        : "bg-[var(--jb-table-row-alt)]",
                    )}
                    style={{
                      transform: `translateY(${vi.start}px)`,
                      minWidth: totalColWidth(table, colWidth),
                    }}
                  >
                    <div
                      className="z-10 sticky left-0 w-10 shrink-0 border-r border-[var(--jb-table-border)] px-2 py-0.5 text-right font-mono text-[10px] text-muted-foreground"
                      style={{ background: "var(--jb-table-row-alt)" }}
                    >
                      {vi.index + 1 + page * PAGE}
                    </div>
                    {table.columns.map((c) => {
                      const vv = row?.[c.name];
                      return (
                        <div
                          key={c.name}
                          className={cn(
                            "shrink-0 truncate px-2 py-0.5 font-mono text-[12px] border-r border-[var(--jb-table-border)] last:border-r-0",
                            c.nullable &&
                              vv == null &&
                              "text-muted-foreground/60 italic",
                          )}
                          style={{ width: colWidth(c) }}
                          title={String(vv ?? "")}
                        >
                          <CellValue
                            v={vv}
                            kind={c.kind}
                            nullable={c.nullable}
                          />
                        </div>
                      );
                    })}
                  </div>
                );
              })}
            </div>
            {!loading && rows.length === 0 && !error && (
              <div className="flex items-center justify-center py-10 text-muted-foreground font-mono text-[12px]">
                No rows
              </div>
            )}
          </div>
        </Panel>

        {/* Detail panel — height is user-resizable, collapsible */}
        {showDetail && (
          <>
            <Separator className="h-1 bg-[var(--jb-toolbar-border)] hover:bg-primary data-[separator-active]:bg-primary transition-colors" />
            <Panel
              id="detail"
              defaultSize={180}
              minSize={48}
              maxSize={480}
              collapsible
              collapsedSize={0}
              className="bg-[var(--jb-toolbar)]"
            >
              {selectedRow !== null && rows[selectedRow] ? (
                <DetailPanel
                  row={rows[selectedRow]}
                  cols={table.columns}
                  index={selectedRow + 1}
                  page={page}
                  pageSize={PAGE}
                />
              ) : (
                <div className="flex h-full items-center justify-center px-3 font-mono text-[11px] text-muted-foreground">
                  Click a row to inspect its fields.
                </div>
              )}
            </Panel>
          </>
        )}
      </Group>

      {/* status bar */}
      <StatusBar
        page={page}
        pageCount={Math.max(1, Math.ceil(totalCount / PAGE))}
        pageSize={PAGE}
        rowsOnPage={rows.length}
        elapsedMs={elapsed}
        total={totalCount}
        loading={loading}
        connection={connection}
        onPrev={() => setPage(Math.max(0, page - 1))}
        onNext={() => setPage((p) => ((p + 1) * PAGE < totalCount ? p + 1 : p))}
      />
    </div>
  );
}

/* ----------------- helpers ----------------- */

function totalColWidth(t: TableMeta, w: (c: ColumnMeta) => number) {
  const n = t.columns.reduce((acc, c) => acc + w(c), 0) + 10;
  return n;
}

function columnMinWidth(c: ColumnMeta): number {
  switch (c.kind) {
    case "json":
      return 220;
    case "text":
      return 150;
    case "date":
      return 180;
    case "uuid":
      return 240;
    case "binary":
      return 110;
    case "float":
      return 120;
    case "int":
      return 90;
    case "bool":
      return 64;
    default:
      return 100;
  }
}

function CellValue({
  v,
  kind,
  nullable,
}: {
  v: unknown;
  kind: ColumnMeta["kind"];
  nullable: boolean;
}) {
  if (v === null || v === undefined) {
    if (!nullable)
      return <span className="italic text-muted-foreground">NULL</span>;
    return <span className="italic text-muted-foreground/40">null</span>;
  }
  let color = "text-foreground";
  let body = String(v);
  switch (kind) {
    case "int":
    case "float":
      color = "text-[var(--jb-number)]";
      break;
    case "bool":
      color = "text-[var(--jb-keyword)]";
      body = v ? "true" : "false";
      break;
    case "json":
      color = "text-[var(--jb-string)]";
      break;
    case "date":
      color = "text-[var(--chart-4)]";
      break;
    case "uuid":
      color = "text-[var(--chart-3)]";
      break;
    case "binary":
      color = "text-[var(--chart-5)]";
      break;
    case "text":
      color = "text-[var(--jb-string)]";
      body = `"${body}"`;
      break;
  }
  return <span className={color}>{body}</span>;
}

function DetailPanel({
  row,
  cols,
  index,
  page,
  pageSize,
}: {
  row: Row;
  cols: ColumnMeta[];
  index: number;
  page: number;
  pageSize: number;
}) {
  const [copied, setCopied] = React.useState<number | null>(null);
  function copy(i: number, val: unknown) {
    navigator.clipboard?.writeText(val === null ? "null" : String(val));
    setCopied(i);
    setTimeout(() => setCopied(null), 800);
  }
  return (
    <div className="flex h-full flex-col overflow-hidden bg-[var(--card)]">
      <div className="flex-none px-3 py-1 font-mono text-[10px] uppercase tracking-wider text-muted-foreground border-b border-[var(--jb-table-border)] bg-[var(--jb-table-header)]">
        Row {index + page * pageSize} · {cols.length} fields
      </div>
      <div className="flex-1 overflow-y-auto jb-scroll">
        <table className="w-full border-collapse font-mono text-[12px]">
          <tbody>
            {cols.map((c, i) => (
              <tr key={c.name} className="hover:bg-[var(--jb-tree-hover)]">
                <td className="w-48 shrink-0 border-r border-[var(--jb-table-border)] px-2 py-0.5">
                  <div className="flex items-center gap-1.5">
                    {c.primaryKey && (
                      <span className="text-[9px] px-1 bg-[var(--primary)]/20 text-[var(--primary)] rounded-[1px]">
                        PK
                      </span>
                    )}
                    <span className="font-semibold text-foreground">
                      {c.name}
                    </span>
                    <span className="ml-auto text-[10px] text-muted-foreground">
                      {c.type}
                    </span>
                    {c.nullable && (
                      <span className="text-[9px] text-muted-foreground">
                        null?
                      </span>
                    )}
                  </div>
                </td>
                <td className="px-2 py-0.5">
                  <div className="flex items-center gap-2">
                    <div className="flex-1 truncate">
                      <CellValue
                        v={row[c.name]}
                        kind={c.kind}
                        nullable={c.nullable}
                      />
                    </div>
                    <button
                      onClick={() => copy(i, row[c.name])}
                      className="inline-flex size-5 items-center justify-center text-muted-foreground hover:text-foreground"
                      aria-label="Copy cell value"
                    >
                      {copied === i ? (
                        <span className="text-[9px] text-[var(--jb-connection-ok)]">
                          ✓ copied
                        </span>
                      ) : (
                        <HugeiconsIcon icon={Copy01Icon} size={11} />
                      )}
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function StatusBar({
  page,
  pageCount,
  pageSize,
  rowsOnPage,
  elapsedMs,
  total,
  loading,
  connection,
  onPrev,
  onNext,
}: {
  page: number;
  pageCount: number;
  pageSize: number;
  rowsOnPage: number;
  elapsedMs: number;
  total: number;
  loading: boolean;
  connection: DBConnection;
  onPrev: () => void;
  onNext: () => void;
}) {
  const lower = rowsOnPage > 0 ? page * pageSize + 1 : 0;
  const upper = page * pageSize + rowsOnPage;
  return (
    <div className="flex h-6 flex-none items-center gap-2 bg-[var(--jb-statusbar)] px-3 font-mono text-[10px] text-[var(--jb-statusbar-fg)] border-t border-[var(--jb-toolbar-border)] bg-[var(--jb-toolbar)]">
      <span className="text-[var(--jb-connection-ok)]">●</span>
      <span>
        {connection.host}
        {connection.port ? `:${connection.port}` : ""}
      </span>
      <span>·</span>
      <span>
        {loading
          ? "fetching…"
          : `${lower}-${upper} of ${total.toLocaleString("en-US")}`}
      </span>
      <span>·</span>
      <span>{loading ? "—" : statusLine(elapsedMs, rowsOnPage, total)}</span>
      <div className="flex-1" />
      <button
        onClick={onPrev}
        disabled={page === 0}
        className={cn(
          "inline-flex size-5 items-center justify-center",
          page === 0 ? "opacity-40" : "hover:text-foreground",
        )}
      >
        <HugeiconsIcon icon={ChevronLeftIcon} size={11} />
      </button>
      <span>
        Page {page + 1} / {pageCount}
      </span>
      <button
        onClick={onNext}
        disabled={page >= pageCount - 1}
        className={cn(
          "inline-flex size-5 items-center justify-center",
          page >= pageCount - 1 ? "opacity-40" : "hover:text-foreground",
        )}
      >
        <HugeiconsIcon icon={ChevronRightIcon} size={11} />
      </button>
    </div>
  );
}

function EmptyState() {
  return (
    <div className="flex flex-1 items-center justify-center text-center">
      <div className="max-w-sm space-y-3">
        <HugeiconsIcon
          icon={Table01Icon}
          size={42}
          strokeWidth={1.2}
          className="mx-auto text-muted-foreground/70"
        />
        <h2 className="font-mono text-[14px] font-medium text-foreground">
          No open data view
        </h2>
        <p className="text-[12px] text-muted-foreground">
          Select a table in the explorer on the left to open its records here.
        </p>
      </div>
    </div>
  );
}

function ToolbarBtn({
  label,
  children,
  onClick,
  disabled,
}: {
  label: string;
  children: React.ReactNode;
  onClick?: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={label}
      aria-label={label}
      className={cn(
        "jb-sidebtn inline-flex h-7 w-7 items-center justify-center rounded-[2px]",
        "disabled:opacity-40 disabled:hover:bg-transparent",
      )}
    >
      {children}
    </button>
  );
}

const Sep = () => (
  <span className="mx-0.5 h-5 w-px bg-[var(--jb-toolbar-border)]" />
);
