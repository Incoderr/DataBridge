import * as React from "react"
import { Dialog, DialogContent, DialogHeader, DialogBody, DialogFooter,
  DialogTitle, DialogDescription } from "@/components/ui/dialog"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { cn } from "@/lib/utils"
import { HugeiconsIcon } from "@hugeicons/react"
import {
  Database01Icon, CloudCheckIcon, AlertCircleIcon,
  RefreshIcon, ArrowDataTransferHorizontalIcon,
} from "@hugeicons/core-free-icons"
import { BrandGlyph } from "@/components/brandIcon"
import {
  CONNECTIONS, type DBType, type DBConnection,
} from "@/lib/mockData"

export type DraftConnection = Omit<DBConnection, "status" | "latencyMs">

export type ConnectResult =
  | { ok: true; connection: DBConnection }
  | { ok: false; reason: string }

interface Props {
  onConnect: (conn: DBConnection) => void
  onCancel?: () => void
}

interface DBTypeDef {
  id: DBType
  label: string
  defaultPort: number
  brandColor: string
}

const DB_TYPES: DBTypeDef[] = [
  { id: "postgresql", label: "PostgreSQL", defaultPort: 5432, brandColor: "#4169e1" },
  { id: "mysql",      label: "MySQL",      defaultPort: 3306, brandColor: "#4479a1" },
  { id: "sqlite",     label: "SQLite",     defaultPort: 0,    brandColor: "#003b57" },
  { id: "mongodb",    label: "MongoDB",    defaultPort: 27017, brandColor: "#47a248" },
  { id: "redis",      label: "Redis",      defaultPort: 6379, brandColor: "#ff4438" },
]

// path removed; BrandGlyph is now imported from "@/components/brandIcon"

export function ConnectDialog({ onConnect, onCancel }: Props) {
  // choose existing vs fresh
  const [phase, setPhase] = React.useState<"choose" | "form" | "testing">("choose")
  const [draft, setDraft] = React.useState<DraftConnection>(() => emptyDraft("postgresql"))
  const [error, setError] = React.useState<string | null>(null)
  const [testing, setTesting] = React.useState(false)
  const [chosenExisting, setChosenExisting] = React.useState<string | null>(null)

  function pickType(t: DBType) {
    const def = DB_TYPES.find((d) => d.id === t)!
    setDraft((d) => ({ ...d, type: t, port: def.defaultPort, database: t === "sqlite" ? "/path/to/db.sqlite" : d.database }))
    setError(null)
    setPhase("form")
  }

  function startExisting(id: string) {
    const conn = CONNECTIONS.find((c) => c.id === id)!
    setDraft({
      id: conn.id, name: conn.name, type: conn.type, host: conn.host,
      port: conn.port, database: conn.database, user: conn.user,
    })
    setChosenExisting(id)
    setError(null)
    setPhase("form")
  }

  async function testConnection(): Promise<ConnectResult> {
    const d = draft
    await new Promise((r) => setTimeout(r, 600))
    if (!d.host && d.type !== "sqlite") return { ok: false, reason: "Host is required" }
    if (!d.database) return { ok: false, reason: "Database name/path is required" }
    // simulate auth error for the mongo slot
    if (d.type === "mongodb" && d.user === "") {
      return { ok: false, reason: "Authentication failed (401): unauthorized" }
    }
    if (d.host === "staging-db.acme.io") {
      return { ok: false, reason: "Connection refused: server unreachable" }
    }
    return { ok: true, connection: {
      ...d, status: "connected", latencyMs: 8 + Math.floor(Math.random() * 50),
    } }
  }

  async function handleTest() {
    setTesting(true)
    setError(null)
    const res = await testConnection()
    setTesting(false)
    if (!res.ok) setError(res.reason)
    else setError("Test connection: SUCCESS")
  }

  async function handleFinalConnect() {
    setPhase("testing")
    setError(null)
    setTesting(true)
    const res = await testConnection()
    setTesting(false)
    if (res.ok) onConnect(res.connection)
    else { setError(res.reason); setPhase("form") }
  }

  function backToChoose() {
    setPhase("choose")
    setChosenExisting(null)
    setError(null)
  }

  function handleClose() {
    if (onCancel) onCancel()
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => { if (!open && phase !== "testing") handleClose() }}
      disablePointerDismissal={phase === "testing"}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <HugeiconsIcon icon={Database01Icon} size={18} strokeWidth={1.6} className="text-primary" />
            New Connection — DataBridge
          </DialogTitle>
          <DialogDescription>
            {phase === "choose" && "Choose a database type to connect, or pick an existing connection below."}
            {phase === "form"   && "Configure connection properties. Click Test Connection to verify."}
            {phase === "testing" && "Establishing connection…"}
          </DialogDescription>
        </DialogHeader>

        <DialogBody className="pt-1">
          {phase === "choose" && (
            <ChoosePhase onPick={pickType} onPickExisting={startExisting} />
          )}

          {phase === "form" && (
            <FormPhase
              draft={draft}
              setDraft={setDraft}
              error={error}
              chosenExisting={chosenExisting}
            />
          )}

          {phase === "testing" && (
            <div className="flex flex-col items-center justify-center gap-3 py-10 text-center">
              <HugeiconsIcon icon={RefreshIcon} size={28} strokeWidth={1.6}
                className="text-primary animate-spin" />
              <div className="font-mono text-[13px] text-muted-foreground">
                Connecting to <span className="text-foreground">{draft.host || "local file"}</span>
                {draft.port ? `:${draft.port}` : ""} …
              </div>
            </div>
          )}
        </DialogBody>

        {phase === "form" && (
          <DialogFooter>
            <Button variant="ghost" size="sm" onClick={backToChoose} disabled={testing}>
              Back
            </Button>
            <div className="flex-1" />
            <Button variant="outline" size="sm" onClick={handleTest} disabled={testing}>
              {testing ? <HugeiconsIcon icon={RefreshIcon} size={14} className="animate-spin" /> : null}
              Test Connection
            </Button>
            <Button size="sm" onClick={handleFinalConnect} disabled={testing}>
              <HugeiconsIcon icon={CloudCheckIcon} size={14} />
              Connect
            </Button>
          </DialogFooter>
        )}
      </DialogContent>
    </Dialog>
  )
}

// ---------- choose phase ----------

function ChoosePhase({ onPick, onPickExisting }: {
  onPick: (t: DBType) => void
  onPickExisting: (id: string) => void
}) {
  return (
    <div className="space-y-5">
      <div>
        <h3 className="mb-2 font-mono text-[11px] uppercase tracking-wider text-muted-foreground">
          Database type
        </h3>
        <div className="grid grid-cols-3 gap-2">
          {DB_TYPES.map((t) => (
            <button
              key={t.id}
              onClick={() => onPick(t.id)}
              className={cn(
                "rounded-lg group flex flex-col items-start gap-1.5 border border-border bg-card p-3 text-left",
                "transition-colors hover:border-primary/70 hover:bg-[var(--accent)]",
              )}
            >
              <span className="group-hover:scale-110 transition-transform inline-flex">
                <BrandGlyph type={t.id} size={22} />
              </span>
              <span className="text-[13px] font-medium text-foreground">{t.label}</span>
              <span className="text-[11px] text-muted-foreground">
                {t.defaultPort ? `port ${t.defaultPort}` : "embedded file"}
              </span>
            </button>
          ))}
        </div>
      </div>

      <div>
        <h3 className="mb-2 font-mono text-[11px] uppercase tracking-wider text-muted-foreground">
          Existing connections
        </h3>
        <div className="space-y-1.5">
          {CONNECTIONS.map((c) => (
            <button
              key={c.id}
              onClick={() => onPickExisting(c.id)}
              className={cn(
                "rounded-lg flex w-full items-center gap-3 border border-transparent px-3 py-2 text-left transition-colors",
                "hover:bg-[var(--accent)] hover:border-border",
              )}
            >
              <StatusDot status={c.status} />
              <span className="shrink-0 inline-flex">
                <BrandGlyph type={c.type} size={16} />
              </span>
              <div className="flex min-w-0 flex-1 flex-col">
                <span className="truncate font-mono text-[13px] text-foreground">{c.name}</span>
                <span className="truncate text-[11px] text-muted-foreground">
                  {c.type} · {c.host}{c.port ? `:${c.port}` : ""} · {c.database}
                </span>
              </div>
              <HugeiconsIcon icon={ArrowDataTransferHorizontalIcon} size={14}
                className="text-muted-foreground" />
            </button>
          ))}
        </div>
      </div>
    </div>
  )
}

// ---------- form phase ----------

function FormPhase({ draft, setDraft, error, chosenExisting }: {
  draft: DraftConnection
  setDraft: React.Dispatch<React.SetStateAction<DraftConnection>>
  error: string | null
  chosenExisting: string | null
}) {
  const isEmbedded = draft.type === "sqlite"
  const dbType = DB_TYPES.find((d) => d.id === draft.type)?.label ?? draft.type

  function up<K extends keyof DraftConnection>(key: K, val: DraftConnection[K]) {
    setDraft((d) => ({ ...d, [key]: val }))
  }

  return (
    <div className="space-y-3">
      {chosenExisting && (
        <div className="mb-1 flex items-center gap-2 rounded-[2px] border border-primary/30 bg-primary/10 px-2.5 py-1.5 text-[11px] font-mono text-primary/90">
          <HugeiconsIcon icon={Database01Icon} size={12} />
          editing connection: {CONNECTIONS.find((c) => c.id === chosenExisting)?.name}
        </div>
      )}

      <Field label="Name">
        <Input
          value={draft.name}
          placeholder="my_database"
          onChange={(e) => up("name", e.target.value)}
          className="h-8 font-mono text-[13px] rounded-[2px]"
        />
      </Field>

      <div className="mb-2 inline-flex items-center gap-1.5 rounded-[2px] bg-[var(--accent)] px-1.5 py-0.5 font-mono text-[11px] uppercase tracking-wider text-primary">
        <span className="inline-flex"><BrandGlyph type={draft.type} size={12} /></span>
        <span>{dbType}</span>
      </div>

      {!isEmbedded ? (
        <div className="grid grid-cols-3 gap-2.5">
          <div className="col-span-2">
            <Field label="Host">
              <Input
                value={draft.host}
                placeholder="localhost"
                onChange={(e) => up("host", e.target.value)}
                className="h-8 font-mono text-[13px] rounded-[2px]"
              />
            </Field>
          </div>
          <Field label="Port">
            <Input
              type="number"
              value={draft.port || ""}
              placeholder="5432"
              onChange={(e) => up("port", Number(e.target.value))}
              className="h-8 font-mono text-[13px] rounded-[2px]"
            />
          </Field>
        </div>
      ) : (
        <Field label="File path">
          <Input
            value={draft.database}
            placeholder="/path/to/db.sqlite"
            onChange={(e) => up("database", e.target.value)}
            className="h-8 font-mono text-[13px] rounded-[2px]"
          />
        </Field>
      )}

      {!isEmbedded && (
        <>
          <Field label={isEmbedded ? "File path" : "Database"}>
            <Input
              value={draft.database}
              placeholder="acme_core"
              onChange={(e) => up("database", e.target.value)}
              className="h-8 font-mono text-[13px] rounded-[2px]"
            />
          </Field>
          <div className="grid grid-cols-2 gap-2.5">
            <Field label="User">
              <Input
                value={draft.user}
                placeholder="app_reader"
                onChange={(e) => up("user", e.target.value)}
                className="h-8 font-mono text-[13px] rounded-[2px]"
              />
            </Field>
            <Field label="Password">
              <Input
                type="password"
                placeholder="••••••••"
                className="h-8 font-mono text-[13px] rounded-[2px]"
              />
            </Field>
          </div>
        </>
      )}

      {error && (
        <div className={cn(
          "flex items-center gap-2 rounded-[2px] border px-2.5 py-1.5 font-mono text-[12px]",
          error.toLowerCase().includes("success")
            ? "border-[var(--jb-connection-ok)]/40 bg-[var(--jb-connection-ok)]/10 text-[var(--jb-connection-ok)]"
            : "border-[var(--jb-connection-err)]/40 bg-[var(--jb-connection-err)]/10 text-[var(--jb-connection-err)]",
        )}>
          <HugeiconsIcon
            icon={error.toLowerCase().includes("success") ? CloudCheckIcon : AlertCircleIcon}
            size={14}
          />
          {error}
        </div>
      )}
    </div>
  )
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="block space-y-1">
      <span className="font-mono text-[11px] uppercase tracking-wider text-muted-foreground block">{label}</span>
      {children}
    </label>
  )
}

function StatusDot({ status }: { status: DBConnection["status"] }) {
  const color =
    status === "connected" ? "var(--jb-connection-ok)" :
    status === "error"     ? "var(--jb-connection-err)" :
    status === "disconnected" ? "var(--muted-foreground)" : "var(--primary)"
  return (
    <span
      aria-hidden
      className="h-2 w-2 shrink-0 rounded-full ring-2 ring-[var(--background)]"
      style={{ background: color }}
    />
  )
}

function emptyDraft(type: DBType): DraftConnection {
  const def = DB_TYPES.find((d) => d.id === type)!
  return {
    id: crypto.randomUUID(),
    name: "",
    type,
    host: type === "sqlite" ? "" : "localhost",
    port: def.defaultPort,
    database: type === "sqlite" ? "" : "my_database",
    user: type === "sqlite" ? "" : "my_user",
  }
}
