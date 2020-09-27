use crate::{
    config::{log_schema, DataType, GenerateConfig, SinkConfig, SinkContext, SinkDescription},
    event::Event,
    sinks::{
        util::{
            batch::{Batch, BatchError},
            encode_event,
            encoding::{EncodingConfig, EncodingConfiguration},
            http::{BatchedHttpSink, HttpClient, HttpSink},
            BatchConfig, BatchSettings, BoxedRawValue, Compression, Encoding, JsonArrayBuffer,
            TowerRequestConfig, VecBuffer,
        },
        Healthcheck, VectorSink,
    },
};
use bytes::Bytes;
use flate2::write::GzEncoder;
use futures::FutureExt;
use futures01::Sink;
use http::{Request, StatusCode};
use hyper::body::Body;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{io::Write, time::Duration};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct DatadogLogsConfig {
    endpoint: Option<String>,
    api_key: String,
    encoding: EncodingConfig<Encoding>,

    #[serde(default)]
    compression: Option<Compression>,

    #[serde(default)]
    batch: BatchConfig,

    #[serde(default)]
    request: TowerRequestConfig,
}

#[derive(Clone)]
pub struct DatadogLogsJsonService {
    config: DatadogLogsConfig,
}

#[derive(Clone)]
pub struct DatadogLogsTextService {
    config: DatadogLogsConfig,
}

inventory::submit! {
    SinkDescription::new::<DatadogLogsConfig>("datadog_logs")
}

impl GenerateConfig for DatadogLogsConfig {}

impl DatadogLogsConfig {
    fn get_endpoint(&self) -> &str {
        self.endpoint
            .as_deref()
            .unwrap_or("https://http-intake.logs.datadoghq.eu/v1/input")
    }

    fn batch_settings<T: Batch>(&self) -> Result<BatchSettings<T>, BatchError> {
        BatchSettings::default()
            .bytes(bytesize::kib(100u64))
            .timeout(1)
            .parse_config(self.batch)
    }

    /// Builds the required BatchedHttpSink.
    /// Since the DataDog sink can create one of two different sinks, this
    /// extracts most of the shared functionality required to create either sink.
    fn build_sink<T, B, O>(
        &self,
        cx: SinkContext,
        service: T,
        batch: B,
        timeout: Duration,
    ) -> crate::Result<(VectorSink, Healthcheck)>
    where
        O: 'static,
        B: Batch<Output = Vec<O>> + std::marker::Send + 'static,
        B::Output: std::marker::Send + Clone,
        B::Input: std::marker::Send,
        T: HttpSink<Input = B::Input, Output = B::Output> + Clone,
    {
        let request_settings = self.request.unwrap_with(&TowerRequestConfig::default());

        let tls_settings = MaybeTlsSettings::from_config(
            &Some(self.tls.clone().unwrap_or_else(TlsConfig::enabled)),
            false,
        )?;

        let client = HttpClient::new(cx.resolver(), tls_settings)?;
        let healthcheck = healthcheck(service.clone(), client.clone()).boxed();
        let sink = BatchedHttpSink::new(
            service,
            batch,
            request_settings,
            timeout,
            client,
            cx.acker(),
        )
        .sink_map_err(|e| error!("Fatal datadog_logs text sink error: {}", e));

                let service = DatadogLogsTextService {
                    config: self.clone(),
                };
                let healthcheck = healthcheck(service.clone(), client.clone()).boxed();
                let sink = BatchedHttpSink::new(
                    service,
                    VecBuffer::new(batch_settings.size),
                    request_settings,
                    batch_settings.timeout,
                )
                .sink_map_err(|e| error!("Fatal datadog_logs text sink error: {}", e));

                Ok((VectorSink::Futures01Sink(Box::new(sink)), healthcheck))
            }
        }
    }

    /// Build the request, GZipping the contents if the config specifies.
    fn build_request(&self, body: Vec<u8>) -> crate::Result<http::Request<Vec<u8>>> {
        let uri = self.get_endpoint();
        let request = Request::post(uri)
            .header("Content-Type", "text/plain")
            .header("DD-API-KEY", self.api_key.clone());

        let compression = self.compression.unwrap_or(Compression::Gzip(None));

        let (request, body) = match compression {
            Compression::None => (request, body),
            Compression::Gzip(level) => {
                // Default the compression level to 6, which is similar to datadog agent.
                // https://docs.datadoghq.com/agent/logs/log_transport/?tab=https#log-compression
                let level = level.unwrap_or(6);
                let mut encoder =
                    GzEncoder::new(Vec::new(), flate2::Compression::new(level as u32));

                encoder.write_all(&body)?;
                (
                    request.header("Content-Encoding", "gzip"),
                    encoder.finish()?,
                )
            }
        };

        request
            .header("Content-Length", body.len())
            .body(body)
            .map_err(Into::into)
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "datadog_logs")]
impl SinkConfig for DatadogLogsConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        // Create a different sink depending on which encoding we have chosen.
        // Json and Text have different batching strategies and so each needs to be
        // handled differently.
        match self.encoding.codec {
            Encoding::Json => {
                let batch_settings = self.batch_settings()?;
                self.build_sink(
                    cx,
                    DatadogLogsJsonService {
                        config: self.clone(),
                    },
                    JsonArrayBuffer::new(batch_settings.size),
                    batch_settings.timeout,
                )
            }
            Encoding::Text => {
                let batch_settings = self.batch_settings()?;
                self.build_sink(
                    cx,
                    DatadogLogsTextService {
                        config: self.clone(),
                    },
                    VecBuffer::new(batch_settings.size),
                    batch_settings.timeout,
                )
            }
        }
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }

    fn sink_type(&self) -> &'static str {
        "datadog_logs"
    }
}

#[async_trait::async_trait]
impl HttpSink for DatadogLogsJsonService {
    type Input = serde_json::Value;
    type Output = Vec<BoxedRawValue>;

    fn encode_event(&self, mut event: Event) -> Option<Self::Input> {
        let log = event.as_mut_log();

        if let Some(message) = log.remove(log_schema().message_key()) {
            log.insert("message", message);
        }

        if let Some(timestamp) = log.remove(log_schema().timestamp_key()) {
            log.insert("date", timestamp);
        }

        if let Some(host) = log.remove(log_schema().host_key()) {
            log.insert("host", host);
        }

        self.config.encoding.apply_rules(&mut event);

        Some(json!(event.into_log()))
    }

    async fn build_request(&self, events: Self::Output) -> crate::Result<http::Request<Vec<u8>>> {
        let body = serde_json::to_vec(&events)?;
        self.config.build_request(body)
    }
}

#[async_trait::async_trait]
impl HttpSink for DatadogLogsTextService {
    type Input = Bytes;
    type Output = Vec<Bytes>;

    fn encode_event(&self, event: Event) -> Option<Self::Input> {
        encode_event(event, &self.config.encoding)
    }

    async fn build_request(&self, events: Self::Output) -> crate::Result<http::Request<Vec<u8>>> {
        let body: Vec<u8> = events.into_iter().flat_map(Bytes::into_iter).collect();
        self.config.build_request(body)
    }
}

/// The healthcheck is performed by sending an empty request to Datadog and checking
/// the return.
async fn healthcheck<T, O>(sink: T, mut client: HttpClient) -> crate::Result<()>
where
    T: HttpSink<Output = Vec<O>>,
{
    let req = sink.build_request(Vec::new()).await?.map(Body::from);

    let res = client.send(req).await?;

    let status = res.status();
    let body = hyper::body::to_bytes(res.into_body()).await?;

    match status {
        StatusCode::OK => Ok(()),
        StatusCode::UNAUTHORIZED => {
            let json: serde_json::Value = serde_json::from_slice(&body[..])?;

            Err(json
                .as_object()
                .and_then(|o| o.get("error"))
                .and_then(|s| s.as_str())
                .unwrap_or("Token is not valid, 401 returned.")
                .to_string()
                .into())
        }
        _ => {
            let body = String::from_utf8_lossy(&body[..]);

            Err(format!(
                "Server returned unexpected error status: {} body: {}",
                status, body
            )
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SinkConfig,
        sinks::util::test::{build_test_server, load_sink},
        test_util::{next_addr, random_lines_with_stream},
    };
    use futures::StreamExt;

    #[tokio::test]
    async fn smoke_text() {
        let (mut config, cx) = load_sink::<DatadogLogsConfig>(
            r#"
            api_key = "atoken"
            encoding = "text"
            compression = "none"
            batch.max_events = 1
            "#,
        )
        .unwrap();

        let addr = next_addr();
        // Swap out the endpoint so we can force send it
        // to our local server
        let endpoint = format!("http://{}", addr);
        config.endpoint = Some(endpoint.clone());

        let (sink, _) = config.build(cx).await.unwrap();

        let (rx, _trigger, server) = build_test_server(addr);
        tokio::spawn(server);

        let (expected, events) = random_lines_with_stream(100, 10);

        let _ = sink.run(events).await.unwrap();

        let output = rx.take(expected.len()).collect::<Vec<_>>().await;

        for (i, val) in output.iter().enumerate() {
            assert_eq!(val.1, format!("{}\n", expected[i]));
        }
    }
}

#[async_trait::async_trait]
impl HttpSink for DatadogLogsTextService {
    type Input = Bytes;
    type Output = Vec<Bytes>;

    fn encode_event(&self, event: Event) -> Option<Self::Input> {
        encode_event(event, &self.config.encoding)
    }

    async fn build_request(&self, events: Self::Output) -> crate::Result<http::Request<Vec<u8>>> {
        let body: Vec<u8> = events.iter().flat_map(|b| b.into_iter()).cloned().collect();
        self.config.build_request(body)
    }
}

/// The healthcheck is performed by sending an empty request to Datadog and checking
/// the return.
async fn healthcheck<T, O>(sink: T, mut client: HttpClient) -> crate::Result<()>
where
    T: HttpSink<Output = Vec<O>>,
{
    let req = sink.build_request(Vec::new()).await?.map(Body::from);

    #[tokio::test]
    async fn smoke_json() {
        let (mut config, cx) = load_sink::<DatadogLogsConfig>(
            r#"
            api_key = "atoken"
            encoding = "json"
            compression = "none"
            batch.max_events = 1
            "#,
        )
        .unwrap();

        let addr = next_addr();
        // Swap out the endpoint so we can force send it
        // to our local server
        let endpoint = format!("http://{}", addr);
        config.endpoint = Some(endpoint.clone());

        let (sink, _) = config.build(cx).await.unwrap();

        let (rx, _trigger, server) = build_test_server(addr);
        tokio::spawn(server);

        let (expected, events) = random_lines_with_stream(100, 10);

        let _ = sink.run(events).await.unwrap();

        let output = rx.take(expected.len()).collect::<Vec<_>>().await;

        for (i, val) in output.iter().enumerate() {
            let mut json = serde_json::Deserializer::from_slice(&val.1[..])
                .into_iter::<serde_json::Value>()
                .map(|v| v.expect("decoding json"));

            let json = json.next().unwrap();

            // The json we send to Datadog is an array of events.
            // As we have set batch.max_events to 1, each entry will be
            // an array containing a single record.
            let message = json
                .get(0)
                .unwrap()
                .get("message")
                .unwrap()
                .as_str()
                .unwrap();
            assert_eq!(message, expected[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SinkConfig,
        sinks::util::test::{build_test_server, load_sink},
        test_util::{next_addr, random_lines_with_stream},
    };
    use futures::StreamExt;

    #[tokio::test]
    async fn smoke_text() {
        let (mut config, cx) = load_sink::<DatadogLogsConfig>(
            r#"
            api_key = "atoken"
            encoding = "text"
            compression = "none"
            batch.max_events = 1
            "#,
        )
        .unwrap();

        let _ = config.build(cx.clone()).unwrap();

        let addr = next_addr();
        // Swap out the endpoint so we can force send it
        // to our local server
        let endpoint = format!("http://{}", addr);
        config.endpoint = Some(endpoint.clone());

        let (sink, _) = config.build(cx).await.unwrap();

        let (rx, _trigger, server) = build_test_server(addr);
        tokio::spawn(server);

        let (expected, events) = random_lines_with_stream(100, 10);

        let _ = sink.run(events).await.unwrap();

        let output = rx.take(expected.len()).collect::<Vec<_>>().await;

        for (i, val) in output.iter().enumerate() {
            assert_eq!(val.1, format!("{}\n", expected[i]));
        }
    }

    #[tokio::test]
    async fn smoke_json() {
        let (mut config, cx) = load_sink::<DatadogLogsConfig>(
            r#"
            api_key = "atoken"
            encoding = "json"
            compression = "none"
            batch.max_events = 1
            "#,
        )
        .unwrap();

        let addr = next_addr();
        // Swap out the endpoint so we can force send it
        // to our local server
        let endpoint = format!("http://{}", addr);
        config.endpoint = Some(endpoint.clone());

        let (sink, _) = config.build(cx).await.unwrap();

        let (rx, _trigger, server) = build_test_server(addr);
        tokio::spawn(server);

        let (expected, events) = random_lines_with_stream(100, 10);

        let _ = sink.run(events).await.unwrap();

        let output = rx.take(expected.len()).collect::<Vec<_>>().await;

        for (i, val) in output.iter().enumerate() {
            let mut json = serde_json::Deserializer::from_slice(&val.1[..])
                .into_iter::<serde_json::Value>()
                .map(|v| v.expect("decoding json"));

            let json = json.next().unwrap();

            // The json we send to Datadog is an array of events.
            // As we have set batch.max_events to 1, each entry will be
            // an array containing a single record.
            let message = json
                .get(0)
                .unwrap()
                .get("message")
                .unwrap()
                .as_str()
                .unwrap();
            assert_eq!(message, expected[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SinkConfig,
        sinks::util::test::{build_test_server, load_sink},
        test_util::{next_addr, random_lines_with_stream},
    };
    use futures::StreamExt;

    #[tokio::test]
    async fn smoke_text() {
        let (mut config, cx) = load_sink::<DatadogLogsConfig>(
            r#"
            api_key = "atoken"
            encoding = "text"
            batch.max_events = 1
            "#,
        )
        .unwrap();

        let _ = config.build(cx.clone()).unwrap();

        let addr = next_addr();
        // Swap out the endpoint so we can force send it
        // to our local server
        let endpoint = format!("http://{}", addr);
        config.endpoint = Some(endpoint.clone());

        let (sink, _) = config.build(cx).unwrap();

        let (rx, _trigger, server) = build_test_server(addr);
        tokio::spawn(server);

        let (expected, events) = random_lines_with_stream(100, 10);

        let _ = sink.run(events).await.unwrap();

        let output = rx.take(expected.len()).collect::<Vec<_>>().await;

        for (i, val) in output.iter().enumerate() {
            assert_eq!(val.1, format!("{}\n", expected[i]));
        }
    }

    #[tokio::test]
    async fn smoke_json() {
        let (mut config, cx) = load_sink::<DatadogLogsConfig>(
            r#"
            api_key = "atoken"
            encoding = "json"
            batch.max_events = 1
            "#,
        )
        .unwrap();

        let _ = config.build(cx.clone()).unwrap();

        let addr = next_addr();
        // Swap out the endpoint so we can force send it
        // to our local server
        let endpoint = format!("http://{}", addr);
        config.endpoint = Some(endpoint.clone());

        let (sink, _) = config.build(cx).unwrap();

        let (rx, _trigger, server) = build_test_server(addr);
        tokio::spawn(server);

        let (expected, events) = random_lines_with_stream(100, 10);

        let _ = sink.run(events).await.unwrap();

        let output = rx.take(expected.len()).collect::<Vec<_>>().await;

        for (i, val) in output.iter().enumerate() {
            let mut json = serde_json::Deserializer::from_slice(&val.1[..])
                .into_iter::<serde_json::Value>()
                .map(|v| v.expect("decoding json"));

            let json = json.next().unwrap();

            // The json we send to Datadog is an array of events.
            // As we have set batch.max_events to 1, each entry will be
            // an array containing a single record.
            let message = json
                .get(0)
                .unwrap()
                .get("message")
                .unwrap()
                .as_str()
                .unwrap();
            assert_eq!(message, expected[i]);
        }
    }
}
