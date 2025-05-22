use std::sync::{Arc};

use async_trait::async_trait;
use futures::{stream};
use futures::Stream;
use arrow::array::{Array, Float32Array, Float64Array};
use pgwire::api::auth::{AuthSource, DefaultServerParameterProvider, LoginInfo, Password};
use pgwire::api::auth::md5pass::{hash_md5_password, Md5PasswordAuthStartupHandler};
use pgwire::api::copy::NoopCopyHandler;
use pgwire::api::portal::{Format, Portal};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::{ClientInfo, PgWireServerHandlers, Type};
use pgwire::error::{PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use pgwire::tokio::process_socket;
use tokio::net::{TcpListener};

use arrow::array::{BooleanArray, Int32Array, Int64Array, LargeStringArray, ListArray, StringArray, StringViewArray};
use arrow::record_batch::RecordBatch;
use datafusion::execution::context::SessionContext;

use arrow::datatypes::{DataType, SchemaRef, TimeUnit};

use datafusion::{
    logical_expr::{create_udf, Volatility, ColumnarValue},
    common::ScalarValue,
};

use crate::replace::{regclass_udfs, replace_set_command_with_namespace, rewrite_array_subquery, rewrite_brace_array_literal};
use crate::session::{execute_sql, rewrite_filters, ClientOpts};
use crate::user_functions::{register_current_schema, register_pg_get_array, register_pg_get_one, register_pg_get_statisticsobjdef_columns, register_pg_postmaster_start_time, register_pg_relation_is_publishable, register_scalar_array_to_string, register_scalar_format_type, register_scalar_pg_encoding_to_char, register_scalar_pg_get_expr, register_scalar_pg_get_partkeydef, register_scalar_pg_get_userbyid, register_scalar_pg_table_is_visible, register_scalar_pg_tablespace_location, register_scalar_regclass_oid};
use tokio::net::TcpStream;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

pub struct DatafusionBackend {
    ctx: Arc<SessionContext>,
    query_parser: Arc<NoopQueryParser>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, BooleanArray, Int32Array};
    use arrow::datatypes::{Field, Schema};
    use futures::StreamExt;

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

        let info = batch_to_field_info(&batch, &Format::UnifiedText).unwrap();
        assert_eq!(info[0].datatype(), &Type::BOOL);
        assert_eq!(info[1].datatype(), &Type::INT4);

        let rows = futures::executor::block_on(
            batch_to_row_stream(&batch, Arc::new(info)).collect::<Vec<_>>(),
        );
        assert_eq!(rows.len(), 2);

        let row0 = rows[0].as_ref().unwrap();
        assert_eq!(row0.field_count, 2);
        let buf = &row0.data;
        assert_eq!(&buf[0..4], &1i32.to_be_bytes());
        assert_eq!(buf[4], b't');
        assert_eq!(&buf[5..9], &2i32.to_be_bytes());
        assert_eq!(&buf[9..11], b"42");

        let row1 = rows[1].as_ref().unwrap();
        let buf = &row1.data;
        assert_eq!(&buf[0..4], &(-1i32).to_be_bytes());
        assert_eq!(&buf[4..8], &(-1i32).to_be_bytes());
    }

    #[test]
    fn test_arrow_to_pg_type() {
        assert_eq!(arrow_to_pg_type(&DataType::Boolean), Type::BOOL);
        assert_eq!(arrow_to_pg_type(&DataType::Int32), Type::INT4);
        assert_eq!(arrow_to_pg_type(&DataType::Int64), Type::INT8);
        assert_eq!(arrow_to_pg_type(&DataType::Int16), Type::INT2);
        assert_eq!(arrow_to_pg_type(&DataType::Utf8), Type::TEXT);
        assert_eq!(arrow_to_pg_type(&DataType::Utf8View), Type::TEXT);
        assert_eq!(arrow_to_pg_type(&DataType::LargeUtf8), Type::TEXT);
        assert_eq!(arrow_to_pg_type(&DataType::Float32), Type::TEXT);
    }
}


impl DatafusionBackend {
    pub fn new(ctx: Arc<SessionContext>) -> Self {
        Self {
            ctx,
            query_parser: Arc::new(NoopQueryParser::new()),
        }
    }

    fn register_current_database<C>(&self, client: &C) -> datafusion::error::Result<()>
    where
        C: ClientInfo + ?Sized,
    {
        static KEY: &str = "current_database";

        if self.ctx.state().scalar_functions().contains_key(KEY){
            return Ok(());
        }

        if let Some(db) = client.metadata().get(pgwire::api::METADATA_DATABASE).cloned() {
            let fun = Arc::new(move |_args: &[ColumnarValue]| {
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(db.clone()))))
            });
            let udf = create_udf(KEY, vec![], DataType::Utf8, Volatility::Stable, fun);
            self.ctx.register_udf(udf);
        }

        Ok(())
    }

    fn register_session_user<C>(&self, client: &C) -> datafusion::error::Result<()>
    where
        C: ClientInfo + ?Sized,
    {
        static KEY: &str = "session_user";
        if self.ctx.state().scalar_functions().contains_key(KEY){
            return Ok(());
        }

        if let Some(user) = client.metadata().get(pgwire::api::METADATA_USER).cloned() {
            let fun = Arc::new(move |_args: &[ColumnarValue]| {
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(user.clone()))))
            });
            let udf = create_udf(KEY, vec![], DataType::Utf8, Volatility::Stable, fun);
            self.ctx.register_udf(udf);
        }

        Ok(())
    }

}

pub struct DummyAuthSource;

#[async_trait]
impl AuthSource for DummyAuthSource {
    async fn get_password(&self, login_info: &LoginInfo) -> PgWireResult<Password> {
        println!("login info: {:?}", login_info);

        let salt = vec![0, 0, 0, 0];
        let password = "pencil";

        let hash_password = hash_md5_password(
            login_info.user().as_ref().unwrap(),
            password,
            &salt,
        );
        Ok(Password::new(Some(salt), hash_password.as_bytes().to_vec()))
    }
}

fn arrow_to_pg_type(dt: &DataType) -> Type {
    use arrow::datatypes::DataType::*;

    match dt {
        Boolean        => Type::BOOL,
        Int16          => Type::INT2,
        Int32          => Type::INT4,
        Int64          => Type::INT8,
        Utf8 | Utf8View | LargeUtf8 => Type::TEXT,
        Timestamp(_, _)              => Type::TIMESTAMP,
        Float32        => Type::FLOAT4,   // real
        Float64        => Type::FLOAT8,   // double precision

        // ── arrays ───────────────────────────────────────────────
        List(inner) => match inner.data_type() {
            Utf8               => Type::TEXT_ARRAY,   // text[]
            Int32              => Type::INT4_ARRAY,   // int4[]
            Int64              => Type::INT8_ARRAY,   // int8[]
            Boolean            => Type::BOOL_ARRAY,   // bool[]
            // add more element types here as you need them
            other => panic!(
                "arrow_to_pg_type: no pgwire::Type for list<{other:?}>"
            ),
        },

        // anything else – send as plain text so the client can at
        // least see something instead of us mangling it away
        other => {
            eprintln!("arrow_to_pg_type: mapping {other:?} to TEXT");
            Type::TEXT
        }
    }
}


fn batch_to_field_info(batch: &RecordBatch, format: &Format) -> PgWireResult<Vec<FieldInfo>> {
    Ok(batch.schema().fields().iter().enumerate().map(|(idx, f)| {
        FieldInfo::new(
            f.name().to_string(),
            None,
            None,
            arrow_to_pg_type(f.data_type()),
            format.format_for(idx),
        )
    }).collect())
}

fn batch_to_row_stream(batch: &RecordBatch, schema: Arc<Vec<FieldInfo>>) -> impl Stream<Item = PgWireResult<DataRow>> {
    let mut rows = Vec::new();
    for row_idx in 0..batch.num_rows() {
        let mut encoder = DataRowEncoder::new(schema.clone());
        for col in batch.columns() {

            // if row_idx == 0 {
            //     println!("col {:?} type {:?}", col, col.data_type());
            // }

            match col.data_type() {
                DataType::Utf8 => {
                    let array = col.as_any().downcast_ref::<StringArray>().unwrap();
                    let value = if col.is_null(row_idx) {
                        None::<String>
                    } else {
                        Some(array.value(row_idx).to_string())
                    };
                    encoder.encode_field(&value).unwrap();
                }
                DataType::Utf8View => {
                    let array = col.as_any().downcast_ref::<StringViewArray>().unwrap();
                    let value = if col.is_null(row_idx) {
                        None::<String>
                    } else {
                        Some(array.value(row_idx).to_string())
                    };
                    encoder.encode_field(&value).unwrap();
                }
                DataType::LargeUtf8 => {
                    let array = col.as_any().downcast_ref::<LargeStringArray>().unwrap();
                    let value = if col.is_null(row_idx) {
                        None::<String>
                    } else {
                        Some(array.value(row_idx).to_string())
                    };
                    encoder.encode_field(&value).unwrap();
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


                // ---------- TIMESTAMP μs / ms / ns ----------
                DataType::Timestamp(unit, _) => {
                    match unit {
                        TimeUnit::Microsecond => {
                            use arrow::array::TimestampMicrosecondArray;
                            let arr =
                                col.as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
                            let value = if arr.is_null(row_idx) {
                                None::<String>
                            } else {
                                let v = arr.value(row_idx);              // micro-seconds
                                let secs = v / 1_000_000;
                                let micros = (v % 1_000_000) as u32;
                                let ts = chrono::NaiveDateTime::from_timestamp_opt(
                                    secs, micros * 1_000).unwrap();
                                Some(ts.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
                            };
                            encoder.encode_field(&value).unwrap();
                        }
                        TimeUnit::Millisecond => {
                            use arrow::array::TimestampMillisecondArray;
                            let arr =
                                col.as_any().downcast_ref::<TimestampMillisecondArray>().unwrap();
                            let value = if arr.is_null(row_idx) {
                                None::<String>
                            } else {
                                let v = arr.value(row_idx);              // milli-seconds
                                let secs = v / 1_000;
                                let millis = (v % 1_000) as u32;
                                let ts = chrono::NaiveDateTime::from_timestamp_opt(
                                    secs, millis * 1_000_000).unwrap();
                                Some(ts.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                            };
                            encoder.encode_field(&value).unwrap();
                        }
                        TimeUnit::Nanosecond => {
                            use arrow::array::TimestampNanosecondArray;
                            let arr =
                                col.as_any().downcast_ref::<TimestampNanosecondArray>().unwrap();
                            let value = if arr.is_null(row_idx) {
                                None::<String>
                            } else {
                                let v = arr.value(row_idx);              // nano-seconds
                                let secs = v / 1_000_000_000;
                                let nanos = (v % 1_000_000_000) as u32;
                                let ts = chrono::NaiveDateTime::from_timestamp_opt(
                                    secs, nanos).unwrap();
                                Some(ts.format("%Y-%m-%d %H:%M:%S%.9f").to_string())
                            };
                            encoder.encode_field(&value).unwrap();
                        }
                        _ => unreachable!(),   // TimeUnit::Second isn’t used by Arrow today
                    }
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

                DataType::List(inner) if inner.data_type() == &DataType::Utf8 => {
                    let list = col.as_any().downcast_ref::<ListArray>().unwrap();
                    let value = if list.is_null(row_idx) {
                        None::<String>
                    } else {
                        let arr = list.value(row_idx);
                        let sa  = arr.as_any().downcast_ref::<StringArray>().unwrap();
                        let mut items = Vec::with_capacity(sa.len());
                        for i in 0..sa.len() {
                            if sa.is_null(i) {
                                items.push("NULL".to_string());
                            } else {
                                items.push(sa.value(i).replace('"', r#"\""#));
                            }
                        }
                        Some(format!("{{{}}}", items.join(",")))
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
        rows.push(encoder.finish());
    }
    stream::iter(rows.into_iter())
}

#[async_trait]
impl SimpleQueryHandler for DatafusionBackend {
    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        println!("query handler");

        let lowercase = query.trim().to_lowercase();
        if lowercase.starts_with("begin") {
            return Ok(vec![Response::Execution(Tag::new("BEGIN"))]);
        } else if lowercase.starts_with("commit") {
            return Ok(vec![Response::Execution(Tag::new("COMMIT"))]);
        } else if lowercase.starts_with("rollback") {
            return Ok(vec![Response::Execution(Tag::new("ROLLBACK"))]);
        } else if lowercase == "" {
            return Ok(vec![Response::Execution(Tag::new(""))]);
        }



        let user = client.metadata().get(pgwire::api::METADATA_USER).cloned();
        let database = client.metadata().get(pgwire::api::METADATA_DATABASE).cloned();
        println!("database: {:?} {:?}", database, user);

        let _ = self.register_current_database(client);
        let _ = self.register_session_user(client);
        let (results, schema) = execute_sql(&self.ctx, query, None, None).await.map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        let mut responses = Vec::new();

        if results.is_empty() {
            println!("!!!!!!! ====== result is empty !!!!");
            // TODO: we are double parsing the sql here. this shouldn't be needed.
            //   also we arent using all the filters
            // let query = rewrite_array_subquery(&query).unwrap();
            // let query = rewrite_brace_array_literal(&query).unwrap();

            // let query = replace_set_command_with_namespace(&query)
            //     .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

            let (query, aliases) = rewrite_filters(query).unwrap();

            // zero-row result, but still need schema
            let df = self.ctx.sql(&query).await.map_err(|e| PgWireError::ApiError(Box::new(e)))?;
            let schema = df.schema();

            let batch = RecordBatch::new_empty(SchemaRef::from(schema.clone()));
            let schema = Arc::new(batch_to_field_info(&batch, &Format::UnifiedText)?);
            let rows = batch_to_row_stream(&batch, schema.clone());

            responses.push(Response::Query(QueryResponse::new(schema, rows)));
        } else {
            for batch in results {
                let schema = Arc::new(batch_to_field_info(&batch, &Format::UnifiedText)?);
                let rows = batch_to_row_stream(&batch, schema.clone());
                responses.push(Response::Query(QueryResponse::new(schema, rows)));
            }
        }

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

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {

        println!("query start extended {:?} {:?}", portal.statement.statement.as_str(), portal.parameters);


        if portal.statement.statement.trim().is_empty() {
            return Ok(Response::Execution(Tag::new("")));
        }
            

        let _ = self.register_current_database(client);
        let _ = self.register_session_user(client);

        let (results, schema) = execute_sql(&self.ctx, portal.statement.statement.as_str(),
                                  Some(portal.parameters.clone()),
                                  Some(portal.statement.parameter_types.clone()),
        ).await.map_err(|e| PgWireError::ApiError(Box::new(e)))?;


        let batch = if results.is_empty() {
            RecordBatch::new_empty(schema.clone())
        } else {
            results[0].clone()
        };

        let field_infos = Arc::new(batch_to_field_info(&batch, &portal.result_column_format)?);
        let rows = batch_to_row_stream(&batch, field_infos.clone());
        // println!("return from do_query {:?}", field_infos);
        Ok(Response::Query(QueryResponse::new(field_infos, rows)))
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        println!("do_describe_statement");
        
        if stmt.statement.trim().is_empty() {
            return Ok(DescribeStatementResponse::new(vec![], vec![]));
        }

        let (results, schema) = execute_sql(&self.ctx, stmt.statement.as_str(), None, None)
            .await
            .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        println!("do_describe_statement {:?}", schema);


        if results.is_empty() {
            return Ok(DescribeStatementResponse::new(vec![], vec![]));
        }

        let batch = &results[0];
        let param_types = stmt.parameter_types.clone();
        let fields = batch_to_field_info(batch, &Format::UnifiedBinary)?;
        println!("return from do_describe {:?}", fields);
        Ok(DescribeStatementResponse::new(param_types, fields))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {

        println!("do_describe_portal");
        if portal.statement.statement.trim().is_empty() {
            return Ok(DescribePortalResponse::new(vec![]));
        }

        let (results, schema) = execute_sql(&self.ctx, portal.statement.statement.as_str(),
            Some(portal.parameters.clone()),
            Some(portal.statement.parameter_types.clone()),
        ).await.map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        // println!("do_describe_portal {:?}", schema);

        let batch = if results.is_empty() {
            RecordBatch::new_empty(schema.clone())
        } else {
            results[0].clone()
        };

        let fields = batch_to_field_info(&batch, &portal.result_column_format)?;
        Ok(DescribePortalResponse::new(fields))
    }
}

pub struct DatafusionBackendFactory {
    handler: Arc<DatafusionBackend>,
}

impl PgWireServerHandlers for DatafusionBackendFactory {
    type StartupHandler = Md5PasswordAuthStartupHandler<DummyAuthSource, DefaultServerParameterProvider>;
    type SimpleQueryHandler = DatafusionBackend;
    type ExtendedQueryHandler = DatafusionBackend;
    type CopyHandler = NoopCopyHandler;
    type ErrorHandler = pgwire::api::NoopErrorHandler;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        let mut params = DefaultServerParameterProvider::default();
        params.server_version = "14.13".to_string();
        println!("startup handler");
        Arc::new(Md5PasswordAuthStartupHandler::new(
            Arc::new(DummyAuthSource),
            Arc::new(params),
        ))
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        Arc::new(NoopCopyHandler)
    }

    fn error_handler(&self) -> Arc<Self::ErrorHandler> {
        Arc::new(pgwire::api::NoopErrorHandler)
    }
}

async fn detect_gssencmode(mut socket: TcpStream) -> Option<TcpStream> {
    let mut buf = [0u8; 8];

    if let Ok(n) = socket.peek(&mut buf).await {
        if n == 8 {
            let request_code = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            if request_code == 80877104 {
                if let Err(e) = socket.read_exact(&mut buf).await {
                    println!("Failed to consume GSSAPI request: {:?}", e);
                }
                if let Err(e) = socket.write_all(b"N").await {
                    println!("Failed to send rejection message: {:?}", e);
                }
            }
        }
    }

    Some(socket)
}

use datafusion::error::Result as DFResult;

async fn ensure_pg_catalog_rows(ctx: &SessionContext) -> DFResult<()> {
    // 1. pg_class (row for oid 50010)
    if ctx
        .sql("SELECT 1 FROM pg_catalog.pg_class WHERE oid = 50010")
        .await?
        .count()
        .await?
        == 0
    {
        ctx.sql("INSERT INTO pg_catalog.pg_class
                 (oid, relname, relnamespace, relkind, reltuples, reltype)
                 VALUES (50010,'users',2200,'r',0,50011)")
            .await?
            .collect()
            .await?;
    }

    // 2. pg_type (row for oid 50011)
    if ctx
        .sql("SELECT 1 FROM pg_catalog.pg_type WHERE oid = 50011")
        .await?
        .count()
        .await?
        == 0
    {
        ctx.sql("INSERT INTO pg_catalog.pg_type
                 (oid, typname, typrelid, typlen, typcategory)
                 VALUES (50011,'_users',50010,-1,'C')")
            .await?
            .collect()
            .await?;
    }

    // 3. pg_attribute (rows for attrelid 50010)
    if ctx
        .sql("SELECT 1 FROM pg_catalog.pg_attribute
              WHERE attrelid = 50010 AND attnum = 1")
        .await?
        .count()
        .await?
        == 0
    {
        ctx.sql("INSERT INTO pg_catalog.pg_attribute
                 (attrelid,attnum,attname,atttypid,atttypmod,attnotnull,attisdropped)
                 VALUES
                 (50010,1,'id',23,-1,false,false),
                 (50010,2,'name',25,-1,false,false)")
            .await?
            .collect()
            .await?;
    }
    Ok(())
}

pub async fn start_server(base_ctx: Arc<SessionContext>, addr: &str, 
    default_catalog:&str, default_schema: &str) -> anyhow::Result<()> {

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on {}", addr);

    loop {
        let (socket, _) = listener.accept().await?;
        if let Some(socket) = detect_gssencmode(socket).await {

            let mut session_config = datafusion::execution::context::SessionConfig::new()
            .with_default_catalog_and_schema(default_catalog, default_schema)
            .with_option_extension(ClientOpts::default());
    
            session_config.options_mut().catalog.information_schema = true;
    
            let ctx = Arc::new(SessionContext::new_with_config_rt(session_config, base_ctx.runtime_env().clone()));
            
            // TODO: public is schema ! catalog is the database.
            if let Some(base_catalog) = base_ctx.catalog(default_catalog) {
                println!("re-registering schema pg_catalog");
                ctx.register_catalog(default_catalog, base_catalog.clone());
            }

            // TODO: duplicate code
            for f in regclass_udfs(&ctx) {
                ctx.register_udf(f);
            }
        
            ctx.register_udtf("regclass_oid", Arc::new(crate::user_functions::RegClassOidFunc));
        
            register_scalar_regclass_oid(&ctx)?;
            register_scalar_pg_tablespace_location(&ctx)?;
            register_current_schema(&ctx)?;
            register_scalar_format_type(&ctx)?;
            register_scalar_pg_get_expr(&ctx)?;
            register_scalar_pg_get_partkeydef(&ctx)?;
            register_scalar_pg_table_is_visible(&ctx)?;
            register_scalar_pg_get_userbyid(&ctx)?;
            register_scalar_pg_encoding_to_char(&ctx)?;
            register_scalar_array_to_string(&ctx)?;
            register_pg_get_one(&ctx)?;
            register_pg_get_array(&ctx)?;
            register_pg_get_statisticsobjdef_columns(&ctx)?;
            register_pg_relation_is_publishable(&ctx)?;
            register_pg_postmaster_start_time(&ctx)?;
            
            let df = ctx.sql("SELECT datname FROM pg_catalog.pg_database where datname='pgtry'").await?;
            if df.count().await? == 0 {
                let df = ctx.sql("INSERT INTO pg_catalog.pg_database (
                    oid,
                    datname,
                    datdba,
                    encoding,
                    datcollate,
                    datctype,
                    datistemplate,
                    datallowconn,
                    datconnlimit,
                    datfrozenxid,
                    datminmxid,
                    dattablespace,
                    datacl
                ) VALUES (
                    27734,
                    'pgtry',
                    27735,
                    6,
                    'C',
                    'C',
                    false,
                    true,
                    -1,
                
                    726,
                    1,
                    1663,
                    '{=Tc/dbuser,dbuser=CTc/dbuser}'
                );
                ").await?;
                df.show().await?;
    
            }
            let df = ctx.sql("select datname from pg_catalog.pg_database").await?;
            df.show().await?;

            ensure_pg_catalog_rows(&ctx).await?;


            let factory = Arc::new(DatafusionBackendFactory {
                handler: Arc::new(DatafusionBackend::new(Arc::clone(&ctx))),
            });
            let factory = factory.clone();

            tokio::spawn(async move {
                if let Err(e) = process_socket(socket, None, factory).await {
                    eprintln!("connection error: {:?}", e);
                }
            });

        }
    }
}
