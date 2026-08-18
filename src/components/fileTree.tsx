import * as React from "react"
import { HugeiconsIcon } from "@hugeicons/react"
import { cn } from "@/lib/utils"
import {
  Search01Icon, Cancel01Icon, RefreshIcon, FilterIcon,
  ChevronDownIcon, ChevronRightIcon,
  CubeIcon, Table01Icon, ViewIcon, FunctionIcon,
} from "@hugeicons/core-free-icons"
import { BrandGlyph } from "@/components/brandIcon"
import { CONNECTIONS, TREE, type TableMeta, type NodeKind, type DBType } from "@/lib/mockData"

/** kind -> icon mapping for the explorer rows */
const KIND_ICON: Record<string, typeof CubeIcon> = {
  schemas: CubeIcon,
  tables: Table01Icon,
  schema: CubeIcon,
  table: Table01Icon,
  view: ViewIcon,
  function: FunctionIcon,
  procedure: FunctionIcon,
}

interface Props {
  connectionId: string
  selectedPath?: { schema: string; table: string }
  onPickTable: (meta: TableMeta) => void
}

export default function NavTree({ connectionId, selectedPath, onPickTable }: Props) {
  const conn = CONNECTIONS.find((c) => c.id === connectionId)!
  const tree = TREE.find((t) => t.connectionId === connectionId)
  const [query, setQuery] = React.useState("")
  // initially: connection + Schemas expanded, every schema expanded open
  const [expanded, setExpanded] = React.useState<Set<string>>(() => {
    const s = new Set<string>(["conn", "schemas"])
    tree?.schemas.forEach((sch) => {
      s.add(`s:${sch.name}`)
      s.add(`ss:${sch.name}:tables`)
      s.add(`ss:${sch.name}:views`)
      s.add(`ss:${sch.name}:routines`)
    })
    return s
  })

  function toggle(id: string) {
    setExpanded((s) => {
      const ns = new Set(s)
      if (ns.has(id)) ns.delete(id)
      else ns.add(id)
      return ns
    })
  }

  // When a search query is set, auto-expand every path with matches
  React.useEffect(() => {
    if (!query.trim() || !tree) return
    setExpanded((s) => {
      const ns = new Set(s)
      ns.add("schemas")
      for (const sch of tree.schemas) {
        ns.add(`s:${sch.name}`)
        ns.add(`ss:${sch.name}:tables`)
        ns.add(`ss:${sch.name}:views`)
        ns.add(`ss:${sch.name}:routines`)
      }
      return ns
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query])

  const q = query.trim().toLowerCase()
  const match = (label: string) => !q || label.toLowerCase().includes(q)

  // Build schema groups; only keep schemas with at least one match (when filtering).
  const schemaNodes = (tree?.schemas ?? [])
    .map((sch) => {
      const tables = (sch.tables ?? []).filter((t) => match(t.name))
      const views = (sch.views ?? []).filter((v) => match(v.name))
      const funcs = (sch.functions ?? []).filter((f) => match(f.name))
      return { sch, tables, views, funcs }
    })
    .filter((node) =>
      !q ||
      match(node.sch.name) ||
      node.tables.length > 0 ||
      node.views.length > 0 ||
      node.funcs.length > 0,
    )
  // When filtering, drop empty Tables / Views / Routines groups inside each schema.
  const schemaRender = schemaNodes.map((node) => ({
    ...node,
    showTables: !q || node.tables.length > 0,
    showViews: !q || node.views.length > 0,
    showRoutines: !q || node.funcs.length > 0,
  }))

  return (
    <div className="flex h-full flex-col bg-[var(--jb-tree-bg)] text-[var(--jb-tree-fg)]">
      {/* tiny DataGrip title strip */}
      <div className="flex h-7 items-center gap-2 border-b border-[var(--jb-toolbar-border)] bg-[var(--jb-toolbar)] px-2">
        <span className="shrink-0 inline-flex"><BrandGlyph type={conn.type} size={12} /></span>
        <span className="truncate font-mono text-[11px] uppercase tracking-wider text-muted-foreground">
          {conn.name}
        </span>
      </div>

      {/* search / tools bar */}
      <div className="flex items-center gap-1.5 border-b border-[var(--jb-toolbar-border)] px-2 py-1.5">
        <div className="relative flex-1">
          <HugeiconsIcon icon={Search01Icon} size={13}
            className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-muted-foreground" />
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="schema.table…"
            className={cn(
              "h-7 w-full rounded-[2px] border border-[var(--jb-toolbar-border)] bg-[var(--input)]",
              "pl-7 pr-7 font-mono text-[12px] text-foreground placeholder:text-muted-foreground",
              "focus:border-ring focus:outline-none",
            )}
          />
          {query && (
            <button
              onClick={() => setQuery("")}
              className="absolute right-1.5 top-1/2 -translate-y-1/2 inline-flex size-4 items-center justify-center text-muted-foreground hover:text-foreground"
              aria-label="Clear filter"
            >
              <HugeiconsIcon icon={Cancel01Icon} size={12} />
            </button>
          )}
        </div>
        <button
          className="inline-flex size-7 items-center justify-center text-muted-foreground hover:bg-[var(--jb-tree-hover)] hover:text-foreground"
          aria-label="Filter options"
        >
          <HugeiconsIcon icon={FilterIcon} size={14} />
        </button>
        <button
          className="inline-flex size-7 items-center justify-center text-muted-foreground hover:bg-[var(--jb-tree-hover)] hover:text-foreground"
          aria-label="Reload schemas"
        >
          <HugeiconsIcon icon={RefreshIcon} size={14} />
        </button>
      </div>

      {/* tree body */}
      <div className="flex-1 overflow-y-auto jb-scroll py-0.5 select-none">
        <TreeRow
          depth={0}
          label={conn.database || connectionId}
          klet="connection"
          brandType={conn.type}
          open={expanded.has("conn")}
          onToggle={() => toggle("conn")}
          bold
        />
        {expanded.has("conn") && (
          <>
            <TreeRow
              depth={1}
              label="Schemas"
              klet="schemas"
              open={expanded.has("schemas")}
              onToggle={() => toggle("schemas")}
              bold
            />
            {expanded.has("schemas") && !tree && (
              <div className="px-4 py-6 font-mono text-[12px] text-muted-foreground">
                No schemas loaded for this connection.
                <br />
                (The mock data explorer has no seed entry for&nbsp;
                <span className="text-foreground">{conn.name}</span>.)
              </div>
            )}
            {expanded.has("schemas") && tree && tree.schemas.length === 0 && (
              <div className="px-4 py-6 font-mono text-[12px] text-muted-foreground">
                Schemas list is empty. {conn.status === "error" ? (
                  <span className="text-[var(--jb-connection-err)]">Last connection attempt failed.</span>
                ) : conn.status === "disconnected" ? (
                  <span>Connection not open.</span>
                ) : (
                  <span>Refresh or pick another connection.</span>
                )}
              </div>
            )}
            {expanded.has("schemas") && tree && tree.schemas.length > 0 && schemaRender.length === 0 && (
              <div className="px-4 py-6 font-mono text-[12px] text-muted-foreground">
                No matches for &ldquo;{q}&rdquo;
              </div>
            )}
            {expanded.has("schemas") && schemaRender.map(({ sch, tables, views, funcs, showTables, showViews, showRoutines }) => {
              const sid = `s:${sch.name}`
              return (
                <React.Fragment key={sid}>
                  <TreeRow
                    depth={2}
                    label={sch.name}
                    klet="schema"
                    open={expanded.has(sid)}
                    onToggle={() => toggle(sid)}
                    bold
                  />
                  {expanded.has(sid) && (
                    <>
                      {showTables && (
                        <>
                          <Group label={`Tables (${tables.length})`} klet="tables"
                            depth={3} open={expanded.has(`ss:${sch.name}:tables`)}
                            onToggle={() => toggle(`ss:${sch.name}:tables`)} />
                          {expanded.has(`ss:${sch.name}:tables`) && tables.map((t) => (
                            <TreeRow key={`t:${sch.name}:${t.name}`} depth={4} label={t.name}
                              klet="table" meta={t}
                              selected={
                                selectedPath?.schema === sch.name &&
                                selectedPath?.table === t.name
                              }
                              onSelect={() => onPickTable(t)}
                            />
                          ))}
                        </>
                      )}
                      {showViews && views.length > 0 && (
                        <>
                          <Group label={`Views (${views.length})`} klet="view"
                            depth={3} open={expanded.has(`ss:${sch.name}:views`)}
                            onToggle={() => toggle(`ss:${sch.name}:views`)} />
                          {expanded.has(`ss:${sch.name}:views`) && views.map((t) => (
                            <TreeRow key={`v:${sch.name}:${t.name}`} depth={4} label={t.name}
                              klet="view" meta={t}
                              selected={
                                selectedPath?.schema === sch.name &&
                                selectedPath?.table === t.name
                              }
                              onSelect={() => onPickTable(t)}
                            />
                          ))}
                        </>
                      )}
                      {showRoutines && funcs.length > 0 && (
                        <>
                          <Group label={`Routines (${funcs.length})`} klet="function"
                            depth={3} open={expanded.has(`ss:${sch.name}:routines`)}
                            onToggle={() => toggle(`ss:${sch.name}:routines`)} />
                          {expanded.has(`ss:${sch.name}:routines`) && funcs.map((t) => (
                            <TreeRow key={`f:${sch.name}:${t.name}`} depth={4} label={t.name}
                              klet="function" meta={t}
                              selected={
                                selectedPath?.schema === sch.name &&
                                selectedPath?.table === t.name
                              }
                              onSelect={() => onPickTable(t)}
                            />
                          ))}
                        </>
                      )}
                    </>
                  )}
                </React.Fragment>
              )
            })}
          </>
        )}
      </div>
    </div>
  )
}

/* ---------------- Illustrated env verbosity fix (use real classes) -------- */

function Group({ label, klet, depth, open, onToggle }: {
  label: string
  klet: NodeKind | "tables"
  depth: number
  open: boolean
  onToggle: () => void
}) {
  return (
    <TreeRow depth={depth} label={label} klet={klet} open={open} onToggle={onToggle} muted />
  )
}

interface RowProps {
  depth: number
  label: string
  klet: NodeKind | "schemas" | "tables" | "connection"
  open?: boolean
  onToggle?: () => void
  bold?: boolean
  muted?: boolean
  selected?: boolean
  onSelect?: () => void
  meta?: TableMeta
  brandType?: DBType
}

function TreeRow({ depth, label, klet, open, onToggle, bold, muted,
                  selected, onSelect, meta, brandType }: RowProps) {
  const isParent = klet === "schemas" || klet === "tables" || klet === "schema" || klet === "connection"
  const Icon = KIND_ICON[klet] ?? CubeIcon
  const selDirect = selected && (klet === "table" || klet === "view" || klet === "function")

  // Clicking anywhere on the row: parent rows toggle expand/collapse,
  // leaf rows select the table.
  function handleRowClick() {
    if (isParent && onToggle) onToggle()
    else if (onSelect) onSelect()
  }

  return (
    <div
      onClick={handleRowClick}
      className={cn(
        "group flex h-[22px] items-center gap-1 pr-2 font-mono text-[12.5px] transition-colors",
        "hover:bg-[var(--jb-tree-hover)] cursor-pointer",
        (muted || isParent) && "text-muted-foreground",
        selDirect && "bg-[var(--jb-tree-selected)] text-foreground",
        !selDirect && bold && "font-medium text-foreground",
      )}
      style={{ paddingLeft: depth * 14 + 2 }}
    >
      <span className="inline-flex size-4 items-center justify-center">
        {isParent && onToggle && (
          <button
            onClick={(e) => { e.stopPropagation(); onToggle() }}
            className="inline-flex size-4 items-center justify-center text-muted-foreground hover:text-foreground"
            aria-label={open ? "collapse" : "expand"}
          >
            <HugeiconsIcon icon={open ? ChevronDownIcon : ChevronRightIcon} size={12} />
          </button>
        )}
      </span>
      {klet === "connection" && brandType ? (
        <span className="shrink-0 inline-flex"><BrandGlyph type={brandType} size={14} /></span>
      ) : (
        <HugeiconsIcon icon={Icon} size={14} strokeWidth={1.4}
          className={cn("shrink-0", klet === "table" && "text-[var(--primary)]",
            klet === "view" && "text-[var(--chart-2)]",
            klet === "function" && "text-[var(--chart-3)]")} />
      )}
      <span className={cn("truncate", bold && !muted && "font-semibold")}>{label}</span>
      {meta && meta.rowCount > 0 && (
        <span className="ml-auto pl-2 text-[10px] text-muted-foreground">
          {meta.rowCount.toLocaleString("en-US")}
        </span>
      )}
    </div>
  )
}
