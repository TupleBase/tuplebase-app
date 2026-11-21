//! Amazon DynamoDB: connecting with an AWS profile, running PartiQL, listing tables and
//! their indexes, describing keys and sampling attributes, and turning DynamoDB's
//! attribute values into JSON.
//!
//! `aws-sdk-dynamodb` is DynamoDB's database driver, so it lives here rather than in
//! `cloud::aws`, which hands it the signed-in `SdkConfig` (L6). Every statement runs
//! alone through `ExecuteStatement`, which takes exactly one; never
//! `BatchExecuteStatement` or `ExecuteTransaction`. Read-only work is checked by
//! `safety::check_partiql` before it gets here, and again here (S2).

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::config::ProvideCredentials;
use aws_sdk_dynamodb::config::timeout::TimeoutConfig;
use aws_sdk_dynamodb::error::DisplayErrorContext;
use aws_sdk_dynamodb::types::{
    AttributeValue, KeySchemaElement, KeyType, Projection, TableDescription,
};
use serde_json::{Number, Value};

use super::connect::ACQUIRE_TIMEOUT;
use super::{ColumnInfo, ObjectInfo, ObjectKind, QueryResult, ScriptResult, Source, TableInfo};
use crate::cloud::aws;
use crate::config::{Adapter, Connection};
use crate::safety;

/// An open DynamoDB client: the SDK keeps its own HTTP connections.
#[derive(Clone)]
pub(super) struct DynamoPool {
    client: Client,
    /// The region it signs for, shown as the schema of every table.
    region: String,
    /// When the credentials it signs with run out, if they do: an SSO session or an
    /// assumed role. `db` connects again before then, which signs in afresh.
    expires: Option<SystemTime>,
}

impl DynamoPool {
    /// Whether its credentials have run out, or will within a minute.
    pub(super) fn expired(&self) -> bool {
        self.expires
            .is_some_and(|at| SystemTime::now() + Duration::from_secs(60) >= at)
    }
}

/// The most pages one statement reads. PartiQL has no `LIMIT`, and a `SELECT` without
/// a key condition scans the table, a page of up to 1 MB at a time.
pub(super) const MAX_PAGES: usize = 10;

/// Said when a statement stopped at [`MAX_PAGES`] with more to read.
pub(super) const PAGES_NOTE: &str = "Stopped after 10 pages: this SELECT likely scans the \
     table. A condition on the partition key reads less.";

/// How many items [`describe`] samples for attributes beyond the keys.
const SAMPLE_SIZE: i32 = 50;

/// The longest one request may take, retries included.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Opens a client for `conn`: signed in with its AWS profile, or, for an endpoint URL
/// with no profile (DynamoDB Local), with dummy credentials.
pub(super) async fn connect(conn: &Connection) -> Result<DynamoPool> {
    let (profile, region) = conn.auth.aws_profile().unwrap_or_default();
    let endpoint = conn.host.trim();
    let config = if !endpoint.is_empty() && profile.trim().is_empty() {
        aws::local_sdk_config(region)
    } else {
        aws::sdk_config(profile, region).await?
    };
    let expires = match config.credentials_provider() {
        Some(provider) => provider
            .provide_credentials()
            .await
            .ok()
            .and_then(|c| c.expiry()),
        None => None,
    };
    let region = config.region().map(ToString::to_string).unwrap_or_default();
    let timeouts = TimeoutConfig::builder()
        .connect_timeout(ACQUIRE_TIMEOUT)
        .operation_timeout(OPERATION_TIMEOUT)
        .build();
    let mut builder = aws_sdk_dynamodb::config::Builder::from(&config).timeout_config(timeouts);
    if !endpoint.is_empty() {
        builder = builder.endpoint_url(endpoint);
    }
    Ok(DynamoPool {
        client: Client::from_conf(builder.build()),
        region,
        expires,
    })
}

/// The SDK's error with its causes, which its own `Display` leaves out.
fn explain<E: std::error::Error>(error: E) -> anyhow::Error {
    anyhow!("{}", DisplayErrorContext(&error))
}

/// Lists one table, which needs the credentials, the region and the endpoint to work.
pub(super) async fn test(pool: &DynamoPool) -> Result<()> {
    pool.client
        .list_tables()
        .limit(1)
        .send()
        .await
        .map_err(explain)?;
    Ok(())
}

/// What one statement read: up to `max_rows` items, whether there were more, and
/// whether it stopped at [`MAX_PAGES`].
struct Read {
    items: Vec<Item>,
    truncated: bool,
    out_of_pages: bool,
}

/// One item as DynamoDB sends it.
type Item = HashMap<String, AttributeValue>;

/// Runs one statement with `ExecuteStatement`, alone, following `NextToken`.
async fn execute(pool: &DynamoPool, statement: &str, max_rows: usize) -> Result<Read> {
    read_pages(
        |token| async move {
            let out = pool
                .client
                .execute_statement()
                .statement(statement)
                .set_next_token(token)
                .send()
                .await
                .map_err(explain)?;
            Ok((out.items.unwrap_or_default(), out.next_token))
        },
        max_rows,
    )
    .await
}

/// Reads pages from `fetch`, given the previous page's token, until `max_rows + 1`
/// items or [`MAX_PAGES`] pages. A page's `Limit` counts the items DynamoDB evaluated,
/// not those it returned, so the row cap is applied here instead.
async fn read_pages<F, Fut>(mut fetch: F, max_rows: usize) -> Result<Read>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<(Vec<Item>, Option<String>)>>,
{
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    for page in 1..=MAX_PAGES {
        let (page_items, next) = fetch(token.take()).await?;
        items.extend(page_items);
        if items.len() > max_rows {
            items.truncate(max_rows);
            return Ok(Read {
                items,
                truncated: true,
                out_of_pages: false,
            });
        }
        match next {
            None => break,
            Some(_) if page == MAX_PAGES => {
                return Ok(Read {
                    items,
                    truncated: true,
                    out_of_pages: true,
                });
            }
            Some(next) => token = Some(next),
        }
    }
    Ok(Read {
        items,
        truncated: false,
        out_of_pages: false,
    })
}

/// Runs `statements` in order, each alone, stopping at the first error. With
/// `read_only`, each must pass `safety::check_partiql` first; `db` has already checked
/// them, so this is a second look. Shows the last statement that returned items, or
/// else the last one.
pub(super) async fn run(
    pool: &DynamoPool,
    adapter: Adapter,
    statements: &[&str],
    read_only: bool,
    max_rows: usize,
) -> Result<ScriptResult> {
    let total = statements.len();
    let numbered = |index: usize, e: anyhow::Error| {
        if total > 1 {
            e.context(format!("statement {} of {total}", index + 1))
        } else {
            e
        }
    };
    if read_only {
        for (index, statement) in statements.iter().enumerate() {
            safety::check_partiql(adapter, statement).map_err(|e| numbered(index, anyhow!(e)))?;
        }
    }
    let started = Instant::now();
    let mut shown: Option<QueryResult> = None;
    let mut changed = false;
    for (index, statement) in statements.iter().enumerate() {
        let read = execute(pool, statement, max_rows)
            .await
            .map_err(|e| numbered(index, e))?;
        changed |= !safety::first_keyword(adapter, statement).is_some_and(|k| k == "select");
        let result = items_result(&read);
        if shown.is_none() || !result.columns.is_empty() {
            shown = Some(result);
        }
    }
    let mut result = shown.context("nothing to run")?;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(ScriptResult {
        result,
        statements: total,
        changed,
        rolled_back: false,
    })
}

/// A result of items: each one as a JSON object, and flattened into columns and rows.
fn items_result(read: &Read) -> QueryResult {
    let documents: Vec<Value> = read.items.iter().map(item).collect();
    let (columns, rows) = super::mongo::flatten(&documents);
    QueryResult {
        columns,
        rows,
        truncated: read.truncated,
        rows_affected: None,
        elapsed_ms: 0,
        note: read.out_of_pages.then_some(PAGES_NOTE),
        documents: Some(documents),
        // DynamoDB can't tell cheaply whether the IAM policy allows writes.
        server_read_only: None,
    }
}

/// One item as a JSON object, its attributes in name order.
pub(super) fn item(attributes: &HashMap<String, AttributeValue>) -> Value {
    let mut names: Vec<&String> = attributes.keys().collect();
    names.sort();
    Value::Object(
        names
            .into_iter()
            .map(|name| (name.clone(), value(&attributes[name])))
            .collect(),
    )
}

/// One attribute value as JSON: `S` a string, `N` a number when it fits an f64 exactly
/// (else its text), `B` base64, `BOOL` a bool, `NULL` null, `M` an object, `L` an array,
/// and the sets `SS`, `NS` and `BS` arrays.
pub(super) fn value(attribute: &AttributeValue) -> Value {
    let base64 = |blob: &aws_sdk_dynamodb::primitives::Blob| {
        Value::String(super::mongo::base64(blob.as_ref()))
    };
    match attribute {
        AttributeValue::S(s) => Value::String(s.clone()),
        AttributeValue::N(n) => number(n),
        AttributeValue::B(b) => base64(b),
        AttributeValue::Bool(b) => Value::Bool(*b),
        AttributeValue::Null(_) => Value::Null,
        AttributeValue::M(map) => item(map),
        AttributeValue::L(list) => Value::Array(list.iter().map(value).collect()),
        AttributeValue::Ss(set) => Value::Array(set.iter().cloned().map(Value::String).collect()),
        AttributeValue::Ns(set) => Value::Array(set.iter().map(|n| number(n)).collect()),
        AttributeValue::Bs(set) => Value::Array(set.iter().map(base64).collect()),
        // A type newer than this SDK.
        _ => Value::Null,
    }
}

/// A DynamoDB number as a JSON number when that loses nothing, else as its text: up to
/// 38 digits fit DynamoDB, far more than an f64 holds.
fn number(text: &str) -> Value {
    if let Ok(n) = text.parse::<i64>() {
        return n.into();
    }
    if let Ok(n) = text.parse::<u64>() {
        return n.into();
    }
    if let Ok(f) = text.parse::<f64>()
        && f.is_finite()
        && decimal(text).is_some()
        && decimal(text) == decimal(&f.to_string())
        && let Some(n) = Number::from_f64(f)
    {
        return Value::Number(n);
    }
    Value::String(text.to_owned())
}

/// A decimal number's sign, significant digits and exponent, so that two spellings of
/// one number compare equal: `1.50` and `15e-1` both give `(false, "15", -1)`.
fn decimal(text: &str) -> Option<(bool, String, i64)> {
    let (negative, rest) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], rest[at + 1..].parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let mut exponent = exponent - fraction.len() as i64;
    let digits = digits.trim_start_matches('0');
    let trimmed = digits.trim_end_matches('0');
    exponent += (digits.len() - trimmed.len()) as i64;
    if trimmed.is_empty() {
        return Some((false, "0".to_owned(), 0));
    }
    Some((negative, trimmed.to_owned(), exponent))
}

/// Every table in the region, or at the endpoint.
pub(super) async fn tables(pool: &DynamoPool) -> Result<Vec<TableInfo>> {
    let mut tables = Vec::new();
    let mut start: Option<String> = None;
    loop {
        let out = pool
            .client
            .list_tables()
            .set_exclusive_start_table_name(start.take())
            .send()
            .await
            .map_err(explain)?;
        tables.extend(
            out.table_names
                .unwrap_or_default()
                .into_iter()
                .map(|name| TableInfo {
                    schema: pool.region.clone(),
                    name,
                    kind: "TABLE".to_owned(),
                }),
        );
        match out.last_evaluated_table_name {
            Some(last) => start = Some(last),
            None => break,
        }
    }
    Ok(tables)
}

async fn describe_table(pool: &DynamoPool, table: &str) -> Result<TableDescription> {
    pool.client
        .describe_table()
        .table_name(table)
        .send()
        .await
        .map_err(explain)?
        .table
        .with_context(|| format!("table \"{table}\" not found"))
}

/// A key's role, as shown beside its type.
fn key_role(key: &KeySchemaElement) -> &'static str {
    match key.key_type {
        KeyType::Hash => "partition key",
        KeyType::Range => "sort key",
        _ => "key",
    }
}

/// A table's keys, such as `pk S (partition key)`, then the other attributes its
/// indexes are keyed on, then the attributes found in a sample of its items, each with
/// the types it was seen with and whether some item lacked it or held `NULL`.
pub(super) async fn describe(pool: &DynamoPool, table: &str) -> Result<Vec<ColumnInfo>> {
    let description = describe_table(pool, table).await?;
    let sample = pool
        .client
        .scan()
        .table_name(table)
        .limit(SAMPLE_SIZE)
        .send()
        .await
        .map_err(explain)?
        .items
        .unwrap_or_default();
    Ok(columns(&description, &sample))
}

fn columns(
    description: &TableDescription,
    sample: &[HashMap<String, AttributeValue>],
) -> Vec<ColumnInfo> {
    let types: HashMap<&str, &str> = description
        .attribute_definitions()
        .iter()
        .map(|d| (d.attribute_name.as_str(), d.attribute_type.as_str()))
        .collect();
    let mut columns: Vec<ColumnInfo> = description
        .key_schema()
        .iter()
        .map(|key| ColumnInfo {
            name: key.attribute_name.clone(),
            data_type: format!(
                "{} ({})",
                types.get(key.attribute_name.as_str()).unwrap_or(&"?"),
                key_role(key)
            ),
            nullable: false,
        })
        .collect();
    for definition in description.attribute_definitions() {
        if columns.iter().all(|c| c.name != definition.attribute_name) {
            columns.push(ColumnInfo {
                name: definition.attribute_name.clone(),
                data_type: format!("{} (index key)", definition.attribute_type.as_str()),
                nullable: true,
            });
        }
    }
    let mut order: Vec<&str> = Vec::new();
    let mut seen: HashMap<&str, (BTreeSet<&'static str>, usize)> = HashMap::new();
    for item in sample {
        let mut names: Vec<&String> = item.keys().collect();
        names.sort();
        for name in names {
            let entry = seen.entry(name.as_str()).or_insert_with(|| {
                order.push(name.as_str());
                (BTreeSet::new(), 0)
            });
            entry.0.insert(type_name(&item[name]));
            entry.1 += 1;
        }
    }
    for name in order {
        if columns.iter().any(|c| c.name == name) {
            continue;
        }
        let (kinds, count) = &seen[name];
        let mut names: Vec<&str> = kinds.iter().copied().filter(|k| *k != "null").collect();
        if kinds.contains("null") {
            names.push("null");
        }
        columns.push(ColumnInfo {
            name: name.to_owned(),
            data_type: names.join("|"),
            nullable: *count < sample.len() || kinds.contains("null"),
        });
    }
    columns
}

fn type_name(attribute: &AttributeValue) -> &'static str {
    match attribute {
        AttributeValue::S(_) => "string",
        AttributeValue::N(_) => "number",
        AttributeValue::B(_) => "binary",
        AttributeValue::Bool(_) => "bool",
        AttributeValue::Null(_) => "null",
        AttributeValue::M(_) => "map",
        AttributeValue::L(_) => "list",
        AttributeValue::Ss(_) => "string set",
        AttributeValue::Ns(_) => "number set",
        AttributeValue::Bs(_) => "binary set",
        _ => "unknown",
    }
}

/// The global and local secondary indexes of every table, each named with its table in
/// the detail (`on orders`). Best effort: a table that can't be described is left out.
pub(super) async fn list_objects(pool: &DynamoPool) -> Vec<ObjectInfo> {
    let Ok(tables) = tables(pool).await else {
        return Vec::new();
    };
    let mut objects = Vec::new();
    for table in tables {
        let Ok(description) = describe_table(pool, &table.name).await else {
            continue;
        };
        let global = description
            .global_secondary_indexes()
            .iter()
            .filter_map(|i| i.index_name.clone());
        let local = description
            .local_secondary_indexes()
            .iter()
            .filter_map(|i| i.index_name.clone());
        for name in global.chain(local) {
            objects.push(ObjectInfo {
                schema: table.schema.clone(),
                kind: ObjectKind::Index,
                name,
                detail: format!("on {}", table.name),
            });
        }
    }
    objects
}

/// A secondary index described in words, to read: its kind, keys and projection.
pub(super) async fn object_definition(pool: &DynamoPool, object: &ObjectInfo) -> Result<Source> {
    if object.kind != ObjectKind::Index {
        return Err(super::unsupported(object.kind));
    }
    let Some(table) = object.detail.strip_prefix("on ") else {
        return Ok(None);
    };
    let description = describe_table(pool, table).await?;
    Ok(index_text(&description, table, &object.name).map(Some))
}

fn index_text(description: &TableDescription, table: &str, name: &str) -> Option<String> {
    let named = |n: &Option<String>| n.as_deref() == Some(name);
    let (kind, keys, projection, status) = if let Some(index) = description
        .global_secondary_indexes()
        .iter()
        .find(|i| named(&i.index_name))
    {
        let status = index.index_status.as_ref().map(|s| s.as_str().to_owned());
        (
            "Global",
            index.key_schema(),
            index.projection.as_ref(),
            status,
        )
    } else {
        let index = description
            .local_secondary_indexes()
            .iter()
            .find(|i| named(&i.index_name))?;
        ("Local", index.key_schema(), index.projection.as_ref(), None)
    };
    let types: HashMap<&str, &str> = description
        .attribute_definitions()
        .iter()
        .map(|d| (d.attribute_name.as_str(), d.attribute_type.as_str()))
        .collect();
    let keys: Vec<String> = keys
        .iter()
        .map(|key| {
            let kind = types.get(key.attribute_name.as_str()).unwrap_or(&"?");
            format!("{} {kind} ({})", key.attribute_name, key_role(key))
        })
        .collect();
    let mut text = format!(
        "{kind} secondary index {name} on {table}\nKeys: {}\nProjection: {}",
        keys.join(", "),
        projection_text(projection)
    );
    if let Some(status) = status {
        text.push_str(&format!("\nStatus: {status}"));
    }
    Some(text)
}

fn projection_text(projection: Option<&Projection>) -> String {
    let Some(projection) = projection else {
        return "?".to_owned();
    };
    let kind = projection
        .projection_type
        .as_ref()
        .map_or("?", |t| t.as_str());
    match projection.non_key_attributes() {
        [] => kind.to_owned(),
        attributes => format!("{kind} ({})", attributes.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_dynamodb::primitives::Blob;
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, GlobalSecondaryIndexDescription, IndexStatus, ProjectionType,
        ScalarAttributeType,
    };
    use serde_json::json;

    use super::*;

    fn s(text: &str) -> AttributeValue {
        AttributeValue::S(text.into())
    }

    fn n(text: &str) -> AttributeValue {
        AttributeValue::N(text.into())
    }

    #[test]
    fn attribute_values_become_json() {
        let attributes = HashMap::from([
            ("pk".to_owned(), s("user#1")),
            ("age".to_owned(), n("42")),
            ("ok".to_owned(), AttributeValue::Bool(true)),
            ("gone".to_owned(), AttributeValue::Null(true)),
            (
                "raw".to_owned(),
                AttributeValue::B(Blob::new(b"hi!".to_vec())),
            ),
            (
                "address".to_owned(),
                AttributeValue::M(HashMap::from([("city".to_owned(), s("Rome"))])),
            ),
            ("list".to_owned(), AttributeValue::L(vec![n("1.5"), s("x")])),
            (
                "tags".to_owned(),
                AttributeValue::Ss(vec!["a".into(), "b".into()]),
            ),
            (
                "scores".to_owned(),
                AttributeValue::Ns(vec!["1".into(), "2.25".into()]),
            ),
            (
                "blobs".to_owned(),
                AttributeValue::Bs(vec![Blob::new(b"a".to_vec())]),
            ),
        ]);
        let json = item(&attributes);
        assert_eq!(
            json,
            json!({
                "address": {"city": "Rome"},
                "age": 42,
                "blobs": ["YQ=="],
                "gone": null,
                "list": [1.5, "x"],
                "ok": true,
                "pk": "user#1",
                "raw": "aGkh",
                "scores": [1, 2.25],
                "tags": ["a", "b"],
            })
        );
        // In name order.
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        assert_eq!(keys.first().map(|k| k.as_str()), Some("address"));
    }

    #[test]
    fn numbers_stay_exact() {
        assert_eq!(number("7"), json!(7));
        assert_eq!(number("-7"), json!(-7));
        assert_eq!(
            number("18446744073709551615"),
            json!(18446744073709551615u64)
        );
        assert_eq!(number("0.1"), json!(0.1));
        assert_eq!(number("1.50"), json!(1.5));
        assert_eq!(number("1e3"), json!(1000.0));
        assert_eq!(number("-0.0"), json!(0.0));
        // Too many digits for an f64: kept as text.
        for text in [
            "12345678901234567890123456789",
            "0.12345678901234567890123",
            "9007199254740993.5",
        ] {
            assert_eq!(number(text), json!(text), "{text}");
        }
        assert_eq!(decimal("15e-1"), decimal("1.5"));
        assert_eq!(decimal("001.500"), Some((false, "15".into(), -1)));
        assert_eq!(decimal("x"), None);
    }

    fn table() -> TableDescription {
        let key = |name: &str, kind| {
            KeySchemaElement::builder()
                .attribute_name(name)
                .key_type(kind)
                .build()
                .unwrap()
        };
        let definition = |name: &str, kind| {
            AttributeDefinition::builder()
                .attribute_name(name)
                .attribute_type(kind)
                .build()
                .unwrap()
        };
        TableDescription::builder()
            .table_name("orders")
            .key_schema(key("pk", KeyType::Hash))
            .key_schema(key("sk", KeyType::Range))
            .attribute_definitions(definition("pk", ScalarAttributeType::S))
            .attribute_definitions(definition("sk", ScalarAttributeType::N))
            .attribute_definitions(definition("user", ScalarAttributeType::S))
            .global_secondary_indexes(
                GlobalSecondaryIndexDescription::builder()
                    .index_name("by_user")
                    .key_schema(key("user", KeyType::Hash))
                    .projection(
                        Projection::builder()
                            .projection_type(ProjectionType::Include)
                            .non_key_attributes("total")
                            .build(),
                    )
                    .index_status(IndexStatus::Active)
                    .build(),
            )
            .build()
    }

    #[test]
    fn keys_come_first_then_sampled_attributes() {
        let sample = [
            HashMap::from([
                ("pk".to_owned(), s("a")),
                ("sk".to_owned(), n("1")),
                ("total".to_owned(), n("3")),
            ]),
            HashMap::from([
                ("pk".to_owned(), s("b")),
                ("sk".to_owned(), n("2")),
                ("total".to_owned(), s("n/a")),
                ("note".to_owned(), AttributeValue::Null(true)),
            ]),
        ];
        let shown: Vec<(String, String, bool)> = columns(&table(), &sample)
            .into_iter()
            .map(|c| (c.name, c.data_type, c.nullable))
            .collect();
        let row = |n: &str, t: &str, null| (n.to_owned(), t.to_owned(), null);
        assert_eq!(
            shown,
            [
                row("pk", "S (partition key)", false),
                row("sk", "N (sort key)", false),
                row("user", "S (index key)", true),
                row("total", "number|string", false),
                row("note", "null", true),
            ]
        );
    }

    #[test]
    fn an_index_reads_as_text() {
        assert_eq!(
            index_text(&table(), "orders", "by_user").unwrap(),
            "Global secondary index by_user on orders\nKeys: user S (partition key)\n\
             Projection: INCLUDE (total)\nStatus: ACTIVE"
        );
        assert_eq!(index_text(&table(), "orders", "missing"), None);
    }

    /// Pages of `per_page` items each, `pages` of them, recording the tokens asked for.
    async fn paged(per_page: usize, pages: usize, max_rows: usize) -> (Read, Vec<Option<String>>) {
        let asked = std::cell::RefCell::new(Vec::new());
        let read = read_pages(
            |token: Option<String>| {
                let page = token.as_deref().map_or(1, |t| t.parse::<usize>().unwrap());
                asked.borrow_mut().push(token);
                async move {
                    let items = (0..per_page)
                        .map(|i| HashMap::from([("pk".to_owned(), s(&format!("{page}-{i}")))]))
                        .collect();
                    let next = (page < pages).then(|| (page + 1).to_string());
                    Ok((items, next))
                }
            },
            max_rows,
        )
        .await
        .unwrap();
        (read, asked.into_inner())
    }

    #[tokio::test]
    async fn pages_are_followed_up_to_the_row_cap_or_the_page_cap() {
        // Everything fits: every page is read, nothing more.
        let (read, asked) = paged(3, 4, 100).await;
        assert_eq!(
            (read.items.len(), read.truncated, read.out_of_pages),
            (12, false, false)
        );
        assert_eq!(
            asked,
            [None, Some("2".into()), Some("3".into()), Some("4".into())]
        );
        // The row cap stops it as soon as one item more than it has arrived.
        let (read, asked) = paged(3, 4, 7).await;
        assert_eq!(
            (read.items.len(), read.truncated, read.out_of_pages),
            (7, true, false)
        );
        assert_eq!(asked.len(), 3);
        // Exactly the cap, with no more pages: not cut short.
        let (read, _) = paged(3, 2, 6).await;
        assert_eq!((read.items.len(), read.truncated), (6, false));
        // A scan with no end stops at the page cap, and says so.
        let (read, asked) = paged(1, 1000, 1000).await;
        assert_eq!(asked.len(), MAX_PAGES);
        assert_eq!(
            (read.items.len(), read.truncated, read.out_of_pages),
            (MAX_PAGES, true, true)
        );
        // Empty pages with a token still count as pages.
        let (read, asked) = paged(0, 1000, 10).await;
        assert_eq!(
            (asked.len(), read.items.len(), read.out_of_pages),
            (MAX_PAGES, 0, true)
        );
    }

    #[test]
    fn a_page_cap_says_so() {
        let read = Read {
            items: vec![HashMap::from([("pk".to_owned(), s("a"))])],
            truncated: true,
            out_of_pages: true,
        };
        let result = items_result(&read);
        assert_eq!(result.note, Some(PAGES_NOTE));
        assert!(result.truncated);
        assert_eq!(result.columns, ["pk"]);
        assert_eq!(result.server_read_only, None);
    }
}
