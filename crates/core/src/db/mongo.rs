//! MongoDB and Amazon DocumentDB: connecting, running shell commands, listing
//! collections and indexes, sampling fields, and turning BSON into JSON.
//!
//! One module for both, as `mysql.rs` serves MySQL and MariaDB; where DocumentDB
//! differs, the code says so. Commands come parsed by `mongo_shell`. Read-only work only
//! ever reaches [`run_reads`], which takes [`ReadOp`]s: a type with no write in it, each
//! mapped to one typed driver call that only reads.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{
    AggregateOptions as AggregateOptionsDriver, AuthMechanism, ClientOptions, CountOptions,
    Credential, DistinctOptions, EstimatedDocumentCountOptions, FindOneOptions, FindOptions,
    IndexOptions, ReplaceOptions, Tls, TlsOptions, UpdateOptions,
};
use mongodb::results::CollectionType;
use mongodb::{Client, Collection, IndexModel};
use serde_json::{Map, Number, Value};

use super::connect::{ACQUIRE_TIMEOUT, POOL_SIZE};
use super::{ColumnInfo, ObjectInfo, ObjectKind, QueryResult, ScriptResult, Source, TableInfo};
use crate::config::{Adapter, Connection, TlsMode};
use crate::mongo_shell::{AggregateOptions, Command, Op, ReadOp, Target};

/// An open MongoDB or DocumentDB client, and what was learnt when it connected.
#[derive(Clone)]
pub(super) struct MongoPool {
    client: Client,
    /// The database a command without `getSiblingDB` works on.
    default_db: String,
    /// Whether the server refuses this connection's writes; see [`server_read_only`].
    pub(super) server_read_only: Option<bool>,
}

/// Databases the server keeps for itself, left out of the schema.
const SYSTEM_DATABASES: [&str; 3] = ["admin", "local", "config"];

/// The longest the server may spend on one read-only find, aggregate, count or
/// distinct: the default when none is given, and the most one may ask for. The SQL
/// engines set no statement timeout, so this is a fixed minute.
pub(super) const READ_ONLY_MAX_TIME: Duration = Duration::from_secs(60);

/// How many documents [`fields`] samples.
const SAMPLE_SIZE: i64 = 100;

/// The most columns a result is flattened into; the documents keep every field.
const MAX_COLUMNS: usize = 200;

/// The connection string, with no user or password in it: those are set in code, so the
/// password is never in a string. `host` and `port` are where to connect, the tunnel's
/// local end when there is one.
fn uri(conn: &Connection, host: &str, port: u16) -> Result<String> {
    let host = host.trim();
    let bad = |c: char| c.is_whitespace() || "@/?#,%[]".contains(c);
    if host.is_empty() || host.contains(bad) {
        bail!("the host must be a plain host name or address, not \"{host}\"");
    }
    let settings = conn.mongo.clone().unwrap_or_default();
    // Checked when saved, and again here for a file edited by hand (S1).
    settings.check()?;
    let options = settings.options.trim().trim_start_matches('?');
    if options.contains('#') {
        bail!("the connection options can't hold `#`");
    }
    // An IPv6 address goes in brackets.
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(if settings.srv && conn.ssh.is_none() {
        format!("mongodb+srv://{host}/?{options}")
    } else {
        format!("mongodb://{host}:{port}/?{options}")
    })
}

/// The driver's options for `conn`. What the form sets wins over the options string:
/// encryption, the user and password, and DocumentDB's needs.
pub(super) async fn options(
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<&str>,
) -> Result<ClientOptions> {
    let uri = uri(conn, host, port)?;
    let mut options = ClientOptions::parse(&uri)
        .await
        .context("reading the connection options")?;
    apply(&mut options, conn, password);
    Ok(options)
}

fn apply(options: &mut ClientOptions, conn: &Connection, password: Option<&str>) {
    options.app_name = Some("TupleBase".into());
    options.server_selection_timeout = Some(ACQUIRE_TIMEOUT);
    options.connect_timeout = Some(ACQUIRE_TIMEOUT);
    options.max_pool_size = Some(POOL_SIZE);
    options.tls = Some(match conn.tls {
        // As the connection string says: TLS for an SRV address or `tls=true`, else
        // plain, as MongoDB serves by default.
        TlsMode::Prefer => match options.tls.take() {
            Some(Tls::Enabled(tls)) => Tls::Enabled(tls),
            _ => Tls::Disabled,
        },
        TlsMode::Require => Tls::Enabled(
            TlsOptions::builder()
                .allow_invalid_certificates(true)
                .build(),
        ),
        TlsMode::Verify => Tls::Enabled(TlsOptions::default()),
    });
    // A tunnel reaches one server: the replica set's other members, by the names it
    // gives them, are out of reach.
    if conn.ssh.is_some() {
        options.direct_connection = Some(true);
    }
    // DocumentDB doesn't support retryable writes.
    if conn.adapter == Adapter::Documentdb {
        options.retry_writes = Some(false);
    }
    let user = conn.user.trim();
    // The driver keeps `authSource` and `authMechanism` only beside a user in the
    // string, and the user is set here instead, so they're read from the options too.
    let settings = conn.mongo.clone().unwrap_or_default();
    let option = |name: &str| {
        settings
            .pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    let parsed = options.credential.take().or_else(|| {
        let mechanism = option("authmechanism").and_then(|m| m.parse::<AuthMechanism>().ok());
        let source = option("authsource");
        (mechanism.is_some() || source.is_some()).then(|| {
            Credential::builder()
                .mechanism(mechanism)
                .source(source)
                .build()
        })
    });
    options.credential = if user.is_empty() {
        // A mechanism that needs no user, such as X.509, is kept; a bare authSource is
        // nothing to sign in with.
        parsed.filter(|c| c.mechanism.is_some())
    } else {
        let parsed = parsed.unwrap_or_default();
        Some(
            Credential::builder()
                .username(user.to_owned())
                .password(password.map(str::to_owned))
                .source(parsed.source.or_else(|| Some("admin".to_owned())))
                .mechanism(parsed.mechanism)
                .mechanism_properties(parsed.mechanism_properties)
                .build(),
        )
    };
}

/// The database a command without `getSiblingDB` works on: the connection's, or `test`
/// as in the shell.
fn default_db(conn: &Connection) -> String {
    match conn.database.trim() {
        "" => "test".to_owned(),
        name => name.to_owned(),
    }
}

/// Connects and checks the server answers, then asks it whether it refuses this
/// connection's writes.
pub(super) async fn connect(
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<&str>,
) -> Result<MongoPool> {
    let options = options(conn, host, port, password).await?;
    let direct = options.direct_connection == Some(true);
    let client = Client::with_options(options)?;
    // The client connects lazily: this is where a wrong host or password shows.
    ping(&client).await?;
    let server_read_only = probe(&client, direct).await;
    Ok(MongoPool {
        client,
        default_db: default_db(conn),
        server_read_only,
    })
}

pub(super) async fn ping(client: &Client) -> Result<()> {
    client
        .database("admin")
        .run_command(doc! {"ping": 1})
        .await?;
    Ok(())
}

/// Closes the client without waiting for work still running on other clones of it.
pub(super) async fn close(pool: &MongoPool) {
    pool.client.clone().shutdown().immediate(true).await;
}

pub(super) async fn test(pool: &MongoPool) -> Result<()> {
    ping(&pool.client).await
}

/// `hello` and `connectionStatus` with privileges, read by [`server_read_only`]. A
/// command that fails counts as no answer.
async fn probe(client: &Client, direct: bool) -> Option<bool> {
    let admin = client.database("admin");
    let hello = admin.run_command(doc! {"hello": 1}).await.ok();
    let status = admin
        .run_command(doc! {"connectionStatus": 1, "showPrivileges": true})
        .await
        .ok();
    server_read_only(hello.as_ref(), status.as_ref(), direct)
}

/// The privilege actions that only read, as the built-in `read` role grants them.
/// Anything else may write, or is unknown, so it doesn't count as reading.
const READ_ACTIONS: [&str; 11] = [
    "changeStream",
    "collStats",
    "dbHash",
    "dbStats",
    "find",
    "killCursors",
    "listCollections",
    "listDatabases",
    "listIndexes",
    "listSearchIndexes",
    "planCacheRead",
];

/// Whether the server refuses writes from this connection, from its answers to `hello`
/// and `connectionStatus`. It fails closed: `Some(true)` only when it is shown, `None`
/// when it can't be told.
///
/// - A secondary refuses writes, but only counts with a direct connection: otherwise the
///   driver sends writes to the primary.
/// - Otherwise the user's privileges decide: `Some(true)` when every action is in
///   [`READ_ACTIONS`], `Some(false)` when one isn't, and `None` when the server sent
///   none, or something unexpected (no user, or a server that doesn't say, such as
///   DocumentDB may be).
pub(super) fn server_read_only(
    hello: Option<&Document>,
    status: Option<&Document>,
    direct: bool,
) -> Option<bool> {
    if let Some(hello) = hello
        && direct
        && hello.get_bool("isWritablePrimary").ok() == Some(false)
        && hello.get_bool("secondary").ok() == Some(true)
    {
        return Some(true);
    }
    let privileges = status?
        .get_document("authInfo")
        .ok()?
        .get_array("authenticatedUserPrivileges")
        .ok()?;
    if privileges.is_empty() {
        return None;
    }
    let mut reads_only = true;
    for privilege in privileges {
        let actions = privilege.as_document()?.get_array("actions").ok()?;
        for action in actions {
            reads_only &= READ_ACTIONS.contains(&action.as_str()?);
        }
    }
    Some(reads_only)
}

impl MongoPool {
    fn database(&self, name: Option<&str>) -> mongodb::Database {
        self.client.database(name.unwrap_or(&self.default_db))
    }

    fn collection(&self, target: &Target) -> Collection<Document> {
        self.database(target.database.as_deref())
            .collection(&target.collection)
    }
}

/// Runs read-only work. Takes only reads, so nothing here can write, whatever the
/// guard let through; the server's own refusal, when it has one, comes on top.
pub(super) async fn run_reads(
    pool: &MongoPool,
    reads: Vec<ReadOp>,
    max_rows: usize,
) -> Result<ScriptResult> {
    let started = Instant::now();
    let total = reads.len();
    let mut shown = None;
    for (index, op) in reads.into_iter().enumerate() {
        let result = read(pool, op, max_rows, Some(READ_ONLY_MAX_TIME)).await;
        shown = Some(numbered(result, index, total)?);
    }
    finish(pool, shown, total, false, started)
}

/// Runs commands on a connection that may write.
pub(super) async fn run(
    pool: &MongoPool,
    commands: Vec<Command>,
    max_rows: usize,
) -> Result<ScriptResult> {
    let started = Instant::now();
    let total = commands.len();
    let mut shown: Option<QueryResult> = None;
    let mut changed = false;
    for (index, command) in commands.into_iter().enumerate() {
        let result = match command.as_read_unguarded() {
            Some(op) => read(pool, op, max_rows, None).await,
            None => {
                changed = true;
                write(pool, command).await
            }
        };
        let result = numbered(result, index, total)?;
        // As for SQL: the last result with rows, or else the last one.
        if shown.is_none() || !result.columns.is_empty() {
            shown = Some(result);
        }
    }
    finish(pool, shown, total, changed, started)
}

fn numbered(result: Result<QueryResult>, index: usize, total: usize) -> Result<QueryResult> {
    if total > 1 {
        result.with_context(|| format!("statement {} of {total}", index + 1))
    } else {
        result
    }
}

fn finish(
    pool: &MongoPool,
    shown: Option<QueryResult>,
    total: usize,
    changed: bool,
    started: Instant,
) -> Result<ScriptResult> {
    let mut result = shown.ok_or_else(|| anyhow!("nothing to run"))?;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    result.server_read_only = pool.server_read_only;
    Ok(ScriptResult {
        result,
        statements: total,
        changed,
        rolled_back: false,
    })
}

/// Reads up to `max_rows` documents, and whether there were more.
async fn collect(
    mut cursor: mongodb::Cursor<Document>,
    max_rows: usize,
) -> Result<(Vec<Document>, bool)> {
    let mut docs = Vec::new();
    while let Some(doc) = cursor.try_next().await? {
        if docs.len() == max_rows {
            return Ok((docs, true));
        }
        docs.push(doc);
    }
    Ok((docs, false))
}

/// The shorter of two time limits, either of which may be unset.
fn shorter(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The one typed driver call for each read. `max_time`, set for read-only work, caps
/// how long the server may spend on it.
async fn read(
    pool: &MongoPool,
    op: ReadOp,
    max_rows: usize,
    max_time: Option<Duration>,
) -> Result<QueryResult> {
    let docs = match op {
        ReadOp::ListDatabases => {
            let databases = pool
                .client
                .list_databases()
                .authorized_databases(true)
                .await?;
            let docs = databases
                .into_iter()
                .map(|d| doc! {"name": d.name, "sizeOnDisk": d.size_on_disk as i64, "empty": d.empty})
                .collect();
            (docs, false)
        }
        ReadOp::ListCollections { database } => {
            let cursor = pool
                .database(database.as_deref())
                .list_collections()
                .authorized_collections(true)
                .await?;
            let specs: Vec<_> = cursor.try_collect().await?;
            let docs = specs
                .into_iter()
                .map(|s| doc! {"name": s.name, "type": collection_type(&s.collection_type)})
                .collect();
            (docs, false)
        }
        ReadOp::Find {
            target,
            filter,
            projection,
            sort,
            limit,
            skip,
        } => {
            // The server stops at the user's limit or one past the row cap, whichever
            // comes first; a negative limit means the same as a positive one here.
            let cap = max_rows as i64 + 1;
            let limit = limit.map(i64::abs).filter(|&n| n != 0);
            let mut options = FindOptions::default();
            options.projection = projection;
            options.sort = sort;
            options.skip = skip;
            options.limit = Some(limit.map_or(cap, |n| n.min(cap)));
            options.max_time = max_time;
            let cursor = pool
                .collection(&target)
                .find(filter)
                .with_options(options)
                .await?;
            collect(cursor, max_rows).await?
        }
        ReadOp::FindOne {
            target,
            filter,
            projection,
        } => {
            let mut options = FindOneOptions::default();
            options.projection = projection;
            options.max_time = max_time;
            let found = pool
                .collection(&target)
                .find_one(filter)
                .with_options(options)
                .await?;
            (found.into_iter().collect(), false)
        }
        ReadOp::Aggregate {
            target,
            pipeline,
            options,
        } => {
            let AggregateOptions {
                allow_disk_use,
                max_time_ms,
                batch_size,
            } = options;
            let mut options = AggregateOptionsDriver::default();
            options.allow_disk_use = allow_disk_use;
            options.max_time = shorter(max_time_ms.map(Duration::from_millis), max_time);
            options.batch_size = batch_size;
            let cursor = pool
                .collection(&target)
                .aggregate(pipeline)
                .with_options(options)
                .await?;
            collect(cursor, max_rows).await?
        }
        ReadOp::CountDocuments { target, filter } => {
            let mut options = CountOptions::default();
            options.max_time = max_time;
            let count = pool
                .collection(&target)
                .count_documents(filter)
                .with_options(options)
                .await?;
            (vec![doc! {"count": count as i64}], false)
        }
        ReadOp::EstimatedDocumentCount { target } => {
            let mut options = EstimatedDocumentCountOptions::default();
            options.max_time = max_time;
            let count = pool
                .collection(&target)
                .estimated_document_count()
                .with_options(options)
                .await?;
            (vec![doc! {"count": count as i64}], false)
        }
        ReadOp::Distinct {
            target,
            field,
            filter,
        } => {
            let mut options = DistinctOptions::default();
            options.max_time = max_time;
            let mut values = pool
                .collection(&target)
                .distinct(&field, filter)
                .with_options(options)
                .await?;
            let truncated = values.len() > max_rows;
            values.truncate(max_rows);
            let docs = values.into_iter().map(|v| doc! {"value": v}).collect();
            (docs, truncated)
        }
        ReadOp::ListIndexes { target } => {
            let cursor = pool.collection(&target).list_indexes().await?;
            let models: Vec<IndexModel> = cursor.try_collect().await?;
            let docs = models
                .iter()
                .map(mongodb::bson::serialize_to_document)
                .collect::<Result<_, _>>()?;
            (docs, false)
        }
        ReadOp::Explain(inner) => {
            let (database, command) = explain_command(*inner)?;
            let plan = pool
                .database(database.as_deref())
                .run_command(command)
                .await?;
            (vec![plan], false)
        }
    };
    let (docs, truncated) = docs;
    Ok(documents_result(&docs, truncated, None))
}

/// The `explain` command for a find or an aggregate, at the `queryPlanner` verbosity,
/// which plans the query without running it.
fn explain_command(op: ReadOp) -> Result<(Option<String>, Document)> {
    let (database, explained) = match op {
        ReadOp::Find {
            target,
            filter,
            projection,
            sort,
            limit,
            skip,
        } => {
            let mut find = doc! {"find": &target.collection, "filter": filter};
            if let Some(projection) = projection {
                find.insert("projection", projection);
            }
            if let Some(sort) = sort {
                find.insert("sort", sort);
            }
            if let Some(limit) = limit {
                find.insert("limit", limit);
            }
            if let Some(skip) = skip {
                find.insert("skip", skip as i64);
            }
            (target.database, find)
        }
        ReadOp::Aggregate {
            target, pipeline, ..
        } => (
            target.database,
            doc! {"aggregate": &target.collection, "pipeline": pipeline, "cursor": {}},
        ),
        _ => bail!("only a find or an aggregate can be explained"),
    };
    Ok((
        database,
        doc! {"explain": explained, "verbosity": "queryPlanner"},
    ))
}

fn collection_type(kind: &CollectionType) -> &'static str {
    match kind {
        CollectionType::View => "view",
        CollectionType::Timeseries => "timeseries",
        _ => "collection",
    }
}

/// Runs a command that may write, through its typed driver call, or `runCommand`'s.
async fn write(pool: &MongoPool, command: Command) -> Result<QueryResult> {
    let (target, op) = match command {
        Command::RunCommand { database, command } => {
            let reply = pool
                .database(database.as_deref())
                .run_command(command)
                .await?;
            return Ok(documents_result(&[reply], false, None));
        }
        Command::Collection { target, op } => (target, op),
        Command::ShowDatabases | Command::ShowCollections { .. } => {
            bail!("this command only reads")
        }
    };
    let coll = pool.collection(&target);
    let (reply, affected) = match op {
        Op::InsertOne { document } => {
            let done = coll.insert_one(document).await?;
            (doc! {"insertedId": done.inserted_id}, 1)
        }
        Op::InsertMany { documents } => {
            let done = coll.insert_many(documents).await?;
            let count = done.inserted_ids.len() as u64;
            (doc! {"insertedCount": count as i64}, count)
        }
        Op::Update {
            many,
            filter,
            update,
            upsert,
        } => {
            let update: mongodb::options::UpdateModifications = match update {
                Bson::Document(d) => d.into(),
                Bson::Array(stages) => stages
                    .into_iter()
                    .map(|s| match s {
                        Bson::Document(d) => Ok(d),
                        _ => Err(anyhow!(
                            "each stage of an update pipeline must be a document"
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into(),
                _ => bail!("the update must be a document or a pipeline"),
            };
            let mut options = UpdateOptions::default();
            options.upsert = upsert;
            let done = if many {
                coll.update_many(filter, update)
                    .with_options(options)
                    .await?
            } else {
                coll.update_one(filter, update)
                    .with_options(options)
                    .await?
            };
            let mut reply = doc! {
                "matched": done.matched_count as i64,
                "modified": done.modified_count as i64,
            };
            if let Some(id) = done.upserted_id {
                reply.insert("upsertedId", id);
            }
            (reply, done.modified_count)
        }
        Op::ReplaceOne {
            filter,
            replacement,
            upsert,
        } => {
            let mut options = ReplaceOptions::default();
            options.upsert = upsert;
            let done = coll
                .replace_one(filter, replacement)
                .with_options(options)
                .await?;
            let mut reply = doc! {
                "matched": done.matched_count as i64,
                "modified": done.modified_count as i64,
            };
            if let Some(id) = done.upserted_id {
                reply.insert("upsertedId", id);
            }
            (reply, done.modified_count)
        }
        Op::Delete { many, filter } => {
            let done = if many {
                coll.delete_many(filter).await?
            } else {
                coll.delete_one(filter).await?
            };
            (
                doc! {"deleted": done.deleted_count as i64},
                done.deleted_count,
            )
        }
        Op::CreateIndex { keys, options } => {
            let options: Option<IndexOptions> = options
                .map(mongodb::bson::deserialize_from_document)
                .transpose()
                .context("reading the index options")?;
            let model = IndexModel::builder().keys(keys).options(options).build();
            let done = coll.create_index(model).await?;
            (doc! {"name": done.index_name}, 0)
        }
        Op::DropIndex { index } => {
            match index {
                Bson::String(name) => coll.drop_index(name).await?,
                keys => {
                    pool.database(target.database.as_deref())
                        .run_command(doc! {"dropIndexes": &target.collection, "index": keys})
                        .await?;
                }
            }
            (doc! {"dropped": true}, 0)
        }
        Op::Drop => {
            coll.drop().await?;
            (doc! {"dropped": true}, 0)
        }
        read => bail!("{read:?} only reads"),
    };
    Ok(documents_result(&[reply], false, Some(affected)))
}

/// A result of documents: each one exactly, in relaxed Extended JSON, and flattened into
/// columns and rows for the table.
fn documents_result(docs: &[Document], truncated: bool, affected: Option<u64>) -> QueryResult {
    let documents: Vec<Value> = docs
        .iter()
        .map(|d| value(&Bson::Document(d.clone())))
        .collect();
    let (columns, rows) = flatten(&documents);
    QueryResult {
        columns,
        rows,
        truncated,
        rows_affected: affected,
        elapsed_ms: 0,
        note: None,
        documents: Some(documents),
        server_read_only: None,
    }
}

/// The table for documents: one column per top-level key, in the order first seen with
/// `_id` first, at most [`MAX_COLUMNS`]; a key a document lacks is `null` in its row.
pub(super) fn flatten(documents: &[Value]) -> (Vec<String>, Vec<Vec<Value>>) {
    let mut columns: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let has_id = documents
        .iter()
        .any(|d| d.as_object().is_some_and(|o| o.contains_key("_id")));
    if has_id {
        columns.push("_id".to_owned());
        seen.insert("_id".to_owned());
    }
    for doc in documents {
        let Some(object) = doc.as_object() else {
            continue;
        };
        for key in object.keys() {
            if columns.len() == MAX_COLUMNS {
                break;
            }
            if seen.insert(key.clone()) {
                columns.push(key.clone());
            }
        }
    }
    let rows = documents
        .iter()
        .map(|doc| {
            columns
                .iter()
                .map(|c| doc.get(c).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    (columns, rows)
}

/// One BSON value as relaxed Extended JSON: numbers as JSON numbers when they fit,
/// other types as `{"$oid": ...}`, `{"$date": ...}` and the like.
pub(super) fn value(bson: &Bson) -> Value {
    let tagged = |tag: &str, inner: Value| {
        let mut map = Map::new();
        map.insert(tag.to_owned(), inner);
        Value::Object(map)
    };
    match bson {
        Bson::Double(f) if f.is_finite() => Number::from_f64(*f).map_or(Value::Null, Value::Number),
        Bson::Double(f) => {
            let text = if f.is_nan() {
                "NaN"
            } else if *f > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            };
            tagged("$numberDouble", text.into())
        }
        Bson::String(s) => Value::String(s.clone()),
        Bson::Array(items) => Value::Array(items.iter().map(value).collect()),
        Bson::Document(doc) => {
            Value::Object(doc.iter().map(|(k, v)| (k.clone(), value(v))).collect())
        }
        Bson::Boolean(b) => Value::Bool(*b),
        Bson::Null => Value::Null,
        Bson::RegularExpression(re) => tagged(
            "$regularExpression",
            serde_json::json!({"pattern": re.pattern.as_str(), "options": re.options.as_str()}),
        ),
        Bson::JavaScriptCode(code) => tagged("$code", code.clone().into()),
        Bson::JavaScriptCodeWithScope(code) => serde_json::json!({
            "$code": code.code,
            "$scope": value(&Bson::Document(code.scope.clone())),
        }),
        Bson::Int32(n) => Value::from(*n),
        Bson::Int64(n) => Value::from(*n),
        Bson::Timestamp(ts) => tagged(
            "$timestamp",
            serde_json::json!({"t": ts.time, "i": ts.increment}),
        ),
        Bson::Binary(binary) => tagged(
            "$binary",
            serde_json::json!({
                "base64": base64(&binary.bytes),
                "subType": format!("{:02x}", u8::from(binary.subtype)),
            }),
        ),
        Bson::ObjectId(id) => tagged("$oid", id.to_hex().into()),
        Bson::DateTime(date) => {
            let millis = date.timestamp_millis();
            // Relaxed form: ISO text for years 1970 to 9999, else the number.
            let iso = (0..=253_402_300_799_999)
                .contains(&millis)
                .then(|| date.try_to_rfc3339_string().ok())
                .flatten();
            match iso {
                Some(text) => tagged("$date", text.into()),
                None => tagged("$date", tagged("$numberLong", millis.to_string().into())),
            }
        }
        Bson::Symbol(s) => tagged("$symbol", s.clone().into()),
        Bson::Decimal128(d) => tagged("$numberDecimal", d.to_string().into()),
        Bson::Undefined => tagged("$undefined", true.into()),
        Bson::MaxKey => tagged("$maxKey", 1.into()),
        Bson::MinKey => tagged("$minKey", 1.into()),
        Bson::DbPointer(pointer) => tagged("$dbPointer", format!("{pointer:?}").into()),
    }
}

/// Standard base64, with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Collections and views in each database the user may see, without the server's own.
pub(super) async fn tables(pool: &MongoPool) -> Result<Vec<TableInfo>> {
    let mut tables = Vec::new();
    for database in databases(pool).await? {
        let cursor = pool
            .client
            .database(&database)
            .list_collections()
            .authorized_collections(true)
            .await?;
        let mut specs: Vec<_> = cursor.try_collect().await?;
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        for spec in specs {
            if spec.name.starts_with("system.") {
                continue;
            }
            let kind = match spec.collection_type {
                CollectionType::View => "VIEW",
                _ => "COLLECTION",
            };
            tables.push(TableInfo {
                schema: database.clone(),
                name: spec.name,
                kind: kind.to_owned(),
            });
        }
    }
    Ok(tables)
}

/// The databases the user may see, without the server's own. A user who may not list
/// them sees the connection's own.
async fn databases(pool: &MongoPool) -> Result<Vec<String>> {
    let listed = pool
        .client
        .list_database_names()
        .authorized_databases(true)
        .await;
    let mut names: Vec<String> = match listed {
        Ok(names) => names
            .into_iter()
            .filter(|n| !SYSTEM_DATABASES.contains(&n.as_str()))
            .collect(),
        Err(_) => Vec::new(),
    };
    if names.is_empty() {
        names.push(pool.default_db.clone());
    }
    names.sort();
    Ok(names)
}

/// Fields of a collection, from a sample of its documents: each top-level key, the
/// types it was seen with (such as `string` or `int32|null`), and whether some document
/// lacked it or held `null`.
pub(super) async fn fields(
    pool: &MongoPool,
    database: Option<&str>,
    collection: &str,
) -> Result<Vec<ColumnInfo>> {
    let target = Target {
        database: database.map(str::to_owned),
        collection: collection.to_owned(),
    };
    let cursor = pool
        .collection(&target)
        .aggregate([doc! {"$sample": {"size": SAMPLE_SIZE}}])
        .await?;
    let docs: Vec<Document> = cursor.try_collect().await?;
    Ok(sampled_fields(&docs))
}

fn sampled_fields(docs: &[Document]) -> Vec<ColumnInfo> {
    let mut order: Vec<String> = Vec::new();
    let mut types: HashMap<String, (BTreeSet<&'static str>, usize)> = HashMap::new();
    for doc in docs {
        for (key, value) in doc {
            let entry = types.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                (BTreeSet::new(), 0)
            });
            entry.0.insert(type_name(value));
            entry.1 += 1;
        }
    }
    if let Some(at) = order.iter().position(|k| k == "_id") {
        let id = order.remove(at);
        order.insert(0, id);
    }
    order
        .into_iter()
        .map(|name| {
            let (kinds, count) = &types[&name];
            let nullable = *count < docs.len() || kinds.contains("null");
            let mut names: Vec<&str> = kinds.iter().copied().filter(|k| *k != "null").collect();
            if kinds.contains("null") {
                names.push("null");
            }
            ColumnInfo {
                name,
                data_type: names.join("|"),
                nullable,
            }
        })
        .collect()
}

fn type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null | Bson::Undefined => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binary",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

/// Indexes of every collection, each named with its collection in the detail. Best
/// effort: a collection whose indexes can't be read is left out.
pub(super) async fn list_objects(pool: &MongoPool) -> Vec<ObjectInfo> {
    let Ok(tables) = tables(pool).await else {
        return Vec::new();
    };
    let mut objects = Vec::new();
    for table in tables.into_iter().filter(|t| t.kind == "COLLECTION") {
        let target = Target {
            database: Some(table.schema.clone()),
            collection: table.name.clone(),
        };
        let Ok(cursor) = pool.collection(&target).list_indexes().await else {
            continue;
        };
        let Ok(models) = cursor.try_collect::<Vec<IndexModel>>().await else {
            continue;
        };
        for model in models {
            let name = model
                .options
                .and_then(|o| o.name)
                .unwrap_or_else(|| "?".to_owned());
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

/// The shell command that makes an index or a view, to read, not run.
pub(super) async fn object_definition(pool: &MongoPool, object: &ObjectInfo) -> Result<Source> {
    let db = pool.client.database(&object.schema);
    let quote = |s: &str| Value::from(s).to_string();
    let json = |d: &Document| value(&Bson::Document(d.clone())).to_string();
    match object.kind {
        ObjectKind::Index => {
            let Some(collection) = object.detail.strip_prefix("on ") else {
                return Ok(None);
            };
            let cursor = db.collection::<Document>(collection).list_indexes().await?;
            let models: Vec<IndexModel> = cursor.try_collect().await?;
            let Some(model) = models.into_iter().find(|m| {
                m.options.as_ref().and_then(|o| o.name.as_deref()) == Some(object.name.as_str())
            }) else {
                return Ok(None);
            };
            let options = model
                .options
                .as_ref()
                .map(mongodb::bson::serialize_to_document)
                .transpose()?
                .unwrap_or_default();
            let mut text = format!(
                "db.getSiblingDB({}).getCollection({}).createIndex({}",
                quote(&object.schema),
                quote(collection),
                json(&model.keys)
            );
            if !options.is_empty() {
                let _ = write!(text, ", {}", json(&options));
            }
            text.push(')');
            Ok(Some(Some(text)))
        }
        ObjectKind::View => {
            let cursor = db
                .list_collections()
                .filter(doc! {"name": &object.name})
                .await?;
            let specs: Vec<_> = cursor.try_collect().await?;
            let Some(spec) = specs.into_iter().next() else {
                return Ok(None);
            };
            let view_on = spec.options.view_on.unwrap_or_default();
            let pipeline = spec.options.pipeline.unwrap_or_default();
            let stages: Vec<String> = pipeline.iter().map(json).collect();
            Ok(Some(Some(format!(
                "db.getSiblingDB({}).createView({}, {}, [{}])",
                quote(&object.schema),
                quote(&object.name),
                quote(&view_on),
                stages.join(", ")
            ))))
        }
        _ => Err(super::unsupported(object.kind)),
    }
}

#[cfg(test)]
mod tests {
    use mongodb::bson::oid::ObjectId;
    use mongodb::bson::{Binary, DateTime, Decimal128, Timestamp, spec::BinarySubtype};
    use serde_json::json;

    use super::*;
    use crate::config::{MongoSettings, SshConfig};

    fn conn(adapter: Adapter, tls: TlsMode) -> Connection {
        Connection {
            name: "m".into(),
            folder: String::new(),
            adapter,
            host: "db.example.com".into(),
            port: 27017,
            database: "app".into(),
            user: "app".into(),
            readonly: false,
            color: String::new(),
            tls,
            ssh: None,
            auth: Default::default(),
            source: None,
            mongo: None,
        }
    }

    fn tunnel() -> SshConfig {
        SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "ec2-user".into(),
            key_path: String::new(),
            fingerprint: "SHA256:abc".into(),
        }
    }

    #[tokio::test]
    async fn the_password_is_set_in_code_never_in_the_connection_string() {
        let mut c = conn(Adapter::Mongodb, TlsMode::Prefer);
        c.mongo = Some(MongoSettings {
            srv: false,
            options: "authSource=shop&replicaSet=rs0".into(),
        });
        let uri = uri(&c, "db.example.com", 27017).unwrap();
        assert_eq!(
            uri,
            "mongodb://db.example.com:27017/?authSource=shop&replicaSet=rs0"
        );
        assert!(!uri.contains("secret"));
        let options = options(&c, "db.example.com", 27017, Some("secret"))
            .await
            .unwrap();
        let credential = options.credential.unwrap();
        assert_eq!(credential.username.as_deref(), Some("app"));
        assert_eq!(credential.password.as_deref(), Some("secret"));
        assert_eq!(credential.source.as_deref(), Some("shop"));
        assert_eq!(options.repl_set_name.as_deref(), Some("rs0"));
        assert_eq!(options.app_name.as_deref(), Some("TupleBase"));
        assert_eq!(options.max_pool_size, Some(POOL_SIZE));
        // No user: no credential, and the source alone is not one.
        c.user.clear();
        let options = super::options(&c, "db.example.com", 27017, None)
            .await
            .unwrap();
        assert!(options.credential.is_none());
    }

    #[tokio::test]
    async fn a_user_signs_in_against_admin_unless_told_otherwise() {
        let c = conn(Adapter::Mongodb, TlsMode::Prefer);
        let options = options(&c, "h", 27017, Some("p")).await.unwrap();
        assert_eq!(options.credential.unwrap().source.as_deref(), Some("admin"));
    }

    #[tokio::test]
    async fn encryption_follows_the_form_over_the_options() {
        let tls = |mode, options: &str| {
            let mut c = conn(Adapter::Mongodb, mode);
            c.mongo = Some(MongoSettings {
                srv: false,
                options: options.into(),
            });
            async move { self::options(&c, "h", 1, None).await.unwrap().tls }
        };
        assert!(matches!(
            tls(TlsMode::Prefer, "").await,
            Some(Tls::Disabled)
        ));
        // If available: as the options say.
        assert!(matches!(
            tls(TlsMode::Prefer, "tls=true").await,
            Some(Tls::Enabled(_))
        ));
        assert!(matches!(
            tls(TlsMode::Require, "tls=false").await,
            Some(Tls::Enabled(_))
        ));
        let Some(Tls::Enabled(require)) = tls(TlsMode::Require, "").await else {
            panic!()
        };
        assert_eq!(require.allow_invalid_certificates, Some(true));
        let Some(Tls::Enabled(verify)) =
            tls(TlsMode::Verify, "tlsAllowInvalidCertificates=true").await
        else {
            panic!()
        };
        assert_eq!(verify.allow_invalid_certificates, None);
    }

    #[tokio::test]
    async fn documentdb_never_retries_writes_and_a_tunnel_connects_directly() {
        let c = conn(Adapter::Documentdb, TlsMode::Verify);
        let options = options(&c, "h", 27017, None).await.unwrap();
        assert_eq!(options.retry_writes, Some(false));
        assert_eq!(options.direct_connection, None);
        let mongo = conn(Adapter::Mongodb, TlsMode::Prefer);
        let options = self::options(&mongo, "h", 27017, None).await.unwrap();
        assert_eq!(options.retry_writes, None);
        let mut tunneled = conn(Adapter::Documentdb, TlsMode::Require);
        tunneled.ssh = Some(tunnel());
        tunneled.mongo = Some(MongoSettings {
            srv: false,
            options: "replicaSet=rs0&readPreference=secondaryPreferred".into(),
        });
        let options = self::options(&tunneled, "127.0.0.1", 40000, None)
            .await
            .unwrap();
        assert_eq!(options.direct_connection, Some(true));
        assert_eq!(options.hosts[0].to_string(), "127.0.0.1:40000");
    }

    #[test]
    fn srv_drops_the_port_and_hosts_are_plain() {
        let mut c = conn(Adapter::Mongodb, TlsMode::Verify);
        c.mongo = Some(MongoSettings {
            srv: true,
            options: String::new(),
        });
        assert_eq!(
            uri(&c, "cluster0.example.net", 27017).unwrap(),
            "mongodb+srv://cluster0.example.net/?"
        );
        assert_eq!(
            uri(&conn(Adapter::Mongodb, TlsMode::Prefer), "::1", 27017).unwrap(),
            "mongodb://[::1]:27017/?"
        );
        for host in [
            "user:pw@h",
            "h/admin",
            "h?tls=false",
            "a,b",
            "h #x",
            "",
            "h%40",
        ] {
            assert!(
                uri(&conn(Adapter::Mongodb, TlsMode::Prefer), host, 1).is_err(),
                "{host}"
            );
        }
        let mut secret = conn(Adapter::Mongodb, TlsMode::Prefer);
        secret.mongo = Some(MongoSettings {
            srv: false,
            options: "password=x".into(),
        });
        assert!(uri(&secret, "h", 1).is_err());
    }

    fn privileges(actions: &[&[&str]]) -> Document {
        let privileges: Vec<Bson> = actions
            .iter()
            .map(|a| {
                Bson::Document(
                    doc! {"resource": {"db": "app", "collection": ""}, "actions": a.to_vec()},
                )
            })
            .collect();
        doc! {"authInfo": {"authenticatedUsers": [], "authenticatedUserPrivileges": privileges}, "ok": 1}
    }

    #[test]
    fn server_read_only_fails_closed() {
        let primary = doc! {"isWritablePrimary": true, "secondary": false};
        let secondary = doc! {"isWritablePrimary": false, "secondary": true};
        let reader = privileges(&[&["find", "listCollections"], &["collStats", "dbStats"]]);
        let writer = privileges(&[&["find", "insert"]]);
        let unknown = privileges(&[&["find", "someNewAction"]]);
        assert_eq!(
            server_read_only(Some(&primary), Some(&reader), false),
            Some(true)
        );
        assert_eq!(
            server_read_only(Some(&primary), Some(&writer), false),
            Some(false)
        );
        assert_eq!(
            server_read_only(Some(&primary), Some(&unknown), false),
            Some(false)
        );
        // A secondary counts only with a direct connection.
        assert_eq!(
            server_read_only(Some(&secondary), Some(&writer), true),
            Some(true)
        );
        assert_eq!(
            server_read_only(Some(&secondary), Some(&writer), false),
            Some(false)
        );
        assert_eq!(server_read_only(Some(&secondary), None, false), None);
        // No answer, no privileges, or an odd one: unknown, never read-only.
        assert_eq!(server_read_only(None, None, false), None);
        assert_eq!(server_read_only(Some(&primary), None, false), None);
        assert_eq!(
            server_read_only(Some(&primary), Some(&privileges(&[])), false),
            None
        );
        assert_eq!(server_read_only(None, Some(&doc! {"ok": 1}), false), None);
        let odd = doc! {"authInfo": {"authenticatedUserPrivileges": [{"actions": [1]}]}};
        assert_eq!(server_read_only(None, Some(&odd), false), None);
        let no_actions = doc! {"authInfo": {"authenticatedUserPrivileges": [{"resource": {}}]}};
        assert_eq!(server_read_only(None, Some(&no_actions), false), None);
    }

    #[test]
    fn bson_becomes_relaxed_extended_json() {
        let id = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let document = doc! {
            "_id": id,
            "n": 1,
            "big": 9_007_199_254_740_993_i64,
            "f": 1.5,
            "nan": f64::NAN,
            "s": "x",
            "at": DateTime::from_millis(1_704_164_645_000),
            "old": DateTime::from_millis(-1),
            "d": Decimal128::from_str("1.10").unwrap(),
            "bin": Binary { subtype: BinarySubtype::Generic, bytes: b"hello".to_vec() },
            "t": Timestamp { time: 5, increment: 1 },
            "nested": {"a": [1, {"b": null}]},
            "max": Bson::MaxKey,
        };
        assert_eq!(
            value(&Bson::Document(document)),
            json!({
                "_id": {"$oid": "65a1b2c3d4e5f60718293a4b"},
                "n": 1,
                "big": 9_007_199_254_740_993_i64,
                "f": 1.5,
                "nan": {"$numberDouble": "NaN"},
                "s": "x",
                "at": {"$date": "2024-01-02T03:04:05Z"},
                "old": {"$date": {"$numberLong": "-1"}},
                "d": {"$numberDecimal": "1.10"},
                "bin": {"$binary": {"base64": "aGVsbG8=", "subType": "00"}},
                "t": {"$timestamp": {"t": 5, "i": 1}},
                "nested": {"a": [1, {"b": null}]},
                "max": {"$maxKey": 1},
            })
        );
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    use std::str::FromStr;

    #[test]
    fn documents_flatten_into_columns_with_id_first() {
        let docs = [
            json!({"name": "Ada", "_id": 1, "tags": ["a"]}),
            json!({"_id": 2, "email": "e", "name": "Edgar"}),
        ];
        let (columns, rows) = flatten(&docs);
        assert_eq!(columns, ["_id", "name", "tags", "email"]);
        assert_eq!(
            rows,
            [
                vec![json!(1), json!("Ada"), json!(["a"]), Value::Null],
                vec![json!(2), json!("Edgar"), Value::Null, json!("e")],
            ]
        );
        let wide: Map<String, Value> = (0..300).map(|i| (format!("k{i}"), json!(i))).collect();
        let (columns, rows) = flatten(&[Value::Object(wide)]);
        assert_eq!(columns.len(), MAX_COLUMNS);
        assert_eq!(rows[0].len(), MAX_COLUMNS);
        let result = documents_result(&[doc! {"a": 1}], false, Some(1));
        assert_eq!(result.documents, Some(vec![json!({"a": 1})]));
        assert_eq!(result.rows_affected, Some(1));
    }

    #[test]
    fn sampled_fields_name_their_types_and_absences() {
        let docs = [
            doc! {"name": "a", "_id": 1, "email": "x"},
            doc! {"_id": 2, "name": "b", "email": Bson::Null, "age": 3},
            doc! {"_id": 3, "name": 4_i64, "email": "y"},
        ];
        let fields: Vec<(String, String, bool)> = sampled_fields(&docs)
            .into_iter()
            .map(|c| (c.name, c.data_type, c.nullable))
            .collect();
        assert_eq!(
            fields,
            [
                ("_id".into(), "int32".into(), false),
                ("name".into(), "int64|string".into(), false),
                ("email".into(), "string|null".into(), true),
                ("age".into(), "int32".into(), true),
            ]
        );
    }

    #[test]
    fn read_only_time_is_capped() {
        let minute = Some(READ_ONLY_MAX_TIME);
        let second = Some(Duration::from_secs(1));
        let hour = Some(Duration::from_secs(3600));
        assert_eq!(shorter(None, minute), minute);
        assert_eq!(shorter(second, minute), second);
        assert_eq!(shorter(hour, minute), minute);
        assert_eq!(
            shorter(hour, None),
            hour,
            "a writable connection keeps its own"
        );
        assert_eq!(shorter(None, None), None);
    }

    #[test]
    fn explain_plans_without_running() {
        let target = Target {
            database: Some("shop".into()),
            collection: "orders".into(),
        };
        let (database, command) = explain_command(ReadOp::Aggregate {
            target,
            pipeline: vec![doc! {"$match": {}}],
            options: AggregateOptions::default(),
        })
        .unwrap();
        assert_eq!(database.as_deref(), Some("shop"));
        assert_eq!(command.get_str("verbosity").unwrap(), "queryPlanner");
        assert_eq!(command.keys().next().map(String::as_str), Some("explain"));
    }
}
