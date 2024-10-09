// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Default [FlightDriver] for Flight SQL

use std::collections::HashMap;

use arrow_flight::error::Result;
use arrow_flight::sql::client::FlightSqlServiceClient;
use async_trait::async_trait;
use tonic::transport::Channel;

use crate::{FlightDriver, FlightMetadata};

pub const QUERY: &str = "flight.sql.query";
pub const USERNAME: &str = "flight.sql.username";
pub const PASSWORD: &str = "flight.sql.password";
pub const HEADER_PREFIX: &str = "flight.sql.header.";

/// Default Flight SQL driver. Requires a [QUERY] to be passed as a table option.
/// If [USERNAME] (and optionally [PASSWORD]) are passed,
/// will perform the `Handshake` using basic authentication.
/// Any additional headers for the `GetFlightInfo` call can be passed as table options
/// using the [HEADER_PREFIX] prefix.
/// If a token is returned by the server with the handshake response, it will be
/// stored as a gRPC authorization header within the returned [FlightMetadata],
/// to be sent with the subsequent `DoGet` requests.
#[derive(Clone, Debug, Default)]
pub struct FlightSqlDriver {}

#[async_trait]
impl FlightDriver for FlightSqlDriver {
    async fn metadata(
        &self,
        channel: Channel,
        options: &HashMap<String, String>,
    ) -> Result<FlightMetadata> {
        let mut client = FlightSqlServiceClient::new(channel);
        let headers = options.iter().filter_map(|(key, value)| {
            key.strip_prefix(HEADER_PREFIX)
                .map(|header_name| (header_name, value))
        });
        for header in headers {
            client.set_header(header.0, header.1)
        }
        if let Some(username) = options.get(USERNAME) {
            let default_password = "".to_string();
            let password = options.get(PASSWORD).unwrap_or(&default_password);
            _ = client.handshake(username, password).await?;
        }
        let info = client.execute(options[QUERY].clone(), None).await?;
        let mut grpc_headers = HashMap::default();
        if let Some(token) = client.token() {
            grpc_headers.insert("authorization".into(), format!("Bearer {}", token));
        }
        FlightMetadata::try_new(info, grpc_headers)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use arrow_array::{Array, Float32Array, Int64Array, Int8Array, RecordBatch};
    use arrow_flight::encode::FlightDataEncoderBuilder;
    use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
    use arrow_flight::sql::server::FlightSqlService;
    use arrow_flight::sql::{
        CommandStatementQuery, ProstMessageExt, SqlInfo, TicketStatementQuery,
    };
    use arrow_flight::{
        FlightDescriptor, FlightEndpoint, FlightInfo, HandshakeRequest, HandshakeResponse, Ticket,
    };
    use arrow_schema::{DataType, Field, Schema};
    use async_trait::async_trait;
    use datafusion::prelude::SessionContext;
    use futures::{stream, Stream, TryStreamExt};
    use prost::Message;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot::{channel, Receiver, Sender};
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::codegen::http::HeaderMap;
    use tonic::codegen::tokio_stream;
    use tonic::metadata::MetadataMap;
    use tonic::transport::Server;
    use tonic::{Extensions, Request, Response, Status, Streaming};

    use crate::sql::FlightSqlDriver;
    use crate::FlightTableFactory;

    const AUTH_HEADER: &str = "authorization";
    const BEARER_TOKEN: &str = "Bearer flight-sql-token";

    struct TestFlightSqlService {
        flight_info: FlightInfo,
        partition_data: RecordBatch,
        expected_handshake_headers: HashMap<String, String>,
        expected_flight_info_query: String,
        shutdown_sender: Option<Sender<()>>,
    }

    impl TestFlightSqlService {
        async fn run_in_background(self, rx: Receiver<()>) -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let service = FlightServiceServer::new(self);
            #[allow(clippy::disallowed_methods)] // spawn allowed only in tests
            tokio::spawn(async move {
                Server::builder()
                    .timeout(Duration::from_secs(1))
                    .add_service(service)
                    .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                        rx.await.ok();
                    })
                    .await
                    .unwrap();
            });
            tokio::time::sleep(Duration::from_millis(25)).await;
            addr
        }
    }

    impl Drop for TestFlightSqlService {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown_sender.take() {
                tx.send(()).ok();
            }
        }
    }

    fn check_header<T>(request: &Request<T>, rpc: &str, header_name: &str, expected_value: &str) {
        let actual_value = request
            .metadata()
            .get(header_name)
            .unwrap_or_else(|| panic!("[{}] missing header `{}`", rpc, header_name))
            .to_str()
            .unwrap_or_else(|e| {
                panic!(
                    "[{}] error parsing value for header `{}`: {:?}",
                    rpc, header_name, e
                )
            });
        assert_eq!(
            actual_value, expected_value,
            "[{}] unexpected value for header `{}`",
            rpc, header_name
        )
    }

    #[async_trait]
    impl FlightSqlService for TestFlightSqlService {
        type FlightService = TestFlightSqlService;

        async fn do_handshake(
            &self,
            request: Request<Streaming<HandshakeRequest>>,
        ) -> Result<
            Response<Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>>,
            Status,
        > {
            for (header_name, expected_value) in self.expected_handshake_headers.iter() {
                check_header(&request, "do_handshake", header_name, expected_value);
            }
            Ok(Response::from_parts(
                MetadataMap::from_headers(HeaderMap::from_iter([(
                    AUTH_HEADER.parse().unwrap(),
                    BEARER_TOKEN.parse().unwrap(),
                )])), // the client should send this header back on the next request (i.e. GetFlightInfo)
                Box::pin(tokio_stream::empty()),
                Extensions::default(),
            ))
        }

        async fn get_flight_info_statement(
            &self,
            query: CommandStatementQuery,
            request: Request<FlightDescriptor>,
        ) -> Result<Response<FlightInfo>, Status> {
            let mut expected_flight_info_headers = self.expected_handshake_headers.clone();
            expected_flight_info_headers.insert(AUTH_HEADER.into(), BEARER_TOKEN.into());
            for (header_name, expected_value) in expected_flight_info_headers.iter() {
                check_header(&request, "get_flight_info", header_name, expected_value);
            }
            assert_eq!(
                query.query.to_lowercase(),
                self.expected_flight_info_query.to_lowercase()
            );
            Ok(Response::new(self.flight_info.clone()))
        }

        async fn do_get_statement(
            &self,
            _ticket: TicketStatementQuery,
            request: Request<Ticket>,
        ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
            let data = self.partition_data.clone();
            let rb = async move { Ok(data) };
            check_header(&request, "do_get", "authorization", BEARER_TOKEN);
            let stream = FlightDataEncoderBuilder::default()
                .with_schema(self.partition_data.schema())
                .build(stream::once(rb))
                .map_err(|e| Status::from_error(Box::new(e)));

            Ok(Response::new(Box::pin(stream)))
        }

        async fn register_sql_info(&self, _id: i32, _result: &SqlInfo) {}
    }

    #[tokio::test]
    async fn test_flight_sql_data_source() -> datafusion::common::Result<()> {
        let partition_data = RecordBatch::try_new(
            Arc::new(Schema::new([
                Arc::new(Field::new("col1", DataType::Float32, false)),
                Arc::new(Field::new("col2", DataType::Int8, false)),
            ])),
            vec![
                Arc::new(Float32Array::from(vec![0.0, 0.1, 0.2, 0.3])),
                Arc::new(Int8Array::from(vec![10, 20, 30, 40])),
            ],
        )
        .unwrap();
        let rows_per_partition = partition_data.num_rows();

        let query = "SELECT * FROM some_table";
        let ticket_payload = TicketStatementQuery::default().as_any().encode_to_vec();
        let endpoint_archetype = FlightEndpoint::default().with_ticket(Ticket::new(ticket_payload));
        let endpoints = vec![
            endpoint_archetype.clone(),
            endpoint_archetype.clone(),
            endpoint_archetype,
        ];
        let num_partitions = endpoints.len();
        let flight_info = FlightInfo::default()
            .try_with_schema(partition_data.schema().as_ref())
            .unwrap();
        let flight_info = endpoints
            .into_iter()
            .fold(flight_info, |fi, e| fi.with_endpoint(e));
        let (tx, rx) = channel();
        let service = TestFlightSqlService {
            flight_info,
            partition_data,
            expected_handshake_headers: HashMap::from([
                (AUTH_HEADER.into(), "Basic YWRtaW46cGFzc3dvcmQ=".into()),
                ("custom-hdr1".into(), "v1".into()),
                ("custom-hdr2".into(), "v2".into()),
            ]),
            expected_flight_info_query: query.into(),
            shutdown_sender: Some(tx),
        };
        let port = service.run_in_background(rx).await.port();
        let ctx = SessionContext::new();
        ctx.state_ref().write().table_factories_mut().insert(
            "FLIGHT_SQL".into(),
            Arc::new(FlightTableFactory::new(
                Arc::new(FlightSqlDriver::default()),
            )),
        );
        let _ = ctx
            .sql(&format!(
                r#"
            CREATE EXTERNAL TABLE fsql STORED AS FLIGHT_SQL
            LOCATION 'http://localhost:{port}'
            OPTIONS(
                'flight.sql.username' 'admin',
                'flight.sql.password' 'password',
                'flight.sql.query' '{query}',
                'flight.sql.header.custom-hdr1' 'v1',
                'flight.sql.header.custom-hdr2' 'v2',
            )"#
            ))
            .await
            .unwrap();
        let df = ctx.sql("select col1 from fsql").await.unwrap();
        assert_eq!(
            df.count().await.unwrap(),
            rows_per_partition * num_partitions
        );
        let df = ctx.sql("select sum(col2) from fsql").await?;
        let rb = df
            .collect()
            .await?
            .first()
            .cloned()
            .expect("no record batch");
        assert_eq!(rb.schema().fields.len(), 1);
        let arr = rb
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("wrong type of column");
        assert_eq!(arr.iter().next().unwrap().unwrap(), 300);
        Ok(())
    }
}
