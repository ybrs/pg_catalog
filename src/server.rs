use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use futures::Stream;

use pgwire::api::auth::{AuthSource, DefaultServerParameterProvider, LoginInfo, Password};
use pgwire::api::auth::md5pass::{hash_md5_password, Md5PasswordAuthStartupHandler};
use pgwire::api::copy::NoopCopyHandler;
use pgwire::api::portal::{Format, Portal};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::{ClientInfo, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;

use arrow::array::{ArrayRef, BooleanArray, Int32Array, Int64Array, StringArray};
use arrow::record_batch::RecordBatch;
use datafusion::execution::context::SessionContext;

use crate::session::{execute_sql};

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
            if col.is_null(row_idx) {
                encoder.encode_field::<Option<&str>>(&None).unwrap();
            } else {
                let value = match col.data_type() {
                    arrow::datatypes::DataType::Utf8 => {
                        let array = col.as_any().downcast_ref::<StringArray>().unwrap();
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
                    _ => Some("[unsupported]".to_string()),
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
        _client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let results = execute_sql(&self.ctx, query).await.map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        let mut responses = Vec::new();
        for batch in results {
            let schema = Arc::new(batch_to_field_info(&batch, &Format::UnifiedText)?);
            let rows = batch_to_row_stream(&batch, schema.clone());
            responses.push(Response::Query(QueryResponse::new(schema, rows)));
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
        _client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let results = execute_sql(&self.ctx, portal.statement.statement.as_str())
            .await
            .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        if results.is_empty() {
            return Ok(Response::Execution(Tag::new("OK").into()));
        }

        let batch = &results[0];
        let schema = Arc::new(batch_to_field_info(batch, &portal.result_column_format)?);
        let rows = batch_to_row_stream(batch, schema.clone());
        Ok(Response::Query(QueryResponse::new(schema, rows)))
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let results = execute_sql(&self.ctx, stmt.statement.as_str())
            .await
            .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        if results.is_empty() {
            return Ok(DescribeStatementResponse::new(vec![], vec![]));
        }

        let batch = &results[0];
        let param_types = stmt.parameter_types.clone();
        let fields = batch_to_field_info(batch, &Format::UnifiedBinary)?;

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
        let results = execute_sql(&self.ctx, portal.statement.statement.as_str())
            .await
            .map_err(|e| PgWireError::ApiError(Box::new(e)))?;

        if results.is_empty() {
            return Ok(DescribePortalResponse::new(vec![]));
        }

        let batch = &results[0];
        let fields = batch_to_field_info(batch, &portal.result_column_format)?;
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

pub async fn start_server(ctx: Arc<SessionContext>, addr: &str) -> anyhow::Result<()> {
    let factory = Arc::new(DatafusionBackendFactory {
        handler: Arc::new(DatafusionBackend::new(ctx)),
    });

    let listener = TcpListener::bind(addr).await?;
    println!("Listening on {}", addr);

    loop {
        let (socket, _) = listener.accept().await?;
        let factory = factory.clone();
        tokio::spawn(async move {
            if let Err(e) = process_socket(socket, None, factory).await {
                eprintln!("connection error: {:?}", e);
            }
        });
    }
}
