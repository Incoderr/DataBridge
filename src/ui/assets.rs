//! Asset source: DataBridge brand + extra icons first, then the bundled
//! gpui-component Lucide icon set as a fallback.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// Brand glyphs for supported databases (simple-icons paths).
static BRAND_ICONS: &[(&str, &[u8])] = &[
    (
        "brand/postgresql.svg",
        include_bytes!("../../assets/icons/postgresql.svg"),
    ),
    ("brand/mysql.svg", include_bytes!("../../assets/icons/mysql.svg")),
    ("brand/sqlite.svg", include_bytes!("../../assets/icons/sqlite.svg")),
    (
        "brand/mongodb.svg",
        include_bytes!("../../assets/icons/mongodb.svg"),
    ),
    ("brand/redis.svg", include_bytes!("../../assets/icons/redis.svg")),
    (
        "brand/mariadb.svg",
        include_bytes!("../../assets/icons/mariadb.svg"),
    ),
    (
        "brand/clickhouse.svg",
        include_bytes!("../../assets/icons/clickhouse.svg"),
    ),
    (
        "brand/apachecassandra.svg",
        include_bytes!("../../assets/icons/apachecassandra.svg"),
    ),
    (
        "brand/scylladb.svg",
        include_bytes!("../../assets/icons/scylladb.svg"),
    ),
];

/// Extra Lucide glyphs missing from the bundled gpui-component icon set.
static EXTRA_ICONS: &[(&str, &[u8])] = &[
    ("icons/cube.svg", include_bytes!("../../assets/icons/cube.svg")),
    ("icons/table.svg", include_bytes!("../../assets/icons/table.svg")),
    ("icons/braces.svg", include_bytes!("../../assets/icons/braces.svg")),
    ("icons/schema.svg", include_bytes!("../../assets/icons/schema.svg")),
    ("icons/sigma.svg", include_bytes!("../../assets/icons/sigma.svg")),
    ("icons/filter.svg", include_bytes!("../../assets/icons/filter.svg")),
    (
        "icons/download.svg",
        include_bytes!("../../assets/icons/download.svg"),
    ),
    ("icons/play.svg", include_bytes!("../../assets/icons/play.svg")),
    ("icons/stop.svg", include_bytes!("../../assets/icons/stop.svg")),
    // Settings → Keyboard tab.
    ("icons/keyboard.svg", include_bytes!("../../assets/icons/keyboard.svg")),
    // Records toolbar: edit / delete row (not in the kit's embedded set).
    ("icons/pencil.svg", include_bytes!("../../assets/icons/pencil.svg")),
    ("icons/trash.svg", include_bytes!("../../assets/icons/trash.svg")),
    // Results pane chart toggle / kinds (chart-pie is in the kit's set).
    ("icons/chart-column.svg", include_bytes!("../../assets/icons/chart-column.svg")),
    ("icons/chart-line.svg", include_bytes!("../../assets/icons/chart-line.svg")),
    ("icons/chart-area.svg", include_bytes!("../../assets/icons/chart-area.svg")),
];

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = BRAND_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some((_, bytes)) = EXTRA_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_kit_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(BRAND_ICONS
            .iter()
            .chain(EXTRA_ICONS.iter())
            .map(|(p, _)| SharedString::from(*p))
            .chain(gpui_kit_assets::Assets.list(path)?)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `"icons/….svg"` the UI names must load — a glyph missing from
    /// both our assets and the kit's embedded set renders as nothing.
    #[test]
    fn every_icon_path_in_the_ui_loads() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui"), &mut files);
        let mut missing = Vec::new();
        for f in files {
            let src = std::fs::read_to_string(&f).unwrap();
            // Product code only (this test names a pattern itself).
            let src = src.split("#[cfg(test)]").next().unwrap_or("");
            for (ix, _) in src.match_indices("\"icons/") {
                let rest = &src[ix + 1..];
                let path = &rest[..rest.find('"').unwrap()];
                if path.ends_with(".svg") && !matches!(AppAssets.load(path), Ok(Some(_))) {
                    missing.push(format!("{path} ({})", f.display()));
                }
            }
        }
        assert!(missing.is_empty(), "icons that do not load:\n{}", missing.join("\n"));
    }
}
