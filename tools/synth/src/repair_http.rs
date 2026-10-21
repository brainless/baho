//! Bounded retries for transient repair model transport and HTTP status failures.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rig::core::error::{retryable_status, transient_transport};
use rig::http_client::ReqwestClient;
use rig::http_client::{
    Error, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};

#[cfg(not(test))]
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
#[cfg(test)]
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(1), Duration::from_millis(2)];

#[derive(Clone, Debug, Default)]
pub struct RetryEvents(Arc<Mutex<Vec<RetryEvent>>>);

#[derive(Clone, Debug)]
pub struct RetryEvent {
    pub attempts: u8,
    pub transport_result: &'static str,
    pub initial_http_status: Option<u16>,
    pub final_http_status: Option<u16>,
    /// One entry per wire attempt; null means a transport error or success.
    pub attempt_http_statuses: Vec<Option<u16>>,
    pub request_body_bytes: usize,
}

impl RetryEvents {
    pub fn drain(&self) -> Vec<RetryEvent> {
        self.0
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default()
    }

    fn push(&self, event: RetryEvent) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }
}

#[derive(Clone)]
pub struct RepairHttpClient {
    inner: ReqwestClient,
    events: RetryEvents,
}

impl RepairHttpClient {
    pub fn new(inner: ReqwestClient, events: RetryEvents) -> Self {
        Self { inner, events }
    }
}

// Request extensions are not used by the OpenAI adapter. Copying the wire
// fields keeps authorization and the serialized model turn identical on retry.
fn replay_request(req: &Request<Bytes>) -> Request<Bytes> {
    let mut replay = Request::new(req.body().clone());
    *replay.method_mut() = req.method().clone();
    *replay.uri_mut() = req.uri().clone();
    *replay.version_mut() = req.version();
    *replay.headers_mut() = req.headers().clone();
    replay
}

fn retryable(error: &Error) -> bool {
    match error.non_success_status() {
        Some(status) => retryable_status(Some(status.as_u16())),
        None => transient_transport(error),
    }
}

async fn with_retries<T, F, Fut>(
    mut send: F,
    events: RetryEvents,
    request_body_bytes: usize,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let mut initial_http_status = None;
    let mut attempt_http_statuses = Vec::with_capacity(RETRY_DELAYS.len() + 1);
    for (index, delay) in RETRY_DELAYS.into_iter().enumerate() {
        match send().await {
            Err(error) if retryable(&error) => {
                attempt_http_statuses.push(error.non_success_status().map(|s| s.as_u16()));
                if index == 0 {
                    initial_http_status = error.non_success_status().map(|s| s.as_u16());
                }
                tokio::time::sleep(delay).await;
            }
            result => {
                attempt_http_statuses.push(
                    result
                        .as_ref()
                        .err()
                        .and_then(Error::non_success_status)
                        .map(|s| s.as_u16()),
                );
                if index > 0 {
                    events.push(RetryEvent {
                        attempts: index as u8 + 1,
                        transport_result: if result.is_ok() {
                            "response_received"
                        } else {
                            "request_failed"
                        },
                        initial_http_status,
                        final_http_status: result
                            .as_ref()
                            .err()
                            .and_then(Error::non_success_status)
                            .map(|s| s.as_u16()),
                        attempt_http_statuses,
                        request_body_bytes,
                    });
                }
                return result;
            }
        }
    }
    let result = send().await;
    attempt_http_statuses.push(
        result
            .as_ref()
            .err()
            .and_then(Error::non_success_status)
            .map(|s| s.as_u16()),
    );
    events.push(RetryEvent {
        attempts: RETRY_DELAYS.len() as u8 + 1,
        transport_result: if result.is_ok() {
            "response_received"
        } else {
            "request_failed"
        },
        initial_http_status,
        final_http_status: result
            .as_ref()
            .err()
            .and_then(Error::non_success_status)
            .map(|s| s.as_u16()),
        attempt_http_statuses,
        request_body_bytes,
    });
    result
}

impl HttpClientExt for RepairHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, Error>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let inner = self.inner.clone();
        let events = self.events.clone();
        let req = req.map(Into::into);
        async move {
            let request_body_bytes = req.body().len();
            with_retries(
                || inner.send::<_, U>(replay_request(&req)),
                events,
                request_body_bytes,
            )
            .await
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, Error>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse, Error>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let inner = self.inner.clone();
        let events = self.events.clone();
        async move {
            let req = req.map(Into::into);
            let request_body_bytes = req.body().len();
            with_retries(
                || inner.send_streaming(replay_request(&req)),
                events,
                request_body_bytes,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[tokio::test]
    async fn retries_response_less_failure_once_with_same_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"opaque-turn") {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0 && request.len() + n < 8192);
                    request.extend_from_slice(&buf[..n]);
                }
                requests.push(request);
                if attempt == 1 {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .unwrap();
                }
            }
            requests
        });
        let events = RetryEvents::default();
        let client = RepairHttpClient::new(ReqwestClient::default(), events.clone());
        let request = Request::builder()
            .method("POST")
            .uri(format!("http://{address}/chat"))
            .header("authorization", "Bearer private-key")
            .header("connection", "close")
            .body(Bytes::from_static(b"opaque-turn"))
            .unwrap();
        let response = client.send::<_, Vec<u8>>(request).await.unwrap();
        assert_eq!(response.into_body().await.unwrap(), b"ok");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        let recorded = events.drain();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].transport_result, "response_received");
        assert!(events.drain().is_empty());
    }

    #[tokio::test]
    async fn stops_after_two_retries_when_transport_still_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0; 1024];
                let _ = stream.read(&mut buf).unwrap();
            }
        });
        let events = RetryEvents::default();
        let client = RepairHttpClient::new(ReqwestClient::default(), events.clone());
        let request = Request::builder()
            .method("POST")
            .uri(format!("http://{address}/chat"))
            .body(Bytes::from_static(b"private payload"))
            .unwrap();
        assert!(client.send::<_, Vec<u8>>(request).await.is_err());
        server.join().unwrap();
        let recorded = events.drain();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].attempts, 3);
        assert_eq!(recorded[0].transport_result, "request_failed");
    }

    #[test]
    fn retryable_status_matches_rig_policy() {
        let error = Error::non_success_with_details(
            rig::http_client::StatusCode::TOO_MANY_REQUESTS,
            Default::default(),
            "private provider body".into(),
        );
        assert!(retryable(&error));
        let error = Error::non_success_with_details(
            rig::http_client::StatusCode::BAD_REQUEST,
            Default::default(),
            "private provider body".into(),
        );
        assert!(!retryable(&error));
    }

    async fn status_case(
        statuses: &[u16],
    ) -> (Result<Vec<u8>, Error>, Vec<Vec<u8>>, Vec<RetryEvent>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let statuses = statuses.to_vec();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"opaque-turn") {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0 && request.len() + n < 8192);
                    request.extend_from_slice(&buf[..n]);
                }
                requests.push(request);
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        let events = RetryEvents::default();
        let client = RepairHttpClient::new(ReqwestClient::default(), events.clone());
        let request = Request::builder()
            .method("POST")
            .uri(format!("http://{address}/chat"))
            .header("authorization", "Bearer private-key")
            .header("connection", "close")
            .body(Bytes::from_static(b"opaque-turn"))
            .unwrap();
        let result = match client.send::<_, Vec<u8>>(request).await {
            Ok(response) => response.into_body().await,
            Err(error) => Err(error),
        };
        (result, server.join().unwrap(), events.drain())
    }

    #[tokio::test]
    async fn retries_503_and_429_with_identical_request() {
        for status in [503, 429] {
            let (result, requests, events) = status_case(&[status, 200]).await;
            assert_eq!(result.unwrap(), b"ok");
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0], requests[1]);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].attempts, 2);
            assert_eq!(events[0].initial_http_status, Some(status));
            assert_eq!(events[0].final_http_status, None);
            assert_eq!(events[0].transport_result, "response_received");
        }
    }

    #[tokio::test]
    async fn does_not_retry_400() {
        let (result, requests, events) = status_case(&[400]).await;
        assert_eq!(
            result.unwrap_err().non_success_status().unwrap().as_u16(),
            400
        );
        assert_eq!(requests.len(), 1);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn third_attempt_recovers_from_two_500_responses() {
        let (result, requests, events) = status_case(&[500, 500, 200]).await;
        assert_eq!(result.unwrap(), b"ok");
        assert_eq!(requests.len(), 3);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attempts, 3);
        assert_eq!(
            events[0].attempt_http_statuses,
            [Some(500), Some(500), None]
        );
        assert_eq!(events[0].request_body_bytes, b"opaque-turn".len());
        assert_eq!(events[0].initial_http_status, Some(500));
        assert_eq!(events[0].final_http_status, None);
        assert_eq!(events[0].transport_result, "response_received");
    }

    #[tokio::test]
    async fn stops_after_repeated_500_and_records_statuses() {
        let (result, requests, events) = status_case(&[500, 500, 500]).await;
        assert_eq!(
            result.unwrap_err().non_success_status().unwrap().as_u16(),
            500
        );
        assert_eq!(requests.len(), 3);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attempts, 3);
        assert_eq!(events[0].attempt_http_statuses, [Some(500); 3]);
        assert_eq!(events[0].request_body_bytes, b"opaque-turn".len());
        assert_eq!(events[0].initial_http_status, Some(500));
        assert_eq!(events[0].final_http_status, Some(500));
        assert_eq!(events[0].transport_result, "request_failed");
    }
}
