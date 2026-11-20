//! Turning a column of a result row into JSON: the parts every engine shares. Each engine
//! module has its own `value` function, which knows that engine's types.

use serde_json::Value;
use sqlx::Row;

/// Returns from the enclosing fn with the first type that decodes the column.
macro_rules! decode_first {
    ($row:expr, $i:expr; $($ty:ty => $conv:expr),* $(,)?) => {
        $(
            if let Ok(v) = $row.try_get::<$ty, _>($i) {
                return $conv(v);
            }
        )*
    };
}
pub(super) use decode_first;

/// The name of the column's type, or the whole answer when it's NULL or unreadable.
macro_rules! type_name_or_return {
    ($row:expr, $i:expr) => {
        match $row.try_get_raw($i) {
            Ok(raw) if raw.is_null() => return serde_json::Value::Null,
            Ok(raw) => raw.type_info().name().to_owned(),
            Err(e) => return serde_json::Value::String(format!("<{e}>")),
        }
    };
}
pub(super) use type_name_or_return;

pub(super) fn text<T: ToString>(v: T) -> Value {
    Value::String(v.to_string())
}

pub(super) fn bytes(v: Vec<u8>) -> Value {
    let hex: String = v.iter().take(64).map(|b| format!("{b:02x}")).collect();
    let more = if v.len() > 64 { "…" } else { "" };
    Value::String(format!("0x{hex}{more}"))
}

/// Last resort for types without a mapping: the raw text if it is printable.
pub(super) fn fallback<R: Row>(row: &R, i: usize, type_name: &str) -> Value
where
    for<'r> String: sqlx::Decode<'r, R::Database>,
    usize: sqlx::ColumnIndex<R>,
{
    match row.try_get_unchecked::<String, _>(i) {
        Ok(s) if !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') => Value::String(s),
        _ => Value::String(format!("<{type_name}>")),
    }
}
