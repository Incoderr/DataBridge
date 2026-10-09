//! Shared helpers for the sqlx drivers.

/// Decode one cell of a dynamically-typed sqlx row into a domain `Value`.
///
/// Grid rows arrive with runtime-known types, so concrete decodings are
/// tried from the narrowest type outward; the first compatible one wins.
/// Anything that fails every attempt (or is SQL NULL) becomes
/// [`Value::Null`]. Dates, UUIDs and JSON are stringified; raw byte
/// arrays become [`Value::Bytes`].
///
/// This is a macro (not a generic fn) on purpose: `Row::try_get` bounds
/// (`T: Decode<DB> + Type<DB>`) can only be proven per concrete database,
/// so the ladder must expand against each driver's row type.
macro_rules! dyn_cell {
    ($row:expr, $i:expr) => {{
        use sqlx::Row;
        if let Ok(v) = $row.try_get::<Option<i64>, _>($i) {
            v.map($crate::core::model::Value::Int)
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<i32>, _>($i) {
            v.map(|x| $crate::core::model::Value::Int(x as i64))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<i16>, _>($i) {
            v.map(|x| $crate::core::model::Value::Int(x as i64))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<i8>, _>($i) {
            v.map(|x| $crate::core::model::Value::Int(x as i64))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<f64>, _>($i) {
            v.map($crate::core::model::Value::Float)
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<f32>, _>($i) {
            v.map(|x| $crate::core::model::Value::Float(x as f64))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<bool>, _>($i) {
            v.map($crate::core::model::Value::Bool)
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<chrono::NaiveDateTime>, _>($i) {
            v.map(|t| $crate::core::model::Value::Str(t.to_string()))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>($i) {
            v.map(|t| $crate::core::model::Value::Str(t.to_string()))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<chrono::NaiveDate>, _>($i) {
            v.map(|t| $crate::core::model::Value::Str(t.to_string()))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<chrono::NaiveTime>, _>($i) {
            v.map(|t| $crate::core::model::Value::Str(t.to_string()))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<uuid::Uuid>, _>($i) {
            v.map(|u| $crate::core::model::Value::Str(u.to_string()))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<serde_json::Value>, _>($i) {
            v.map(|j| $crate::core::model::Value::Str(j.to_string()))
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<String>, _>($i) {
            v.map($crate::core::model::Value::Str)
                .unwrap_or($crate::core::model::Value::Null)
        } else if let Ok(v) = $row.try_get::<Option<Vec<u8>>, _>($i) {
            v.map($crate::core::model::Value::Bytes)
                .unwrap_or($crate::core::model::Value::Null)
        } else {
            $crate::core::model::Value::Null
        }
    }};
}
pub(crate) use dyn_cell;

/// Bind one typed cell onto any sqlx query. NULL binds as a typed
/// `Option<T>::None` so PostgreSQL can infer the parameter type; the
/// non-null variants bind by their own value type. A macro for the same
/// reason as [`dyn_cell`]: `Encode`/`Type` bounds are proven per
/// concrete database.
macro_rules! bind_cell {
    ($query:expr, $cell:expr) => {{
        use $crate::core::model::{ColumnKind, Value};
        match (&$cell.value, $cell.kind) {
            (Value::Int(v), _) => $query.bind(*v),
            (Value::Float(v), _) => $query.bind(*v),
            (Value::Bool(v), _) => $query.bind(*v),
            (Value::Str(s), _) => $query.bind(s.clone()),
            (Value::Bytes(b), _) => $query.bind(b.clone()),
            (Value::Null, ColumnKind::Int) => $query.bind(Option::<i64>::None),
            (Value::Null, ColumnKind::Float) => $query.bind(Option::<f64>::None),
            (Value::Null, ColumnKind::Bool) => $query.bind(Option::<bool>::None),
            (Value::Null, _) => $query.bind(Option::<String>::None),
        }
    }};
}
pub(crate) use bind_cell;
