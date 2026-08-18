import * as React from "react"

export type Theme = "light" | "dark"

const STORAGE_KEY = "databridge:theme"

function readStored(): Theme {
  if (typeof localStorage !== "undefined") {
    const v = localStorage.getItem(STORAGE_KEY)
    if (v === "light" || v === "dark") return v
  }
  // Default: dark — it's the signature JetBrains look and avoids the
  // "icon buttons invisible on white" pitfall for the first run.
  return "dark"
}

function apply(theme: Theme) {
  if (typeof document === "undefined") return
  const root = document.documentElement
  if (theme === "dark") root.classList.add("dark")
  else root.classList.remove("dark")
  root.style.colorScheme = theme
}

const ThemeCtx = React.createContext<{
  theme: Theme
  toggle: () => void
  setTheme: (t: Theme) => void
}>({ theme: "dark", toggle: () => {}, setTheme: () => {} })

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [theme, setThemeState] = React.useState<Theme>(() => readStored())

  React.useEffect(() => {
    apply(theme)
    try { localStorage.setItem(STORAGE_KEY, theme) } catch {}
  }, [theme])

  const value = React.useMemo(() => ({
    theme,
    setTheme: setThemeState,
    toggle: () => setThemeState((t) => (t === "dark" ? "light" : "dark")),
  }), [theme])

  return <ThemeCtx.Provider value={value}>{children}</ThemeCtx.Provider>
}

export function useTheme() {
  return React.useContext(ThemeCtx)
}
