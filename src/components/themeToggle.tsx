import { HugeiconsIcon } from "@hugeicons/react"
import { Sun01Icon, Moon01Icon } from "@hugeicons/core-free-icons"
import { Tooltip, TooltipTrigger, TooltipContent } from "@/components/ui/tooltip"
import { Button } from "@/components/ui/button"
import { useTheme } from "@/lib/theme"

/** Light/dark toggle button (JetBrains-style). Lives in toolbars. */
export function ThemeToggle({ size = 14 }: { size?: number }) {
  const { theme, toggle } = useTheme()
  const isDark = theme === "dark"
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            size="icon-xs"
            variant="ghost"
            aria-label={isDark ? "Switch to light theme" : "Switch to dark theme"}
            className="jb-sidebtn"
            onClick={toggle}
          >
            <HugeiconsIcon
              icon={isDark ? Sun01Icon : Moon01Icon}
              size={size}
              strokeWidth={1.7}
            />
          </Button>
        }
      />
      <TooltipContent side="bottom" className="font-mono">
        {isDark ? "Light theme" : "Dark theme"}
      </TooltipContent>
    </Tooltip>
  )
}
