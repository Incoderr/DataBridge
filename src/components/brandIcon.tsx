import {
  siPostgresql, siMysql, siMongodb, siRedis, siSqlite, siMariadb,
  type SimpleIcon,
} from "simple-icons"
import { HugeiconsIcon } from "@hugeicons/react"
import { Database01Icon } from "@hugeicons/core-free-icons"
import type { DBType } from "@/lib/mockData"

export const DB_BRAND: Record<string, SimpleIcon> = {
  postgresql: siPostgresql,
  mysql: siMysql,
  sqlite: siSqlite,
  mongodb: siMongodb,
  redis: siRedis,
  mariadb: siMariadb,
}

/** Render a simple-icons brand glyph directly to inline SVG with currentColor. */
export function BrandIcon({
  icon,
  size = 16,
  className,
  title,
}: {
  icon: SimpleIcon
  size?: number
  className?: string
  title?: string
}) {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="currentColor"
      role="img"
      aria-label={title ?? icon.title}
      className={className}
    >
      {title && <title>{title}</title>}
      <path d={icon.path} />
    </svg>
  )
}

/** Convenience: pick a brand glyph by DB type and render it tinted with its own brand color. */
export function BrandGlyph({ type, size = 16 }: { type: DBType; size?: number }) {
  const sii = DB_BRAND[type]
  if (sii) {
    return (
      <span style={{ color: `#${sii.hex}` }} className="inline-flex leading-none">
        <BrandIcon icon={sii} size={size} />
      </span>
    )
  }
  return <HugeiconsIcon icon={Database01Icon} size={size} strokeWidth={1.4} className="text-[var(--primary)]" />
}
