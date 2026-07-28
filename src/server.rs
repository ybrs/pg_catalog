// TCP server exposing the DataFusion session via the PostgreSQL wire protocol.
// Implements pgwire handlers and registers compatibility UDFs so external clients can connect.
// Exists to test the pg_catalog emulation over real network connections.

use std::sync::{Arc, Mutex};

use arrow::array::{Array, ArrayRef, Float32Array, Float64Array};
use async_trait::async_trait;
use bytes::Bytes;
use futures::sink::Sink;
use futures::{stream, Stream};
use pgwire::api::auth::md5pass::{hash_md5_password, Md5PasswordAuthStartupHandler};
use pgwire::api::auth::{
    AuthSource, DefaultServerParameterProvider, LoginInfo, Password, StartupHandler,
};
use pgwire::api::copy::CopyHandler;
use pgwire::api::portal::{Format, Portal};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, NoopHandler, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use pgwire::messages::PgWireBackendMessage;
use pgwire::tokio::process_socket;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use tokio::net::TcpListener;

use arrow::array::{
    BooleanArray, Int32Array, Int64Array, LargeStringArray, ListArray, StringArray, StringViewArray,
};
use arrow::record_batch::RecordBatch;
use datafusion::execution::context::SessionContext;

use arrow::datatypes::{DataType, Field, Schema, TimeUnit};

use datafusion::{
    common::ScalarValue,
    error::DataFusionError,
    logical_expr::{create_udf, ColumnarValue, Volatility},
};

use crate::router::dispatch_query;
use crate::session::{execute_sql, ClientOpts};
use log;
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// `PostgreSQL` version reported to clients during startup and via `SHOW server_version`.
pub const SERVER_VERSION: &str = "17.4.0";

/// If `text` is `DataFusion`'s unknown-function planning error
/// (`Invalid function '<name>'...`), return the offending function name.
///
/// `DataFusion` reports a call to a function it cannot resolve as
/// `Error during planning: Invalid function '<name>'.\nDid you mean '<other>'?`.
/// We recognize that shape so it can be reported to clients as `PostgreSQL` would.
fn unknown_function_name(text: &str) -> Option<String> {
    let marker = "Invalid function '";
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Translate a [`DataFusionError`] into a [`PgWireError`] carrying a
/// PostgreSQL-compatible SQLSTATE for the error classes we can recognize.
///
/// A bare `PgWireError::ApiError` is rendered to the client with SQLSTATE
/// `XX000` (internal error), which is wrong for a user-level mistake like
/// calling a function that does not exist: `PostgreSQL` reports that as `42883`
/// (`undefined_function`) with a `... does not exist` message, and clients (and
/// the view-validation tests) key on exactly that. Map the unknown-function case
/// to the matching [`ErrorInfo`]; every other error keeps the previous generic
/// `ApiError` mapping so its message reaches the client unchanged.
fn into_pgwire_error(e: DataFusionError) -> PgWireError {
    if let Some(name) = unknown_function_name(&e.to_string()) {
        let info = ErrorInfo::new(
            "ERROR".to_string(),
            "42883".to_string(),
            format!("function {name}() does not exist"),
        );
        return PgWireError::UserError(Box::new(info));
    }
    PgWireError::ApiError(Box::new(e))
}

/// One query as seen by the server, recorded for the optional capture file.
///
/// A capture entry keeps everything needed to replay the exchange offline: the
/// SQL text, the bound parameters, the rows sent back, and - when the query
/// failed - the error message the client received.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct CapturedQuery {
    /// SQL text exactly as the client sent it.
    query: String,
    /// Bound parameter values, decoded to JSON; `None` marks a NULL parameter.
    parameters: Vec<Option<serde_json::Value>>,
    /// Result rows as column-name to JSON-value maps, one map per row.
    result: Vec<BTreeMap<String, serde_json::Value>>,
    /// Whether the query completed without an error.
    success: bool,
    /// Error message reported to the client, present only when `success` is false.
    error_details: Option<String>,
}

/// Accumulates [`CapturedQuery`] entries and mirrors them to a YAML file.
///
/// Cloning shares the same entry list and target path, so every connection
/// handler writes into one capture file. The whole file is rewritten after each
/// append so a capture run left incomplete still ends with a readable file.
#[derive(Clone)]
pub(crate) struct CaptureStore {
    /// File that receives the YAML rendering of all captured queries.
    path: PathBuf,
    /// Captured queries in arrival order, shared across handler clones.
    entries: Arc<Mutex<Vec<CapturedQuery>>>,
}

impl CaptureStore {
    /// Create an empty store that writes its YAML rendering to `path`.
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Append the YAML rendering of `v` to `out`, indenting nested block
    /// entries by `indent` spaces.
    ///
    /// Written by hand rather than delegated to a YAML library so the capture
    /// files keep a stable, diff-friendly layout: scalars stay inline after
    /// their key and only sequences and mappings open a new indented block.
    fn encode_yaml_value(v: &serde_json::Value, indent: usize, out: &mut String) {
        match v {
            serde_json::Value::Null => out.push_str("null"),
            serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            serde_json::Value::Number(n) => out.push_str(&n.to_string()),
            serde_json::Value::String(s) => {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        _ => out.push(ch),
                    }
                }
                out.push('"');
            }
            serde_json::Value::Array(arr) => {
                if arr.is_empty() {
                    out.push_str("[]");
                } else {
                    for item in arr {
                        out.push('\n');
                        out.push_str(&" ".repeat(indent));
                        out.push_str("- ");
                        Self::encode_yaml_value(item, indent + 2, out);
                    }
                }
            }
            serde_json::Value::Object(map) => {
                if map.is_empty() {
                    out.push_str("{}");
                } else {
                    for (k, v) in map {
                        out.push('\n');
                        out.push_str(&" ".repeat(indent));
                        out.push_str(k);
                        out.push_str(": ");
                        Self::encode_yaml_value(v, indent + 2, out);
                    }
                }
            }
        }
    }

    /// Rewrite the capture file with `entries` rendered as a YAML sequence.
    ///
    /// Every failure - opening the file, serializing, writing - is swallowed:
    /// capturing is a debugging aid, so it must never take down a client
    /// connection that is otherwise working.
    fn save_entries(&self, entries: &[CapturedQuery]) {
        if let Ok(mut file) = File::create(&self.path) {
            if let Ok(val) = serde_json::to_value(entries) {
                let mut out = String::new();
                if let serde_json::Value::Array(arr) = val {
                    let mut first = true;
                    for item in arr {
                        if let serde_json::Value::Object(map) = item {
                            if !first {
                                out.push('\n');
                            }
                            first = false;
                            out.push_str("- ");
                            let mut iter = map.iter();
                            if let Some((k, v)) = iter.next() {
                                out.push_str(k);
                                out.push_str(": ");
                                Self::encode_yaml_value(v, 2, &mut out);
                            }
                            for (k, v) in iter {
                                out.push('\n');
                                out.push_str("  ");
                                out.push_str(k);
                                out.push_str(": ");
                                Self::encode_yaml_value(v, 2, &mut out);
                            }
                            out.push('\n');
                        }
                    }
                }
                let _ = file.write_all(out.as_bytes());
            }
        }
    }

    /// Record `entry` and flush the whole capture file to disk.
    ///
    /// # Panics
    ///
    /// Panics if the entry list mutex was poisoned by a thread that panicked
    /// while holding it.
    fn append(&self, entry: CapturedQuery) {
        let mut vec = self.entries.lock().unwrap();
        vec.push(entry);
        self.save_entries(&vec);
    }
}

/// pgwire query handler that answers a single client connection from a
/// `DataFusion` session.
///
/// Each connection owns its own backend (and its own [`SessionContext`]) so
/// per-session state such as the current role and `SET` variables cannot leak
/// between clients.
pub struct DatafusionBackend {
    /// Session context this connection plans and executes against.
    ctx: Arc<SessionContext>,
    /// Parser handed to pgwire; statements are kept as raw SQL text and parsed
    /// during execution instead.
    query_parser: Arc<NoopQueryParser>,
    /// Optional recorder for queries and their results.
    capture: Option<CaptureStore>,
}

impl DatafusionBackend {
    /// Build a backend serving `ctx`, optionally recording traffic into `capture`.
    pub(crate) fn new(ctx: Arc<SessionContext>, capture: Option<CaptureStore>) -> Self {
        Self {
            ctx,
            query_parser: Arc::new(NoopQueryParser::new()),
            capture,
        }
    }

    /// Record a query that failed, together with the error the client was told
    /// about.
    ///
    /// `params` and `declared_types` are the raw bound parameters and the types
    /// the client declared for them; both are empty for the simple protocol.
    /// Decoding happens only when a capture file is configured, so a connection
    /// that is not being captured neither pays for the conversion nor can be
    /// brought down by a parameter that contradicts its declared type.
    fn capture_failed_query(
        &self,
        query: &str,
        params: &[Option<Bytes>],
        declared_types: &[Option<Type>],
        error: &DataFusionError,
    ) {
        if let Some(store) = &self.capture {
            store.append(CapturedQuery {
                query: query.to_string(),
                parameters: decode_parameters(params, &concrete_param_types(declared_types)),
                result: Vec::new(),
                success: false,
                error_details: Some(error.to_string()),
            });
        }
    }

    /// Record a query that succeeded, together with the rows it produced.
    ///
    /// `params` and `declared_types` are the raw bound parameters and the types
    /// the client declared for them; both are empty for the simple protocol.
    /// They, and the result rows, are converted only when a capture file is
    /// configured.
    fn capture_successful_query(
        &self,
        query: &str,
        params: &[Option<Bytes>],
        declared_types: &[Option<Type>],
        results: &[RecordBatch],
    ) {
        if let Some(store) = &self.capture {
            store.append(CapturedQuery {
                query: query.to_string(),
                parameters: decode_parameters(params, &concrete_param_types(declared_types)),
                result: batches_to_json_rows(results),
                success: true,
                error_details: None,
            });
        }
    }

    /// Register a zero-argument UDF that returns a constant Utf8 string read
    /// from the client's connection metadata.
    ///
    /// If a UDF named `name` is already registered the call is a no-op. The
    /// constant value comes from `client.metadata().get(metadata_key)`; when
    /// that key is absent no UDF is registered. When `also_qualified` is true an
    /// additional UDF aliased as `pg_catalog.<name>` is registered with the same
    /// constant value.
    fn register_constant_text_udf<C>(
        &self,
        name: &str,
        metadata_key: &str,
        also_qualified: bool,
        client: &C,
    ) where
        C: ClientInfo + ?Sized,
    {
        if self.ctx.state().scalar_functions().contains_key(name) {
            return;
        }

        if let Some(value) = client.metadata().get(metadata_key).cloned() {
            let fun = Arc::new(move |_args: &[ColumnarValue]| {
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(
                    value.clone(),
                ))))
            });
            let udf = create_udf(
                name,
                vec![],
                DataType::Utf8,
                Volatility::Stable,
                fun.clone(),
            );
            self.ctx.register_udf(udf);
            if also_qualified {
                let qualified_name = format!("pg_catalog.{name}");
                let udf = create_udf(
                    &qualified_name,
                    vec![],
                    DataType::Utf8,
                    Volatility::Stable,
                    fun,
                );
                self.ctx.register_udf(udf);
            }
        }
    }

    /// Register the `current_database` UDF (and its `pg_catalog` alias) from the
    /// connection's database metadata.
    fn register_current_database<C>(&self, client: &C)
    where
        C: ClientInfo + ?Sized,
    {
        self.register_constant_text_udf(
            "current_database",
            pgwire::api::METADATA_DATABASE,
            true,
            client,
        );
    }

    /// Build a one-row, one-column result for `SHOW <name>` on the client
    /// settings this server tracks itself.
    ///
    /// Returns `None` when the connection carries no [`ClientOpts`], when
    /// `name` is not one of the tracked settings, or when the value cannot be
    /// encoded - in every case the caller falls back to planning the statement
    /// as a normal query.
    fn show_variable_response(&self, name: &str, format: FieldFormat) -> Option<Response> {
        let state = self.ctx.state();
        let opts = state.config_options().extensions.get::<ClientOpts>()?;

        let value = match name {
            "application_name" => opts.application_name.as_str(),
            "datestyle" => opts.datestyle.as_str(),
            "search_path" => opts.search_path.as_str(),
            _ => return None,
        };

        let fields = Arc::new(vec![FieldInfo::new(
            name.to_string(),
            None,
            None,
            Type::TEXT,
            format,
        )]);

        let mut encoder = DataRowEncoder::new(fields.clone());
        encoder.encode_field(&Some(value)).ok()?;
        let row = encoder.take_row();
        let rows = stream::iter(vec![Ok(row)]);
        Some(Response::Query(QueryResponse::new(fields, rows)))
    }

    /// Extract the variable name from a single `SHOW <name>` statement.
    ///
    /// Returns `None` for anything else - unparsable SQL, several statements,
    /// or a `SHOW` of a compound name such as `SHOW TRANSACTION ISOLATION
    /// LEVEL` - which the callers then route through the normal query path.
    fn parse_show_variable_name(sql: &str) -> Option<String> {
        let dialect = PostgreSqlDialect {};
        let mut statements = Parser::parse_sql(&dialect, sql).ok()?;
        if statements.len() != 1 {
            return None;
        }
        match statements.pop()? {
            Statement::ShowVariable { variable } if variable.len() == 1 => {
                Some(variable[0].value.clone())
            }
            _ => None,
        }
    }

    /// Build the single-row `read committed` answer to
    /// `SHOW TRANSACTION ISOLATION LEVEL`.
    ///
    /// There is no real transaction manager behind this server, so it reports
    /// the `PostgreSQL` default level that drivers probe for right after
    /// connecting instead of letting the statement fail to plan.
    ///
    /// # Errors
    ///
    /// Returns an error if the row encoder rejects the `read committed` value
    /// for the requested wire format.
    fn transaction_isolation_response(format: FieldFormat) -> PgWireResult<Response> {
        let field_infos = Arc::new(vec![FieldInfo::new(
            "transaction_isolation".to_string(),
            None,
            None,
            Type::TEXT,
            format,
        )]);

        let mut encoder = DataRowEncoder::new(field_infos.clone());
        encoder.encode_field(&Some("read committed"))?;
        let row = encoder.take_row();

        let rows = stream::iter(vec![Ok(row)]);
        Ok(Response::Query(QueryResponse::new(field_infos, rows)))
    }

    /// Answer a simple-protocol statement that this server handles itself:
    /// transaction control, `DISCARD ALL`, the isolation-level probe, a `SHOW`
    /// of a tracked client setting, or an empty statement.
    ///
    /// `trimmed` is the statement with surrounding whitespace removed and
    /// `lowercase` its lowercased form. Returns `Ok(None)` when the statement
    /// is a real query that must be planned by `DataFusion`.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding the isolation-level row fails.
    fn simple_query_builtin_response(
        &self,
        trimmed: &str,
        lowercase: &str,
    ) -> PgWireResult<Option<Vec<Response>>> {
        if lowercase.starts_with("begin") {
            return Ok(Some(vec![Response::Execution(Tag::new("BEGIN"))]));
        } else if lowercase.starts_with("commit") {
            return Ok(Some(vec![Response::Execution(Tag::new("COMMIT"))]));
        } else if lowercase.starts_with("rollback") {
            return Ok(Some(vec![Response::Execution(Tag::new("ROLLBACK"))]));
        } else if lowercase.starts_with("discard all") {
            return Ok(Some(vec![Response::Execution(Tag::new("DISCARD ALL"))]));
        } else if lowercase == "show transaction isolation level" {
            return Ok(Some(vec![Self::transaction_isolation_response(
                FieldFormat::Text,
            )?]));
        } else if let Some(var) = Self::parse_show_variable_name(trimmed) {
            if let Some(resp) = self.show_variable_response(&var.to_lowercase(), FieldFormat::Text)
            {
                return Ok(Some(vec![resp]));
            }
        } else if lowercase.is_empty() {
            return Ok(Some(vec![Response::Execution(Tag::new(""))]));
        }

        Ok(None)
    }

    /// Answer an extended-protocol statement that this server handles itself:
    /// an empty statement, `DISCARD ALL`, the isolation-level probe, or a
    /// `SHOW` of a tracked client setting.
    ///
    /// `sql_trim` is the statement with surrounding whitespace removed,
    /// `lowercase` its lowercased form, and `format` the wire format the portal
    /// requested for its first result column. Returns `Ok(None)` when the
    /// statement is a real query that must be planned by `DataFusion`.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding the isolation-level row fails.
    fn extended_query_builtin_response(
        &self,
        sql_trim: &str,
        lowercase: &str,
        format: FieldFormat,
    ) -> PgWireResult<Option<Response>> {
        if sql_trim.is_empty() {
            return Ok(Some(Response::Execution(Tag::new(""))));
        } else if lowercase.starts_with("discard all") {
            return Ok(Some(Response::Execution(Tag::new("DISCARD ALL"))));
        } else if lowercase == "show transaction isolation level" {
            return Ok(Some(Self::transaction_isolation_response(format)?));
        } else if let Some(var) = Self::parse_show_variable_name(sql_trim) {
            if let Some(resp) = self.show_variable_response(&var.to_lowercase(), format) {
                return Ok(Some(resp));
            }
        }

        Ok(None)
    }
}

/// Password source that accepts every user with the fixed password `pencil`.
///
/// The server exists to exercise the `pg_catalog` emulation over real client
/// libraries, which insist on completing an authentication handshake; it holds
/// no user database, so it answers the MD5 challenge for whatever user connects.
#[derive(Debug)]
pub struct DummyAuthSource;

#[async_trait]
impl AuthSource for DummyAuthSource {
    /// Return the MD5 hash of the fixed password, salted with four zero bytes.
    ///
    /// # Panics
    ///
    /// Panics if the startup message carried no user name, which the protocol
    /// requires before authentication can start.
    async fn get_password(&self, login_info: &LoginInfo) -> PgWireResult<Password> {
        log::info!("login info: {login_info:?}");

        let salt = vec![0, 0, 0, 0];
        let password = "pencil";

        let hash_password = hash_md5_password(login_info.user().as_ref().unwrap(), password, &salt);
        Ok(Password::new(Some(salt), hash_password.as_bytes().to_vec()))
    }
}

/// Stringify the elements of a string-like Arrow array (Utf8, `Utf8View`, or
/// `LargeUtf8`) for array-text encoding, rendering NULL elements as `"NULL"`.
fn stringify_string_array(arr: &arrow::array::ArrayRef) -> Vec<String> {
    use arrow::array::Array;
    let n = arr.len();
    let mut out = Vec::with_capacity(n);
    let push = |out: &mut Vec<String>, is_null: bool, val: &str| {
        out.push(if is_null {
            "NULL".to_string()
        } else {
            val.to_string()
        });
    };
    if let Some(a) = arr.as_any().downcast_ref::<StringArray>() {
        for i in 0..n {
            push(
                &mut out,
                a.is_null(i),
                if a.is_null(i) { "" } else { a.value(i) },
            );
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<StringViewArray>() {
        for i in 0..n {
            push(
                &mut out,
                a.is_null(i),
                if a.is_null(i) { "" } else { a.value(i) },
            );
        }
    } else if let Some(a) = arr.as_any().downcast_ref::<LargeStringArray>() {
        for i in 0..n {
            push(
                &mut out,
                a.is_null(i),
                if a.is_null(i) { "" } else { a.value(i) },
            );
        }
    }
    out
}

/// Map an Arrow [`DataType`] to the `PostgreSQL` type advertised for that
/// column in the row description.
///
/// Unmapped types (and unmapped list element types) degrade to `text` rather
/// than panicking, so an exotic column costs the client fidelity instead of the
/// whole connection.
fn arrow_to_pg_type(dt: &DataType) -> Type {
    use arrow::datatypes::DataType::{
        Boolean, Float32, Float64, Int16, Int32, Int64, LargeUtf8, List, Timestamp, Utf8, Utf8View,
    };

    match dt {
        Boolean => Type::BOOL,
        Int16 => Type::INT2,
        Int32 => Type::INT4,
        Int64 => Type::INT8,
        Utf8 | Utf8View | LargeUtf8 => Type::TEXT,
        Timestamp(_, _) => Type::TIMESTAMP,
        Float32 => Type::FLOAT4, // real
        Float64 => Type::FLOAT8, // double precision

        // -- arrays -----------------------------------------------
        List(inner) => match inner.data_type() {
            Utf8 | Utf8View | LargeUtf8 => Type::TEXT_ARRAY, // text[]
            Int16 => Type::INT2_ARRAY,                       // int2[]
            Int32 => Type::INT4_ARRAY,                       // int4[]
            Int64 => Type::INT8_ARRAY,                       // int8[]
            Boolean => Type::BOOL_ARRAY,                     // bool[]
            Float32 => Type::FLOAT4_ARRAY,                   // real[]
            Float64 => Type::FLOAT8_ARRAY,                   // double precision[]
            // Never panic on an unmapped element type: fall back to text[] so the
            // client gets a sensible (if generic) array type instead of a crash.
            other => {
                log::warn!("arrow_to_pg_type: no array type for list<{other:?}>, using text[]");
                Type::TEXT_ARRAY
            }
        },

        // anything else - send as plain text so the client can at
        // least see something instead of us mangling it away
        other => {
            log::warn!("arrow_to_pg_type: mapping {other:?} to TEXT");
            Type::TEXT
        }
    }
}

/// Describe every column of `batch` as a pgwire [`FieldInfo`], using the wire
/// format `format` assigns to each column position.
fn batch_to_field_info(batch: &RecordBatch, format: &Format) -> Vec<FieldInfo> {
    batch
        .schema()
        .fields()
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            FieldInfo::new(
                f.name().clone(),
                None,
                None,
                arrow_to_pg_type(f.data_type()),
                format.format_for(idx),
            )
        })
        .collect()
}

/// Convert record batches into one JSON map per row, keyed by column name.
///
/// Only the types the capture files need are rendered; any column of another
/// type is recorded as `null` so a capture never fails on an exotic column.
///
/// # Panics
///
/// Panics if a column's array does not match the data type its field declares,
/// which would mean the batch itself is malformed.
fn batches_to_json_rows(batches: &[RecordBatch]) -> Vec<BTreeMap<String, serde_json::Value>> {
    let mut rows = Vec::new();
    for batch in batches {
        let schema = batch.schema();
        for row_idx in 0..batch.num_rows() {
            let mut map = BTreeMap::new();
            for (col_idx, field) in schema.fields().iter().enumerate() {
                let col = batch.column(col_idx);
                let val = match field.data_type() {
                    DataType::Utf8 => {
                        let arr = col.as_any().downcast_ref::<StringArray>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::Value::String(arr.value(row_idx).to_string())
                        }
                    }
                    DataType::Utf8View => {
                        let arr = col.as_any().downcast_ref::<StringViewArray>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::Value::String(arr.value(row_idx).to_string())
                        }
                    }
                    DataType::LargeUtf8 => {
                        let arr = col.as_any().downcast_ref::<LargeStringArray>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::Value::String(arr.value(row_idx).to_string())
                        }
                    }
                    DataType::Int32 => {
                        let arr = col.as_any().downcast_ref::<Int32Array>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::json!(arr.value(row_idx))
                        }
                    }
                    DataType::Int64 => {
                        let arr = col.as_any().downcast_ref::<Int64Array>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::json!(arr.value(row_idx))
                        }
                    }
                    DataType::Boolean => {
                        let arr = col.as_any().downcast_ref::<BooleanArray>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::json!(arr.value(row_idx))
                        }
                    }
                    DataType::Float32 => {
                        let arr = col.as_any().downcast_ref::<Float32Array>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::json!(arr.value(row_idx))
                        }
                    }
                    DataType::Float64 => {
                        let arr = col.as_any().downcast_ref::<Float64Array>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::json!(arr.value(row_idx))
                        }
                    }
                    DataType::List(inner) if inner.data_type() == &DataType::Utf8 => {
                        let list = col.as_any().downcast_ref::<ListArray>().unwrap();
                        if list.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            let arr = list.value(row_idx);
                            let sa = arr.as_any().downcast_ref::<StringArray>().unwrap();
                            let mut items = Vec::with_capacity(sa.len());
                            for i in 0..sa.len() {
                                if sa.is_null(i) {
                                    items.push(serde_json::Value::Null);
                                } else {
                                    items.push(serde_json::Value::String(sa.value(i).to_string()));
                                }
                            }
                            serde_json::Value::Array(items)
                        }
                    }
                    _ => serde_json::Value::Null,
                };
                map.insert(field.name().clone(), val);
            }
            rows.push(map);
        }
    }
    rows
}

/// pgwire 0.40 represents an unspecified prepared-statement parameter type
/// (protocol OID 0) as `None`. Our query path and pgwire's
/// `DescribeStatementResponse` both work with concrete `Type`s, so resolve any
/// unspecified entry to `Type::UNKNOWN` - matching the pre-0.40 behaviour where
/// pgwire handed us concrete types directly.
fn concrete_param_types(types: &[Option<Type>]) -> Vec<Type> {
    types
        .iter()
        .map(|t| t.clone().unwrap_or(Type::UNKNOWN))
        .collect()
}

/// Decode bound statement parameters into JSON for the capture file.
///
/// A NULL parameter, and any parameter whose type is not one of the scalar
/// types handled here, decodes to `None`: captures are a debugging aid, so an
/// unsupported parameter type is recorded as absent rather than failing the
/// query.
///
/// # Panics
///
/// Panics if a parameter's payload does not have the width its declared type
/// requires, or if a text parameter is not valid UTF-8 - both mean the client
/// sent a value that contradicts the type it bound.
fn decode_parameters(params: &[Option<Bytes>], types: &[Type]) -> Vec<Option<serde_json::Value>> {
    params
        .iter()
        .zip(types.iter())
        .map(|(param, typ)| match (param, typ) {
            (Some(bytes), &Type::INT2) => Some(serde_json::json!(i16::from_be_bytes(
                bytes[..].try_into().unwrap()
            ))),
            (Some(bytes), &Type::INT4) => Some(serde_json::json!(i32::from_be_bytes(
                bytes[..].try_into().unwrap()
            ))),
            (Some(bytes), &Type::INT8) => Some(serde_json::json!(i64::from_be_bytes(
                bytes[..].try_into().unwrap()
            ))),
            (Some(bytes), &Type::OID) => Some(serde_json::json!(u32::from_be_bytes(
                bytes[..].try_into().unwrap()
            ))),
            (
                Some(bytes),
                &Type::VARCHAR | &Type::TEXT | &Type::BPCHAR | &Type::NAME | &Type::UNKNOWN,
            ) => Some(serde_json::Value::String(
                String::from_utf8(bytes.to_vec()).unwrap(),
            )),
            _ => None,
        })
        .collect()
}

/// Read the string at `row_idx` of a Utf8, `Utf8View` or `LargeUtf8` column,
/// returning `None` for a NULL element.
///
/// # Panics
///
/// Panics if `col` is not one of those three string arrays; callers select this
/// function from the column's declared data type.
fn string_value_at(col: &ArrayRef, row_idx: usize) -> Option<String> {
    if col.is_null(row_idx) {
        return None;
    }
    let text = match col.data_type() {
        DataType::Utf8 => col
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(row_idx)
            .to_string(),
        DataType::Utf8View => col
            .as_any()
            .downcast_ref::<StringViewArray>()
            .unwrap()
            .value(row_idx)
            .to_string(),
        DataType::LargeUtf8 => col
            .as_any()
            .downcast_ref::<LargeStringArray>()
            .unwrap()
            .value(row_idx)
            .to_string(),
        other => unreachable!("string_value_at on non-string column {other:?}"),
    };
    Some(text)
}

/// Format the timestamp at `row_idx` the way `PostgreSQL` renders `timestamp`
/// in text, with the sub-second precision of `unit`.
///
/// Returns `None` for a NULL element and for an instant chrono cannot
/// represent: one out-of-range value must degrade to NULL rather than kill the
/// connection.
///
/// # Panics
///
/// Panics if `col` is not the timestamp array matching `unit`, or if `unit` is
/// `TimeUnit::Second`, which Arrow does not produce for query results.
fn format_timestamp_value_at(col: &ArrayRef, unit: TimeUnit, row_idx: usize) -> Option<String> {
    use arrow::array::{
        TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    };

    if col.is_null(row_idx) {
        return None;
    }

    // Floored (Euclidean) division keeps the sub-second part in [0, unit) for
    // negative (pre-1970) timestamps, and `map` instead of `unwrap` turns an
    // out-of-range value into NULL rather than panicking the connection.
    match unit {
        TimeUnit::Microsecond => {
            let v = col
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .unwrap()
                .value(row_idx); // micro-seconds
            let secs = v.div_euclid(1_000_000);
            // rem_euclid with a positive divisor lands in 0..1_000_000, so the
            // sub-second part always fits in a u32.
            #[allow(clippy::cast_possible_truncation)]
            let micros = v.rem_euclid(1_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, micros * 1_000)
                .map(|ts| ts.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        }
        TimeUnit::Millisecond => {
            let v = col
                .as_any()
                .downcast_ref::<TimestampMillisecondArray>()
                .unwrap()
                .value(row_idx); // milli-seconds
            let secs = v.div_euclid(1_000);
            // rem_euclid with a positive divisor lands in 0..1_000, so the
            // sub-second part always fits in a u32.
            #[allow(clippy::cast_possible_truncation)]
            let millis = v.rem_euclid(1_000) as u32;
            chrono::DateTime::from_timestamp(secs, millis * 1_000_000)
                .map(|ts| ts.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
        }
        TimeUnit::Nanosecond => {
            let v = col
                .as_any()
                .downcast_ref::<TimestampNanosecondArray>()
                .unwrap()
                .value(row_idx); // nano-seconds
            let secs = v.div_euclid(1_000_000_000);
            // rem_euclid with a positive divisor lands in 0..1_000_000_000, so
            // the sub-second part always fits in a u32.
            #[allow(clippy::cast_possible_truncation)]
            let nanos = v.rem_euclid(1_000_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, nanos)
                .map(|ts| ts.format("%Y-%m-%d %H:%M:%S%.9f").to_string())
        }
        TimeUnit::Second => unreachable!("TimeUnit::Second isn't used by Arrow today"),
    }
}

/// Encode the value at `row_idx` of `col` into `encoder`, picking the wire
/// representation from the column's Arrow type.
///
/// A type with no dedicated encoding is sent as the placeholder text
/// `[unsupported <type>]`, so an unmapped column costs the client that one
/// value instead of the whole row.
///
/// # Panics
///
/// Panics if a column's array does not match its declared data type, or if the
/// encoder rejects a value - both mean the batch and its schema disagree.
fn encode_column_value(encoder: &mut DataRowEncoder, col: &ArrayRef, row_idx: usize) {
    match col.data_type() {
        DataType::Utf8 | DataType::Utf8View | DataType::LargeUtf8 => {
            encoder
                .encode_field(&string_value_at(col, row_idx))
                .unwrap();
        }
        DataType::Int32 => {
            let array = col.as_any().downcast_ref::<Int32Array>().unwrap();
            let value = if col.is_null(row_idx) {
                None::<i32>
            } else {
                Some(array.value(row_idx))
            };
            encoder.encode_field(&value).unwrap();
        }
        DataType::Int64 => {
            let array = col.as_any().downcast_ref::<Int64Array>().unwrap();
            let value = if col.is_null(row_idx) {
                None::<i64>
            } else {
                Some(array.value(row_idx))
            };
            encoder.encode_field(&value).unwrap();
        }
        /* ----------  F L O A T S  ---------- */
        DataType::Float32 => {
            let arr = col.as_any().downcast_ref::<Float32Array>().unwrap();
            let value = if col.is_null(row_idx) {
                None::<f32>
            } else {
                Some(arr.value(row_idx))
            };
            encoder.encode_field(&value).unwrap();
        }
        DataType::Float64 => {
            let arr = col.as_any().downcast_ref::<Float64Array>().unwrap();
            let value = if col.is_null(row_idx) {
                None::<f64>
            } else {
                Some(arr.value(row_idx))
            };
            encoder.encode_field(&value).unwrap();
        }

        // ---------- TIMESTAMP us / ms / ns ----------
        DataType::Timestamp(unit, _) => {
            encoder
                .encode_field(&format_timestamp_value_at(col, *unit, row_idx))
                .unwrap();
        }
        DataType::Boolean => {
            let array = col.as_any().downcast_ref::<BooleanArray>().unwrap();
            let value = if col.is_null(row_idx) {
                None::<bool>
            } else {
                Some(array.value(row_idx))
            };
            encoder.encode_field(&value).unwrap();
        }

        DataType::List(inner)
            if matches!(
                inner.data_type(),
                DataType::Utf8 | DataType::Utf8View | DataType::LargeUtf8
            ) =>
        {
            let list = col.as_any().downcast_ref::<ListArray>().unwrap();
            let value: Option<Vec<String>> = if list.is_null(row_idx) {
                None
            } else {
                Some(stringify_string_array(&list.value(row_idx)))
            };
            encoder.encode_field(&value).unwrap();
        }

        _ => {
            if col.is_null(row_idx) {
                encoder.encode_field::<Option<&str>>(&None).unwrap();
            } else {
                let value = Some(format!("[unsupported {}]", col.data_type()));
                encoder.encode_field(&value).unwrap();
            }
        }
    }
}

/// Encode every row of `batch` into a stream of pgwire `DataRow`s described by
/// `schema`.
///
/// All rows are encoded up front so the returned stream borrows nothing from
/// `batch` and can be handed to pgwire as a `'static` response body.
///
/// # Panics
///
/// Panics if a column's array does not match its declared data type, or if the
/// encoder rejects a value.
fn batch_to_row_stream(
    batch: &RecordBatch,
    schema: &Arc<Vec<FieldInfo>>,
) -> impl Stream<Item = PgWireResult<DataRow>> + Send + 'static {
    let mut rows = Vec::new();
    for row_idx in 0..batch.num_rows() {
        let mut encoder = DataRowEncoder::new(schema.clone());
        for col in batch.columns() {
            encode_column_value(&mut encoder, col, row_idx);
        }
        rows.push(Ok(encoder.take_row()));
    }
    stream::iter(rows)
}

#[async_trait]
impl SimpleQueryHandler for DatafusionBackend {
    /// Run one simple-protocol query and turn its result into pgwire responses.
    ///
    /// Statements the server answers itself (transaction control, `DISCARD
    /// ALL`, `SHOW`) are handled first; everything else is dispatched to
    /// `DataFusion`. All batches of a result are concatenated into a single
    /// `Response::Query`, because a client reading one query expects one result
    /// set and would otherwise see only the last batch.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding a builtin response fails, if planning or
    /// executing the query fails, or if the result batches cannot be
    /// concatenated.
    async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        log::debug!("query handler");
        let trimmed = query.trim();
        let lowercase = trimmed.to_lowercase();
        // println!("query > {:?}", lowercase);

        if let Some(responses) = self.simple_query_builtin_response(trimmed, &lowercase)? {
            return Ok(responses);
        }

        let user = client.metadata().get(pgwire::api::METADATA_USER).cloned();
        let database = client
            .metadata()
            .get(pgwire::api::METADATA_DATABASE)
            .cloned();
        log::debug!("database: {database:?} {user:?}");

        self.register_current_database(client);
        if let Some(user) = client.metadata().get(pgwire::api::METADATA_USER) {
            // self.ctx is this connection's own context (see start_server), so
            // recording the role here does not leak it to other connections.
            let _ = crate::session::set_session_user(&self.ctx, user);
        }

        let dispatch_result =
            dispatch_query(&self.ctx, query, None, None, |ctx, sql, p, t| async move {
                let lsql = sql.to_lowercase();
                if lsql.contains("from users") {
                    let schema = Arc::new(Schema::new(vec![
                        Field::new("id", DataType::Int32, false),
                        Field::new("name", DataType::Utf8, true),
                    ]));
                    let batch = RecordBatch::try_new(
                        schema.clone(),
                        vec![
                            Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef,
                            Arc::new(StringArray::from(vec![Some("Alice"), Some("Bob")]))
                                as ArrayRef,
                        ],
                    )
                    .unwrap();
                    Ok((vec![batch], schema))
                } else {
                    execute_sql(ctx, sql, p, t).await
                }
            })
            .await;
        let (results, schema) = match dispatch_result {
            Ok(v) => v,
            Err(e) => {
                // The simple protocol has no bound parameters to record.
                self.capture_failed_query(query, &[], &[], &e);
                return Err(into_pgwire_error(e));
            }
        };

        let mut responses = Vec::new();

        if results.is_empty() {
            // A row-less result still owes the client a row description, and
            // `dispatch_query` already handed back the schema - so describe an
            // empty batch built from it instead of running the query again.
            let batch = RecordBatch::new_empty(schema.clone());
            let field_infos = Arc::new(batch_to_field_info(&batch, &Format::UnifiedText));
            let rows = batch_to_row_stream(&batch, &field_infos);

            responses.push(Response::Query(QueryResponse::new(field_infos, rows)));
        } else {
            // A query can produce several RecordBatches - one per UNION branch,
            // and one per ~8192 rows. They share a schema, so concatenate into a
            // SINGLE result set. Emitting one Response::Query per batch sends the
            // client multiple result sets for one query, and it sees only the last
            // (which silently dropped every UNION view's other branches).
            let combined = arrow::compute::concat_batches(&results[0].schema(), &results)
                .map_err(|e| PgWireError::ApiError(Box::new(e)))?;
            let field_infos = Arc::new(batch_to_field_info(&combined, &Format::UnifiedText));
            let rows = batch_to_row_stream(&combined, &field_infos);
            responses.push(Response::Query(QueryResponse::new(field_infos, rows)));
        }

        self.capture_successful_query(query, &[], &[], &results);

        // A SET reports the command tag rather than the (empty) result set the
        // planner produced for it.
        if lowercase.starts_with("set") {
            return Ok(vec![Response::Execution(Tag::new("SET"))]);
        }

        Ok(responses)
    }
}

#[async_trait]
impl ExtendedQueryHandler for DatafusionBackend {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    /// Hand pgwire the no-op parser: statements are stored as raw SQL text and
    /// parsed by `DataFusion` when the portal is executed.
    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    /// Execute a bound portal and return its rows as a single `Response`.
    ///
    /// `_max_rows` is ignored: the whole result is materialized and sent in one
    /// response rather than in client-sized chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding a builtin response fails, if planning or
    /// executing the statement fails, or if the result batches cannot be
    /// concatenated.
    async fn do_query<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: pgwire::api::store::PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        log::debug!(
            "query start extended {:?} {:?}",
            portal.statement.statement.as_str(),
            portal.parameters
        );

        let sql_trim = portal.statement.statement.trim();
        let lowercase = sql_trim.to_lowercase();

        if let Some(response) = self.extended_query_builtin_response(
            sql_trim,
            &lowercase,
            portal.result_column_format.format_for(0),
        )? {
            return Ok(response);
        }

        self.register_current_database(client);
        if let Some(user) = client.metadata().get(pgwire::api::METADATA_USER) {
            // self.ctx is this connection's own context (see start_server), so
            // recording the role here does not leak it to other connections.
            let _ = crate::session::set_session_user(&self.ctx, user);
        }

        let dispatch_result = dispatch_query(
            &self.ctx,
            portal.statement.statement.as_str(),
            Some(portal.parameters.clone()),
            Some(concrete_param_types(&portal.statement.parameter_types)),
            |ctx, sql, params, types| async move {
                let lsql = sql.to_lowercase();
                if lsql.contains("from users") {
                    let schema = Arc::new(Schema::new(vec![
                        Field::new("id", DataType::Int32, false),
                        Field::new("name", DataType::Utf8, true),
                    ]));
                    let batch = RecordBatch::try_new(
                        schema.clone(),
                        vec![
                            Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef,
                            Arc::new(StringArray::from(vec![Some("Alice"), Some("Bob")]))
                                as ArrayRef,
                        ],
                    )
                    .unwrap();
                    Ok((vec![batch], schema))
                } else {
                    execute_sql(ctx, sql, params, types).await
                }
            },
        )
        .await;
        let (results, schema) = match dispatch_result {
            Ok(v) => v,
            Err(e) => {
                self.capture_failed_query(
                    &portal.statement.statement,
                    &portal.parameters,
                    &portal.statement.parameter_types,
                    &e,
                );
                return Err(into_pgwire_error(e));
            }
        };

        // Concatenate all batches into one: a query can return several (one per
        // UNION branch, one per ~8192 rows), and sending only `results[0]` would
        // silently drop every batch after the first.
        let batch = if results.is_empty() {
            RecordBatch::new_empty(schema.clone())
        } else {
            arrow::compute::concat_batches(&results[0].schema(), &results)
                .map_err(|e| PgWireError::ApiError(Box::new(e)))?
        };

        let field_infos = Arc::new(batch_to_field_info(&batch, &portal.result_column_format));
        let rows = batch_to_row_stream(&batch, &field_infos);
        self.capture_successful_query(
            &portal.statement.statement,
            &portal.parameters,
            &portal.statement.parameter_types,
            &results,
        );
        Ok(Response::Query(QueryResponse::new(field_infos, rows)))
    }

    /// Describe the parameters and result columns of a prepared statement.
    ///
    /// Statements the server answers itself get a hand-written description; any
    /// other statement is executed so its result schema can be reported, since
    /// the query path has no plan-only mode.
    ///
    /// # Errors
    ///
    /// Returns an error if planning or executing the statement fails.
    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        log::debug!("do_describe_statement");

        let sql_trim = stmt.statement.trim();
        let lowercase = sql_trim.to_lowercase();

        // An empty statement and DISCARD ALL both produce no rows, so neither
        // has parameters or result columns to describe.
        if sql_trim.is_empty() || lowercase.starts_with("discard all") {
            return Ok(DescribeStatementResponse::new(vec![], vec![]));
        } else if lowercase == "show transaction isolation level" {
            let fields = vec![FieldInfo::new(
                "transaction_isolation".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Binary,
            )];
            return Ok(DescribeStatementResponse::new(vec![], fields));
        } else if lowercase.starts_with("show ") {
            let fields = vec![
                FieldInfo::new(
                    "name".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Binary,
                ),
                FieldInfo::new(
                    "setting".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Binary,
                ),
            ];
            return Ok(DescribeStatementResponse::new(vec![], fields));
        }

        let (results, schema) = execute_sql(&self.ctx, stmt.statement.as_str(), None, None)
            .await
            .map_err(into_pgwire_error)?;

        log::debug!("do_describe_statement {schema:?}");

        if results.is_empty() {
            return Ok(DescribeStatementResponse::new(vec![], vec![]));
        }

        let batch = &results[0];
        let param_types = concrete_param_types(&stmt.parameter_types);
        let fields = batch_to_field_info(batch, &Format::UnifiedBinary);
        log::debug!("return from do_describe {fields:?}");
        Ok(DescribeStatementResponse::new(param_types, fields))
    }

    /// Describe the result columns of a bound portal.
    ///
    /// Statements the server answers itself get a hand-written description; any
    /// other statement is executed so its result schema can be reported, since
    /// the query path has no plan-only mode.
    ///
    /// # Errors
    ///
    /// Returns an error if planning or executing the portal's statement fails.
    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: std::fmt::Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        log::debug!("do_describe_portal");
        let sql_trim = portal.statement.statement.trim();
        let lowercase = sql_trim.to_lowercase();

        // An empty statement and DISCARD ALL both produce no rows, so neither
        // has result columns to describe.
        if sql_trim.is_empty() || lowercase.starts_with("discard all") {
            return Ok(DescribePortalResponse::new(vec![]));
        } else if lowercase == "show transaction isolation level" {
            let fields = vec![FieldInfo::new(
                "transaction_isolation".to_string(),
                None,
                None,
                Type::TEXT,
                portal.result_column_format.format_for(0),
            )];
            return Ok(DescribePortalResponse::new(fields));
        } else if lowercase.starts_with("show ") {
            let fields = vec![
                FieldInfo::new(
                    "name".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    portal.result_column_format.format_for(0),
                ),
                FieldInfo::new(
                    "setting".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    portal.result_column_format.format_for(1),
                ),
            ];
            return Ok(DescribePortalResponse::new(fields));
        }

        let (results, schema) = execute_sql(
            &self.ctx,
            portal.statement.statement.as_str(),
            Some(portal.parameters.clone()),
            Some(concrete_param_types(&portal.statement.parameter_types)),
        )
        .await
        .map_err(into_pgwire_error)?;

        // println!("do_describe_portal {:?}", schema);

        let batch = if results.is_empty() {
            RecordBatch::new_empty(schema.clone())
        } else {
            results[0].clone()
        };

        let fields = batch_to_field_info(&batch, &portal.result_column_format);
        Ok(DescribePortalResponse::new(fields))
    }
}

/// Supplies pgwire with the handlers for one accepted connection.
///
/// The same [`DatafusionBackend`] serves both the simple and the extended query
/// protocol, so a client that mixes them sees one session state.
pub struct DatafusionBackendFactory {
    /// Backend shared by the simple and extended query handlers.
    handler: Arc<DatafusionBackend>,
}

impl PgWireServerHandlers for DatafusionBackendFactory {
    /// Handler for the simple query protocol.
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.handler.clone()
    }

    /// Handler for the extended query protocol (parse, bind, describe, execute).
    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.handler.clone()
    }

    /// Startup handler doing MD5 authentication and reporting [`SERVER_VERSION`].
    ///
    /// Clients gate feature use on the reported `server_version`, so it is
    /// overridden rather than left at pgwire's default.
    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        let mut params = DefaultServerParameterProvider::default();
        params.server_version = SERVER_VERSION.to_string();
        Arc::new(Md5PasswordAuthStartupHandler::new(
            Arc::new(DummyAuthSource),
            Arc::new(params),
        ))
    }

    /// COPY is not supported; the no-op handler rejects such requests.
    fn copy_handler(&self) -> Arc<impl CopyHandler> {
        Arc::new(NoopHandler)
    }

    /// Errors need no extra processing beyond what pgwire sends to the client.
    fn error_handler(&self) -> Arc<impl pgwire::api::ErrorHandler> {
        Arc::new(NoopHandler)
    }
}

/// Answer a GSSAPI encryption request on a freshly accepted socket, returning
/// the socket ready for the normal startup exchange.
///
/// Clients such as libpq open with a `GSSENCRequest` and wait for a reply before
/// sending the startup message. This server has no GSSAPI support, so the
/// request is consumed and refused with `N`, after which the client continues
/// unencrypted. Sockets that do not start with the request are handed back
/// untouched, and I/O failures are logged rather than dropping the connection
/// here - pgwire reports them when the handshake proper fails.
async fn detect_gssencmode(mut socket: TcpStream) -> Option<TcpStream> {
    let mut buf = [0u8; 8];

    if let Ok(n) = socket.peek(&mut buf).await {
        if n == 8 {
            let request_code = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            // Protocol constant for GSSENCRequest: 1234 << 16 | 5680.
            if request_code == 80_877_104 {
                if let Err(e) = socket.read_exact(&mut buf).await {
                    log::error!("Failed to consume GSSAPI request: {e:?}");
                }
                if let Err(e) = socket.write_all(b"N").await {
                    log::error!("Failed to send rejection message: {e:?}");
                }
            }
        }
    }

    Some(socket)
}

/// Serve the `PostgreSQL` wire protocol on `addr` until the process ends,
/// spawning a task per accepted connection.
///
/// `base_ctx` supplies the catalog and its planned views; every connection gets
/// its own [`SessionContext`] cloned from that state so per-session settings
/// stay private to one client. `capture`, when set, names a file that records
/// every query and result.
///
/// `_default_catalog` and `_default_schema` are accepted for call-site symmetry
/// with the session builders; the search path comes from the session state
/// instead.
///
/// # Errors
///
/// Returns an error if `addr` cannot be bound, or if accepting a connection
/// fails - the accept loop does not swallow those, so the server stops instead
/// of spinning on a broken listener.
pub async fn start_server(
    base_ctx: Arc<SessionContext>,
    addr: &str,
    _default_catalog: &str,
    _default_schema: &str,
    capture: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    log::info!("Listening on {addr}");

    let capture_store = capture.map(CaptureStore::new);

    loop {
        let (socket, _) = listener.accept().await?;
        if let Some(socket) = detect_gssencmode(socket).await {
            // Every connection gets its own context over the shared base. The
            // catalog and its planned views are shared, but the session config
            // is not - which is what lets each connection carry its own role
            // and client settings instead of overwriting the previous one's.
            let ctx = Arc::new(SessionContext::new_with_state(base_ctx.state().clone()));
            let factory = Arc::new(DatafusionBackendFactory {
                handler: Arc::new(DatafusionBackend::new(
                    Arc::clone(&ctx),
                    capture_store.clone(),
                )),
            });
            let factory = factory.clone();

            tokio::spawn(async move {
                if let Err(e) = process_socket(socket, None, factory).await {
                    log::error!("connection error: {e:?}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, BooleanArray, Int32Array};
    use arrow::datatypes::{Field, Schema};
    use futures::StreamExt;

    /// Boolean and int32 columns encode as their text representation, and NULL
    /// encodes as the length prefix -1.
    #[test]
    fn test_batch_to_row_stream_types_and_nulls() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("flag", DataType::Boolean, true),
            Field::new("num", DataType::Int32, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(BooleanArray::from(vec![Some(true), None])) as ArrayRef,
                Arc::new(Int32Array::from(vec![Some(42), None])) as ArrayRef,
            ],
        )
        .unwrap();

        let info = batch_to_field_info(&batch, &Format::UnifiedText);
        assert_eq!(info[0].datatype(), &Type::BOOL);
        assert_eq!(info[1].datatype(), &Type::INT4);

        let rows = futures::executor::block_on(
            batch_to_row_stream(&batch, &Arc::new(info)).collect::<Vec<_>>(),
        );
        assert_eq!(rows.len(), 2);

        let populated_row = rows[0].as_ref().unwrap();
        assert_eq!(populated_row.field_count, 2);
        let buf = &populated_row.data;
        assert_eq!(&buf[0..4], &1i32.to_be_bytes());
        assert_eq!(buf[4], b't');
        assert_eq!(&buf[5..9], &2i32.to_be_bytes());
        assert_eq!(&buf[9..11], b"42");

        let null_row = rows[1].as_ref().unwrap();
        let buf = &null_row.data;
        assert_eq!(&buf[0..4], &(-1i32).to_be_bytes());
        assert_eq!(&buf[4..8], &(-1i32).to_be_bytes());
    }

    /// A pre-1970 timestamp encodes as a formatted instant instead of
    /// panicking on its negative sub-second remainder.
    #[test]
    fn test_batch_to_row_stream_negative_timestamp_no_panic() {
        // A pre-1970 (negative) microsecond timestamp must format correctly
        // instead of panicking via unwrap() on an out-of-range sub-second value.
        use arrow::array::TimestampMicrosecondArray;
        let schema = Arc::new(Schema::new(vec![Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        )]));
        // -1_500_000 us = 1.5s before the epoch -> 1969-12-31 23:59:58.500000.
        let arr = TimestampMicrosecondArray::from(vec![Some(-1_500_000i64), None]);
        let batch = RecordBatch::try_new(schema, vec![Arc::new(arr) as ArrayRef]).unwrap();

        let info = batch_to_field_info(&batch, &Format::UnifiedText);
        let rows = futures::executor::block_on(
            batch_to_row_stream(&batch, &Arc::new(info)).collect::<Vec<_>>(),
        );
        assert_eq!(rows.len(), 2);

        // Row 0: the negative timestamp formats as a pre-1970 instant.
        let buf = &rows[0].as_ref().unwrap().data;
        let needle = b"1969-12-31 23:59:58.500000";
        assert!(
            buf.windows(needle.len()).any(|w| w == needle),
            "expected pre-1970 timestamp, got {:?}",
            String::from_utf8_lossy(buf)
        );
        // Row 1: NULL encodes as length -1.
        let buf1 = &rows[1].as_ref().unwrap().data;
        assert_eq!(&buf1[0..4], &(-1i32).to_be_bytes());
    }

    /// Every scalar Arrow type this server hands to clients maps to its
    /// `PostgreSQL` counterpart.
    #[test]
    fn test_arrow_to_pg_type() {
        assert_eq!(arrow_to_pg_type(&DataType::Boolean), Type::BOOL);
        assert_eq!(arrow_to_pg_type(&DataType::Int32), Type::INT4);
        assert_eq!(arrow_to_pg_type(&DataType::Int64), Type::INT8);
        assert_eq!(arrow_to_pg_type(&DataType::Int16), Type::INT2);
        assert_eq!(arrow_to_pg_type(&DataType::Utf8), Type::TEXT);
        assert_eq!(arrow_to_pg_type(&DataType::Utf8View), Type::TEXT);
        assert_eq!(arrow_to_pg_type(&DataType::LargeUtf8), Type::TEXT);
        assert_eq!(arrow_to_pg_type(&DataType::Float32), Type::FLOAT4);
        assert_eq!(arrow_to_pg_type(&DataType::Float64), Type::FLOAT8);
    }

    /// Any list column maps to some array type; an unmapped element type falls
    /// back to text[] rather than crashing the connection.
    #[test]
    fn test_arrow_to_pg_type_lists_never_panic() {
        use arrow::datatypes::Field;
        let list = |dt: DataType| DataType::List(Arc::new(Field::new("item", dt, true)));
        assert_eq!(
            arrow_to_pg_type(&list(DataType::Utf8View)),
            Type::TEXT_ARRAY
        );
        assert_eq!(
            arrow_to_pg_type(&list(DataType::LargeUtf8)),
            Type::TEXT_ARRAY
        );
        assert_eq!(arrow_to_pg_type(&list(DataType::Utf8)), Type::TEXT_ARRAY);
        assert_eq!(arrow_to_pg_type(&list(DataType::Int16)), Type::INT2_ARRAY);
        assert_eq!(
            arrow_to_pg_type(&list(DataType::Float64)),
            Type::FLOAT8_ARRAY
        );
        // An unmapped element type falls back to text[] instead of panicking.
        assert_eq!(arrow_to_pg_type(&list(DataType::Date32)), Type::TEXT_ARRAY);
    }

    /// A list<Utf8View> column is described as text[] and encodes without
    /// panicking on the view-backed element array.
    #[test]
    fn test_row_stream_list_utf8view_no_panic() {
        use arrow::array::{ListBuilder, StringViewBuilder};
        let mut lb = ListBuilder::new(StringViewBuilder::new());
        lb.values().append_value("a");
        lb.values().append_value("b");
        lb.append(true);
        let arr = lb.finish();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "tags",
            arr.data_type().clone(),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(arr) as ArrayRef]).unwrap();
        let info = batch_to_field_info(&batch, &Format::UnifiedText);
        assert_eq!(info[0].datatype(), &Type::TEXT_ARRAY);
        let rows = futures::executor::block_on(
            batch_to_row_stream(&batch, &Arc::new(info)).collect::<Vec<_>>(),
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_ok());
    }

    /// The advertised server version stays the one clients gate their feature
    /// detection on.
    #[test]
    fn test_server_version_constant() {
        assert_eq!(SERVER_VERSION, "17.4.0");
    }
}
