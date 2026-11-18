//! Keyboard shortcuts: two built-in presets (DataGrip / DBeaver and
//! VS Code, picked in Settings → Keyboard) plus the user's own overrides
//! in `keymap.json` next to `settings.json`.
//!
//! Every customizable command is a row of [`COMMANDS`]: a stable id
//! (`sql.run`, …), the key context it lives in, its keys per preset, and
//! the action it dispatches. `keymap.json` maps ids to keys and replaces
//! the preset's keys for those ids (`[]` unbinds):
//!
//! ```json
//! { "sql.run": ["ctrl-enter", "f5"], "sql.duplicate_line": "ctrl-shift-d" }
//! ```
//!
//! Keys use gpui's syntax (`ctrl-shift-enter`, `alt-up`, chords separated
//! by spaces); `secondary-` is Ctrl, or Cmd on macOS. Keys starting with
//! `_` are ignored (room for notes); unknown ids and bad keys are reported
//! in the Keyboard tab while the rest still applies.
//!
//! gpui's keymap only grows, so [`apply`] rebuilds it: the bindings
//! gpui-component registered at startup (captured once in [`init`], minus
//! the ones for actions this table owns, such as Undo / Redo), then ours.
//! Ours come last and so win over the kit's at the same context depth.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{Action, App, Global, KeyBinding, KeyBindingContextPredicate, actions};
use gpui_component::input::{Redo, ShowCompletions, Undo};

use crate::core::storage;

actions!(
    databridge,
    [
        RunQuery,
        ToggleComment,
        DuplicateLine,
        DeleteLine,
        MoveLineUp,
        MoveLineDown,
        NewLineBelow,
        NewLineAbove,
        NewQuery,
        CloseTab,
        RefreshData,
        ToggleAiChat,
        OpenSettings,
    ]
);

/// Key context of the SQL editor pane (`ui/query.rs`).
pub const SQL_EDITOR: &str = "SqlEditor";
/// Key context of the app's root element (`ui/app.rs`).
pub const APP: &str = "DataBridge";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// DataGrip / DBeaver: Ctrl+Enter runs, Ctrl+D duplicates, Ctrl+Y deletes a line.
    JetBrains,
    /// VS Code: Ctrl+Enter starts a line below, F5 runs, Alt+↑/↓ move lines.
    VsCode,
}

impl Preset {
    pub const ALL: [Preset; 2] = [Preset::JetBrains, Preset::VsCode];

    pub fn key(self) -> &'static str {
        match self {
            Preset::JetBrains => "jetbrains",
            Preset::VsCode => "vscode",
        }
    }

    pub fn from_key(k: &str) -> Preset {
        match k {
            "vscode" => Preset::VsCode,
            _ => Preset::JetBrains,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Preset::JetBrains => "DataGrip / DBeaver",
            Preset::VsCode => "VS Code",
        }
    }
}

/// Where a command's keys work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The SQL editor.
    SqlEditor,
    /// Every text field (undo / redo).
    TextField,
    /// Anywhere in the window (a focused editor's own keys win).
    App,
}

impl Scope {
    fn context(self) -> String {
        match self {
            Scope::SqlEditor => format!("{SQL_EDITOR} > Input"),
            Scope::TextField => "Input".into(),
            Scope::App => APP.into(),
        }
    }

    /// Settings → Keyboard section title key.
    pub fn title_key(self) -> &'static str {
        match self {
            Scope::SqlEditor => "keys.scope_sql",
            Scope::TextField => "keys.scope_text",
            Scope::App => "keys.scope_app",
        }
    }
}

pub struct Command {
    /// Stable id, used in `keymap.json`.
    pub id: &'static str,
    pub scope: Scope,
    jetbrains: &'static [&'static str],
    vscode: &'static [&'static str],
    action: fn() -> Box<dyn Action>,
}

impl Command {
    pub fn preset_keys(&self, p: Preset) -> &'static [&'static str] {
        match p {
            Preset::JetBrains => self.jetbrains,
            Preset::VsCode => self.vscode,
        }
    }

    /// Translation key of the command's name.
    pub fn label_key(&self) -> String {
        format!("keys.{}", self.id.replace('.', "_"))
    }
}

macro_rules! cmd {
    ($id:literal, $scope:ident, $jb:expr, $vs:expr, $action:expr) => {
        Command { id: $id, scope: Scope::$scope, jetbrains: &$jb, vscode: &$vs, action: || Box::new($action) }
    };
}

pub static COMMANDS: &[Command] = &[
    cmd!("sql.run", SqlEditor, ["secondary-enter", "f5"], ["f5", "secondary-shift-e"], RunQuery),
    cmd!("sql.complete", SqlEditor, ["ctrl-space"], ["ctrl-space"], ShowCompletions),
    cmd!("sql.toggle_comment", SqlEditor, ["secondary-/"], ["secondary-/"], ToggleComment),
    cmd!("sql.duplicate_line", SqlEditor, ["secondary-d"], ["shift-alt-down"], DuplicateLine),
    cmd!("sql.delete_line", SqlEditor, ["secondary-y"], ["secondary-shift-k"], DeleteLine),
    cmd!("sql.move_line_up", SqlEditor, ["alt-shift-up"], ["alt-up"], MoveLineUp),
    cmd!("sql.move_line_down", SqlEditor, ["alt-shift-down"], ["alt-down"], MoveLineDown),
    cmd!("sql.line_below", SqlEditor, ["shift-enter"], ["secondary-enter"], NewLineBelow),
    cmd!("sql.line_above", SqlEditor, ["secondary-alt-enter"], ["secondary-shift-enter"], NewLineAbove),
    cmd!("edit.undo", TextField, ["secondary-z"], ["secondary-z"], Undo),
    cmd!("edit.redo", TextField, ["secondary-shift-z"], ["secondary-y", "secondary-shift-z"], Redo),
    cmd!("app.new_query", App, ["secondary-t"], ["secondary-n"], NewQuery),
    cmd!("app.close_tab", App, ["secondary-f4"], ["secondary-w"], CloseTab),
    cmd!("app.refresh", App, ["f5"], ["f5"], RefreshData),
    cmd!("app.toggle_ai", App, ["secondary-alt-i"], ["secondary-alt-i"], ToggleAiChat),
    cmd!("app.settings", App, ["secondary-,"], ["secondary-,"], OpenSettings),
];

/// Bindings that are not customizable (registered after the table).
pub type FixedBindings = fn(&App) -> Vec<KeyBinding>;

/// gpui-component's own bindings, captured right after its `init`.
struct BaseKeymap(Vec<KeyBinding>);
impl Global for BaseKeymap {}

/// The active preset, the user's overrides and what went wrong reading them.
#[derive(Default)]
pub struct Keymap {
    pub preset: Option<Preset>,
    pub user: BTreeMap<String, Vec<String>>,
    /// Problems reading `keymap.json`.
    file_errors: Vec<String>,
    /// Keys gpui refused to parse.
    bind_errors: Vec<String>,
    fixed: Vec<FixedBindings>,
}

impl Keymap {
    pub fn errors(&self) -> impl Iterator<Item = &String> {
        self.file_errors.iter().chain(&self.bind_errors)
    }
}
impl Global for Keymap {}

pub const FILE: &str = "keymap.json";

pub fn file_path() -> Option<PathBuf> {
    storage::config_dir().map(|d| d.join(FILE))
}

/// Capture the kit's bindings and install ours. Call once, right after
/// `gpui_component::init`; `fixed` adds bindings that are not in the table.
pub fn init(preset: Preset, fixed: Vec<FixedBindings>, cx: &mut App) {
    let base: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
    cx.set_global(BaseKeymap(base));
    let mut km = Keymap { preset: Some(preset), fixed, ..Default::default() };
    load_user(&mut km);
    cx.set_global(km);
    apply(cx);
}

pub fn preset(cx: &App) -> Preset {
    cx.try_global::<Keymap>().and_then(|k| k.preset).unwrap_or(Preset::JetBrains)
}

pub fn set_preset(p: Preset, cx: &mut App) {
    cx.global_mut::<Keymap>().preset = Some(p);
    apply(cx);
    crate::ui::settings::persist(cx);
}

/// Re-read `keymap.json` and rebuild the bindings.
pub fn reload(cx: &mut App) {
    let mut km = cx.remove_global::<Keymap>();
    load_user(&mut km);
    cx.set_global(km);
    apply(cx);
}

/// The keys a command has right now: the user's override, else the preset's.
pub fn keys_for(cmd: &Command, cx: &App) -> Vec<String> {
    let km = cx.try_global::<Keymap>();
    if let Some(user) = km.and_then(|k| k.user.get(cmd.id)) {
        return user.clone();
    }
    cmd.preset_keys(preset(cx)).iter().map(|s| s.to_string()).collect()
}

pub fn is_overridden(cmd: &Command, cx: &App) -> bool {
    cx.try_global::<Keymap>().is_some_and(|k| k.user.contains_key(cmd.id))
}

pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

/// "Ctrl+Enter" for a command's first key, for tooltips; `None` if unbound.
pub fn shortcut(id: &str, cx: &App) -> Option<String> {
    let cmd = command(id)?;
    keys_for(cmd, cx).first().map(|k| display(k, cx.platform_info().is_mac()))
}

/// `label` plus the command's shortcut in parentheses, when it has one.
pub fn with_shortcut(label: impl Into<String>, id: &str, cx: &App) -> String {
    let label = label.into();
    match shortcut(id, cx) {
        Some(k) => format!("{label} ({k})"),
        None => label,
    }
}

/// Human-readable keys: `secondary-shift-enter` → `Ctrl+Shift+Enter`.
pub fn display(keys: &str, mac: bool) -> String {
    keys.split_whitespace()
        .map(|stroke| {
            // The key itself may be `-` (`ctrl--`).
            let (mods, key) = match stroke.strip_suffix("--") {
                Some(m) => (m, "-"),
                None => match stroke.rsplit_once('-') {
                    Some((m, k)) if !k.is_empty() => (m, k),
                    _ => ("", stroke),
                },
            };
            let mut parts: Vec<String> = mods
                .split('-')
                .filter(|m| !m.is_empty())
                .map(|m| {
                    match m {
                        "secondary" if mac => "Cmd",
                        "secondary" | "ctrl" | "control" => "Ctrl",
                        "cmd" | "super" | "win" => if mac { "Cmd" } else { "Win" },
                        "alt" | "option" => if mac { "Option" } else { "Alt" },
                        "shift" => "Shift",
                        "fn" => "Fn",
                        other => other,
                    }
                    .to_string()
                })
                .collect();
            let key = match key {
                "enter" => "Enter".to_string(),
                "space" => "Space".into(),
                "escape" => "Esc".into(),
                "backspace" => "Backspace".into(),
                "delete" => "Delete".into(),
                "tab" => "Tab".into(),
                "up" => "↑".into(),
                "down" => "↓".into(),
                "left" => "←".into(),
                "right" => "→".into(),
                "pageup" => "PageUp".into(),
                "pagedown" => "PageDown".into(),
                "home" => "Home".into(),
                "end" => "End".into(),
                k => k.to_uppercase(),
            };
            parts.push(key);
            parts.join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse `keymap.json` into `km.user` / `km.errors`. A missing file is an
/// empty override set.
fn load_user(km: &mut Keymap) {
    km.user.clear();
    km.file_errors.clear();
    // Tests never read the developer's own keymap.
    if cfg!(test) {
        return;
    }
    let Some(path) = file_path() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let (user, errors) = parse_user(&text);
    km.user = user;
    km.file_errors = errors;
}

/// The override map of a `keymap.json` text, plus readable errors.
/// Keystrokes are checked here only for shape; gpui validates them when
/// the bindings are built (`apply` reports what it rejects).
pub fn parse_user(text: &str) -> (BTreeMap<String, Vec<String>>, Vec<String>) {
    let mut user = BTreeMap::new();
    let mut errors = Vec::new();
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return (user, vec![format!("{FILE}: {e}")]),
    };
    let Some(map) = value.as_object() else {
        return (user, vec![format!("{FILE}: expected an object of \"command\": [\"keys\"]")]);
    };
    for (id, v) in map {
        if id.starts_with('_') {
            continue;
        }
        if command(id).is_none() {
            errors.push(format!("{id}: unknown command"));
            continue;
        }
        let keys: Option<Vec<String>> = match v {
            serde_json::Value::String(s) => Some(vec![s.clone()]),
            serde_json::Value::Null => Some(Vec::new()),
            serde_json::Value::Array(a) => a.iter().map(|k| k.as_str().map(str::to_string)).collect(),
            _ => None,
        };
        match keys {
            Some(keys) => {
                user.insert(id.clone(), keys.into_iter().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect());
            }
            None => errors.push(format!("{id}: expected a string or a list of strings")),
        }
    }
    (user, errors)
}

/// Rebuild the whole keymap: the kit's bindings, then the table, then the
/// fixed extras.
pub fn apply(cx: &mut App) {
    let Some(base) = cx.try_global::<BaseKeymap>().map(|b| b.0.clone()) else { return };
    // Actions the table owns lose their kit bindings (Undo / Redo), so the
    // preset decides their keys alone.
    let owned: Vec<&'static str> = COMMANDS.iter().map(|c| (c.action)().name()).collect();
    let base: Vec<KeyBinding> = base.into_iter().filter(|b| !owned.contains(&b.action().name())).collect();

    let mut ours = Vec::new();
    let mut errors = Vec::new();
    for cmd in COMMANDS {
        let ctx = cmd.scope.context();
        let predicate: Option<Rc<KeyBindingContextPredicate>> =
            KeyBindingContextPredicate::parse(&ctx).ok().map(Rc::new);
        for keys in keys_for(cmd, cx) {
            // Key equivalents: punctuation means the key at that spot of the
            // US layout, so Ctrl+/ stays one keypress on a Russian layout
            // (where that key types `.`), as in JetBrains IDEs and VS Code.
            match KeyBinding::load(&keys, (cmd.action)(), predicate.clone(), true, None, cx) {
                Ok(b) => ours.push(b),
                Err(e) => errors.push(format!("{}: \"{keys}\": {e}", cmd.id)),
            }
        }
    }
    let fixed: Vec<FixedBindings> = cx.try_global::<Keymap>().map(|k| k.fixed.clone()).unwrap_or_default();
    for f in fixed {
        ours.extend(f(cx));
    }
    if cx.has_global::<Keymap>() {
        cx.global_mut::<Keymap>().bind_errors = errors;
    }
    cx.clear_key_bindings();
    cx.bind_keys(base);
    cx.bind_keys(ours);
}

/// Create `keymap.json` with a short note if it is missing, then open it
/// in the system's editor for JSON files.
pub fn open_file(cx: &mut App) {
    let Some(path) = file_path() else { return };
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, TEMPLATE);
    }
    cx.open_with_system(&path);
}

const TEMPLATE: &str = r#"{
  "_help": "Overrides of the active preset: \"command id\": [\"keys\", ...]; [] unbinds. Ids and current keys: Settings > Keyboard. Keys: ctrl-shift-enter, alt-up, f5; secondary = Ctrl (Cmd on macOS). Save, then press Reload in Settings.",
  "_example": { "sql.run": ["secondary-enter", "f5"] }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_display_like_menus() {
        assert_eq!(display("secondary-shift-enter", false), "Ctrl+Shift+Enter");
        assert_eq!(display("secondary-enter", true), "Cmd+Enter");
        assert_eq!(display("alt-up", false), "Alt+↑");
        assert_eq!(display("f5", false), "F5");
        assert_eq!(display("secondary-/", false), "Ctrl+/");
        assert_eq!(display("ctrl--", false), "Ctrl+-");
        assert_eq!(display("ctrl-k ctrl-c", false), "Ctrl+K Ctrl+C");
    }

    #[test]
    fn user_overrides_parse_with_readable_errors() {
        let (user, errors) = parse_user(
            r#"{ "_help": "x", "sql.run": "f9", "sql.delete_line": [], "app.close_tab": null,
                 "sql.nope": "f1", "sql.duplicate_line": 3 }"#,
        );
        assert_eq!(user["sql.run"], vec!["f9"]);
        assert!(user["sql.delete_line"].is_empty());
        assert!(user["app.close_tab"].is_empty());
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(parse_user("not json").1[0].starts_with(FILE));
    }

    #[test]
    fn command_table_is_consistent() {
        let mut ids: Vec<&str> = COMMANDS.iter().map(|c| c.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), COMMANDS.len(), "duplicate command ids");
        for c in COMMANDS {
            for p in Preset::ALL {
                for k in c.preset_keys(p) {
                    for stroke in k.split_whitespace() {
                        let os = gpui::OperatingSystem::Windows;
                        assert!(gpui::Keystroke::parse_with_os(stroke, os).is_ok(), "{}: {k}", c.id);
                    }
                }
            }
            for loc in ["en", "ru"] {
                let key = c.label_key();
                assert_ne!(rust_i18n::t!(&key, locale = loc), key, "{loc}: {key} untranslated");
            }
        }
    }
}
