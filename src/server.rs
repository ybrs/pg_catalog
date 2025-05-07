use std::sync::{Arc};

use async_trait::async_trait;
use futures::{stream};
use futures::Stream;

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

use arrow::array::{BooleanArray, Int32Array, Int64Array, LargeStringArray, StringArray, StringViewArray};
use arrow::record_batch::RecordBatch;
use datafusion::execution::context::SessionContext;

use arrow::datatypes::{DataType, SchemaRef};

use datafusion::{
    logical_expr::{create_udf, Volatility, ColumnarValue},
    common::ScalarValue,
};

use crate::session::{execute_sql};
use tokio::net::TcpStream;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

pub struct DatafusionBackend {
    ctx: Arc<SessionContext>,
    query_parser: Arc<NoopQueryParser>,
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

fn batch_to_field_info(batch: &RecordBatch, format: &Format) -> PgWireResult<Vec<FieldInfo>> {
    Ok(batch.schema().fields().iter().enumerate().map(|(idx, f)| {
        FieldInfo::new(
            f.name().to_string(),
            None,
            None,
            Type::TEXT, // TODO: We return everything as TEXT for simplicity
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

            if col.is_null(row_idx) {
                encoder.encode_field::<Option<&str>>(&None).unwrap();
            } else {
                let value = match col.data_type() {
                    arrow::datatypes::DataType::Utf8 => {
                        let array = col.as_any().downcast_ref::<StringArray>().unwrap();
                        Some(array.value(row_idx).to_string())
                    }
                    arrow::datatypes::DataType::Utf8View => {
                        let array = col.as_any().downcast_ref::<StringViewArray>().unwrap();
                        Some(array.value(row_idx).to_string())
                    }
                    arrow::datatypes::DataType::LargeUtf8 => {
                        let array = col.as_any().downcast_ref::<LargeStringArray>().unwrap();
                        Some(array.value(row_idx).to_string())
                    }
                    arrow::datatypes::DataType::Int32 => {
                        let array = col.as_any().downcast_ref::<Int32Array>().unwrap();
                        Some(array.value(row_idx).to_string())
                    }
                    arrow::datatypes::DataType::Int64 => {
                        let array = col.as_any().downcast_ref::<Int64Array>().unwrap();
                        Some(array.value(row_idx).to_string())
                    }
                    arrow::datatypes::DataType::Boolean => {
                        let array = col.as_any().downcast_ref::<BooleanArray>().unwrap();
                        Some(array.value(row_idx).to_string())
                    }
                    _ => Some(format!("[unsupported {}]", col.data_type())),
                };
                encoder.encode_field(&value).unwrap();
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
        }


        let user = client.metadata().get(pgwire::api::METADATA_USER).cloned();
        let database = client.metadata().get(pgwire::api::METADATA_DATABASE).cloned();
        println!("database: {:?} {:?}", database, user);

        let _ = self.register_current_database(client);
        let _ = self.register_session_user(client);
        let (results, schema) = execute_sql(&self.ctx, query, None, None).await.map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        let mut responses = Vec::new();

        if results.is_empty() {
            // TODO:
            // zero-row result, but still need schema
            let df = self.ctx.sql(query).await.map_err(|e| PgWireError::ApiError(Box::new(e)))?;
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
        println!("return from do_query {:?}", field_infos);
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
        let (results, schema) = execute_sql(&self.ctx, portal.statement.statement.as_str(), None, None)
            .await
            .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        println!("do_describe_portal {:?}", schema);

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


pub async fn start_server(ctx: Arc<SessionContext>, addr: &str) -> anyhow::Result<()> {

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on {}", addr);

    loop {
        let (socket, _) = listener.accept().await?;
        if let Some(socket) = detect_gssencmode(socket).await {

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
