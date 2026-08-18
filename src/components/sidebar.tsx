import * as React from "react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipTrigger,
  TooltipContent,
  TooltipProvider,
} from "@/components/ui/tooltip";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  RefreshIcon,
  Settings01Icon,
  PlusSignIcon,
  GridIcon,
  ListViewIcon,
  Cancel01Icon,
} from "@hugeicons/core-free-icons";
import { BrandGlyph } from "@/components/brandIcon";
import type { DBConnection } from "@/lib/mockData";

interface Props {
  connection: DBConnection;
  onDisconnect: () => void;
  onNewConnection: () => void;
  onRefresh: () => void;
  /** visual state — JetBrains DB tool has light/dark dropdown */
  activeView?: "grid" | "list";
}

/** A small square toolbar button with tooltip - JetBrains tool-window style. */
function ToolBtn(
  props: React.ComponentProps<typeof Button> & {
    tlabel: string;
    children: React.ReactNode;
    active?: boolean;
    danger?: boolean;
  },
) {
  const { tlabel, children, active, danger, ...rest } = props;
  const icon = React.Children.toArray(children)[0];
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            size="icon-xs"
            variant="ghost"
            aria-label={tlabel}
            data-slot="sidebtn"
            className="jb-sidebtn"
            data-active={active ? "true" : undefined}
            data-danger={danger ? "true" : undefined}
            {...rest}
          >
            {icon}
          </Button>
        }
      />
      <TooltipContent side="right" className="font-mono">
        {tlabel}
      </TooltipContent>
    </Tooltip>
  );
}

function statusColor(status: DBConnection["status"]) {
  return status === "connected"
    ? "var(--jb-connection-ok)"
    : status === "error"
      ? "var(--jb-connection-err)"
      : status === "disconnected"
        ? "var(--muted-foreground)"
        : "var(--primary)";
}

export default function Sidebar({
  connection,
  onDisconnect,
  onNewConnection,
  onRefresh,
  activeView = "grid",
}: Props) {
  return (
    <TooltipProvider>
      <div className="flex h-full w-11 flex-col items-center gap-1 bg-[var(--jb-toolbar)] py-2 border-r border-[var(--jb-toolbar-border)] rounded-l-2xl">
        {/* Status dot */}
        <div
          aria-hidden
          className="mt-0.5 mb-1 h-2 w-2 rounded-full"
          style={{ background: statusColor(connection.status) }}
        />
        <ToolBtn tlabel={`Active: ${connection.name}`}>
          <BrandGlyph type={connection.type} size={16} />
          <span className="block max-w-40 truncate">{connection.name}</span>
        </ToolBtn>

        <div className="my-1 h-px w-6 bg-[var(--jb-toolbar-border)]" />

        <ToolBtn tlabel="New connection" onClick={onNewConnection}>
          <HugeiconsIcon icon={PlusSignIcon} size={16} strokeWidth={1.7} />
          New connection
        </ToolBtn>
        <ToolBtn tlabel="Refresh" onClick={onRefresh}>
          <HugeiconsIcon icon={RefreshIcon} size={16} strokeWidth={1.7} />
          Refresh
        </ToolBtn>

        <div className="flex-1" />

        <ToolBtn tlabel="Grid view" active={activeView === "grid"}>
          <HugeiconsIcon icon={GridIcon} size={15} strokeWidth={1.7} />
          Grid view
        </ToolBtn>
        <ToolBtn tlabel="List view" active={activeView === "list"}>
          <HugeiconsIcon icon={ListViewIcon} size={15} strokeWidth={1.7} />
          List view
        </ToolBtn>

        <div className="my-1 h-px w-6 bg-[var(--jb-toolbar-border)]" />

        <ToolBtn tlabel="Settings">
          <HugeiconsIcon icon={Settings01Icon} size={16} strokeWidth={1.7} />
          Settings
        </ToolBtn>
        <ToolBtn tlabel="Disconnect" onClick={onDisconnect} danger>
          <HugeiconsIcon icon={Cancel01Icon} size={16} strokeWidth={1.7} />
          Disconnect
        </ToolBtn>
      </div>
    </TooltipProvider>
  );
}
