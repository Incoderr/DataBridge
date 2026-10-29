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
    ("icons/schema.svg", include_bytes!("../../assets/icons/schema.svg")),
    ("icons/sigma.svg", include_bytes!("../../assets/icons/sigma.svg")),
    ("icons/filter.svg", include_bytes!("../../assets/icons/filter.svg")),
    (
        "icons/download.svg",
        include_bytes!("../../assets/icons/download.svg"),
    ),
    ("icons/play.svg", include_bytes!("../../assets/icons/play.svg")),
    ("icons/stop.svg", include_bytes!("../../assets/icons/stop.svg")),
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
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(BRAND_ICONS
            .iter()
            .chain(EXTRA_ICONS.iter())
            .map(|(p, _)| SharedString::from(*p))
            .chain(gpui_component_assets::Assets.list(path)?)
            .collect())
    }
}
