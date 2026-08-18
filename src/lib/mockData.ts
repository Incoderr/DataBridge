/**
 * Mock data layer for DataBridge.
 *
 * Simulates database connections and records purely in the frontend so the
 * JetBrains-style UI can be exercised end-to-end. The Rust/Tauri backend
 * currently only has a `greet` stub; once real DB drivers are wired up, the
 * types in this file are the contract the Tauri commands should return.
 */

// ---------- Types (also the Tauri command contract) ----------

export type ConnectionStatus = "connected" | "disconnected" | "error" | "connecting";

export type DBType = "postgresql" | "mysql" | "sqlite" | "mongodb" | "redis";

export interface DBConnection {
  id: string;
  name: string;
  type: DBType;
  host: string;
  port: number;
  database: string;
  user: string;
  status: ConnectionStatus;
  /** how many ms the (mock) ping took */
  latencyMs: number;
}

export type NodeKind = "connection" | "schema" | "table" | "view" | "function" | "procedure";

export interface ColumnMeta {
  name: string;
  type: string;            // postgres-ish label e.g. "integer", "varchar(255)"
  nullable: boolean;
  primaryKey?: boolean;
  /** for numeric/text: rough width hint for color tags */
  kind: "int" | "float" | "text" | "json" | "bool" | "date" | "uuid" | "binary";
}

export interface TableMeta {
  name: string;
  schema: string;
  kind: NodeKind;          // table | view | function
  columns: ColumnMeta[];
  rowCount: number;        // total rows (mock)
}

/** A row is an ordered object keyed by column name. */
export type Row = Record<string, unknown>;

// ---------- Connections ----------

export const CONNECTIONS: DBConnection[] = [
  {
    id: "pg-prod",
    name: "production_postgres",
    type: "postgresql",
    host: "db.internal.acme.io",
    port: 5432,
    database: "acme_core",
    user: "app_reader",
    status: "connected",
    latencyMs: 18,
  },
  {
    id: "mysql-analytics",
    name: "analytics_mysql",
    type: "mysql",
    host: "analytics.acme.io",
    port: 3306,
    database: "warehouse",
    user: "bi_user",
    status: "connected",
    latencyMs: 41,
  },
  {
    id: "sqlite-local",
    name: "local_books.sqlite",
    type: "sqlite",
    host: "localhost",
    port: 0,
    database: "/home/laen7/data/books.sqlite",
    user: "",
    status: "connected",
    latencyMs: 2,
  },
  {
    id: "staging-pg",
    name: "staging_pg (offline)",
    type: "postgresql",
    host: "staging-db.acme.io",
    port: 5432,
    database: "acme_staging",
    user: "staging_reader",
    status: "error",
    latencyMs: 0,
  },
  {
    id: "unauth-mongo",
    name: "logs_mongo (auth)",
    type: "mongodb",
    host: "mongo.acme.io",
    port: 27017,
    database: "logs",
    user: "logger",
    status: "disconnected",
    latencyMs: 0,
  },
];

// ---------- Schemas & tables per connection ----------

export interface ConnectionTree {
  connectionId: string;
  schemas: {
    name: string;
    tables: TableMeta[];
    views?: TableMeta[];
    functions?: TableMeta[];
  }[];
}

export const TREE: ConnectionTree[] = [
  {
    connectionId: "pg-prod",
    schemas: [
      {
        name: "public",
        tables: [
          makeTable("public", "users", [
            col("id", "int8", "int", false, true),
            col("email", "varchar(255)", "text", false),
            col("name", "varchar(120)", "text", true),
            col("is_active", "boolean", "bool", false),
            col("created_at", "timestamptz", "date", false),
            col("balance", "numeric(12,2)", "float", true),
          ], 24_812),
          makeTable("public", "orders", [
            col("id", "int8", "int", false, true),
            col("user_id", "int8", "int", false),
            col("total", "numeric(10,2)", "float", false),
            col("status", "varchar(20)", "text", false),
            col("placed_at", "timestamptz", "date", false),
          ], 158_204),
          makeTable("public", "products", [
            col("id", "int8", "int", false, true),
            col("sku", "varchar(48)", "text", false),
            col("title", "varchar(200)", "text", false),
            col("price", "numeric(10,2)", "float", false),
            col("in_stock", "int4", "int", false),
            col("attributes", "jsonb", "json", true),
          ], 3_410),
          makeTable("public", "sessions", [
            col("id", "uuid", "uuid", false, true),
            col("user_id", "int8", "int", false),
            col("token", "varchar(64)", "text", false),
            col("expires_at", "timestamptz", "date", false),
          ], 9_221),
        ],
        views: [
          makeView("public", "user_summary", [
            col("user_id", "int8", "int", false),
            col("email", "varchar(255)", "text", false),
            col("order_count", "int8", "int", false),
            col("lifetime_value", "numeric(14,2)", "float", false),
          ], 8_142),
        ],
        functions: [
          makeFunction("public", "calculate_tax(amount numeric)", [
            col("amount", "numeric", "float", false),
          ]),
        ],
      },
      {
        name: "auth",
        tables: [
          makeTable("auth", "credentials", [
            col("user_id", "int8", "int", false, true),
            col("provider", "varchar(40)", "text", false),
            col("provider_uid", "varchar(255)", "text", false),
            col("password_hash", "varchar(128)", "text", true),
            col("last_login", "timestamptz", "date", true),
          ], 24_812),
          makeTable("auth", "audit_log", [
            col("id", "int8", "int", false, true),
            col("actor", "varchar(120)", "text", false),
            col("action", "varchar(60)", "text", false),
            col("metadata", "jsonb", "json", true),
            col("at", "timestamptz", "date", false),
          ], 312_045),
        ],
      },
    ],
  },
  {
    connectionId: "mysql-analytics",
    schemas: [
      {
        name: "warehouse",
        tables: [
          makeTable("warehouse", "events", [
            col("id", "bigint", "int", false, true),
            col("event_type", "varchar(40)", "text", false),
            col("user_id", "bigint", "int", true),
            col("payload", "json", "json", true),
            col("ts", "datetime", "date", false),
          ], 1_028_221),
          makeTable("warehouse", "funnel_steps", [
            col("session_id", "varchar(64)", "text", false, true),
            col("step", "int", "int", false),
            col("value", "decimal(10,4)", "float", true),
            col("ts", "datetime", "date", false),
          ], 532_810),
        ],
        views: [
          makeView("warehouse", "daily_events", [
            col("day", "date", "date", false),
            col("event_count", "bigint", "int", false),
            col("unique_users", "bigint", "int", false),
          ], 365),
        ],
      },
    ],
  },
  {
    connectionId: "sqlite-local",
    schemas: [
      {
        name: "main",
        tables: [
          makeTable("main", "books", [
            col("id", "INTEGER", "int", false, true),
            col("title", "TEXT", "text", false),
            col("author", "TEXT", "text", true),
            col("year", "INTEGER", "int", true),
            col("rating", "REAL", "float", true),
            col("tags", "TEXT", "text", true),
          ], 1_204),
          makeTable("main", "authors", [
            col("id", "INTEGER", "int", false, true),
            col("name", "TEXT", "text", false),
            col("country", "TEXT", "text", true),
            col("birth_year", "INTEGER", "int", true),
          ], 412),
        ],
      },
    ],
  },
  {
    connectionId: "staging-pg",
    schemas: [],     // unreachable -> empty
  },
  {
    connectionId: "unauth-mongo",
    schemas: [],     // auth needed
  },
];

// ---------- helpers to build schema ----------

function col(
  name: string,
  type: string,
  kind: ColumnMeta["kind"],
  nullable: boolean,
  primaryKey = false,
): ColumnMeta {
  return { name, type, kind, nullable, primaryKey };
}
function makeTable(schema: string, name: string, columns: ColumnMeta[], rowCount: number): TableMeta {
  return { name, schema, kind: "table", columns, rowCount };
}
function makeView(schema: string, name: string, columns: ColumnMeta[], rowCount: number): TableMeta {
  return { name, schema, kind: "view", columns, rowCount };
}
function makeFunction(schema: string, name: string, columns: ColumnMeta[]): TableMeta {
  return { name, schema, kind: "function", columns, rowCount: 0 };
}

// ---------- Mock record generator ----------
//
// Each call returns deterministic sample rows for the given connection/table.
// Deterministic so the UI is stable across clicks and the rand never rekeys.

const md5 = (s: string): number => {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return h;
};
const pick = (arr: readonly string[], seed: number) => arr[Math.abs(seed) % arr.length];
const pad = (n: number, w: number) => String(n).padStart(w, "0");

function genValue(col: ColumnMeta, rowSeed: number): unknown {
  const s = md5(col.name + ":" + rowSeed);
  const abs = Math.abs(s);
  switch (col.kind) {
    case "int":
      return abs % 100000;
    case "float":
      return Number(((abs % 100000) / 100).toFixed(2));
    case "bool":
      return Boolean(abs % 2);
    case "date": {
      const days = abs % 1800;
      const d = new Date(Date.UTC(2020, 0, 1));
      d.setUTCDate(d.getUTCDate() + days);
      const hh = pad((abs >> 4) % 24, 2),
        mm = pad((abs >> 8) % 60, 2),
        ss = pad((abs >> 12) % 60, 2);
      return `${d.toISOString().slice(0, 10)} ${hh}:${mm}:${ss}`;
    }
    case "uuid":
      return `${pad((abs >> 0) & 0xff, 2)}${pad((abs >> 8) & 0xff, 2)}${pad(
        (abs >> 16) & 0xff, 2,
      )}-${pad((abs >> 4) & 0xf, 1)}-${pad((abs >> 7) & 0xff, 2)}-${pad(
        (abs >> 12) & 0xff, 2,
      )}-${pad((abs >> 2) & 0xffff, 4)}${pad((abs >> 18) & 0xffff, 4)}`;
    case "json":
      return JSON.stringify({ k1: pick(["alpha", "beta", "gamma"], s), n: abs % 100, ok: Boolean(abs % 2) });
    case "binary":
      return `0x${pad(abs & 0xff, 2)}${pad((abs >> 8) & 0xff, 2)}`;
    case "text": {
      const cols: Record<string, readonly string[]> = {
        email: ["runner@acme.io", "maria.d@example.com", "freddie@bies.eu", "lu@pp.dev", "alex@host.ru", "kai@null.cloud"],
        name: ["Ada Lovelace", "Alan Turing", "Edsger Dijkstra", "Grace Hopper", "Knuth", "Guido", "Linus"],
        status: ["open", "paid", "shipped", "refunded", "cancelled", "pending"],
        title: ["The Pragmatic Programmer", "Refactoring", "Clean Code", "DDD", "The Phoenix Project", "PostgreSQL Up & Running"],
        sku: ["SKU-" + pad(abs % 9999, 4), "PRD-" + pad((abs >> 8) % 9999, 4)],
        author: ["Sandi Metz", "Robert Martin", "Kathy Sierra", "Brian Goetz"],
        country: ["RU", "US", "DE", "JP", "BR", "FR"],
        action: ["login", "logout", "update", "create", "delete", "refresh"],
        actor: ["app_reader", "staging_reader", "cron_worker", "frontend"],
        provider: ["google", "github", "email", "apple"],
        provider_uid: ["gid:" + abs, "ghid:" + abs, "email:" + abs, "aid:" + abs],
        token: ["tok-" + pad(abs, 6) + pad((abs >> 8) & 0xffffff, 6)],
        event_type: ["click", "view", "purchase", "signup", "error", "page_view"],
        password_hash: ["$2b$12$" + pad(abs, 6) + pad((abs >> 8) & 0xffff, 4)],
      };
      return cols[col.name] ? pick(cols[col.name], s) : `${col.name}-${abs}`;
    }
    default:
      return null;
  }
}

/**
 * Returns up to `limit` mock rows for the given table in the given connection.
 * Deterministic — same (connection, schema, table, offset, limit) => same rows.
 */
export async function fetchRows(
  connectionId: string,
  schema: string,
  table: string,
  options: { offset?: number; limit?: number } = {},
): Promise<{ rows: Row[]; total: number; truncated: boolean; elapsedMs: number }> {
  const { offset = 0, limit = 100 } = options;
  const t = findTable(connectionId, schema, table);
  if (!t) {
    return { rows: [], total: 0, truncated: false, elapsedMs: 0 };
  }
  // simulate async query latency per connection
  const conn = CONNECTIONS.find((c) => c.id === connectionId)!;
  const latency = Math.max(2, Math.min(conn.latencyMs, 120));
  await new Promise((r) => setTimeout(r, latency));

  const rows: Row[] = [];
  for (let i = 0; i < limit && offset + i < t.rowCount; i++) {
    const rowId = offset + i;
    const row: Row = {};
    for (const c of t.columns) row[c.name] = genValue(c, rowId);
    rows.push(row);
  }
  return {
    rows,
    total: t.rowCount,
    truncated: t.rowCount > limit,
    elapsedMs: latency,
  };
}

/** Returns the metadata "status" string JetBrains editors show: SELECT 100 in N ms. */
export function statusLine(elapsedMs: number, rowCount: number, total: number): string {
  return `${rowCount} row${rowCount === 1 ? "" : "s"} fetched in ${elapsedMs} ms${total > rowCount ? ` (truncated; ${total} total)` : ""}`;
}

// ---------- Tree lookup helpers ----------

export function findTable(connectionId: string, schema: string, table: string): TableMeta | undefined {
  const tree = TREE.find((t) => t.connectionId === connectionId);
  if (!tree) return undefined;
  for (const s of tree.schemas) {
    if (s.name !== schema) continue;
    const all = [...(s.tables ?? []), ...(s.views ?? []), ...(s.functions ?? [])];
    return all.find((t) => t.name === table);
  }
  return undefined;
}

export function findConnection(id: string): DBConnection | undefined {
  return CONNECTIONS.find((c) => c.id === id);
}
