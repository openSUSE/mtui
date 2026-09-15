//! A loopback HTTP/1.1 keep-alive responder that counts accepted connections
//! and requests. wiremock exposes no connection identity, and "one pool, one
//! connection" is exactly the property the #594 tests pin.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mtui_datasources::HttpClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The responder's address and counters.
pub(crate) struct CountingHost {
    pub(crate) base: String,
    accepts: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

impl CountingHost {
    pub(crate) fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }

    pub(crate) fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// The `Content-Length` of a request head, `0` when absent.
fn content_length(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

/// Answer every request on every connection with `200` + `body`, keeping the
/// connection open. A request body is consumed per `Content-Length`, so a
/// POST leaves nothing behind to be read as the next head.
pub(crate) async fn counting_host(body: &'static str) -> CountingHost {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let host = CountingHost {
        base,
        accepts: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(AtomicUsize::new(0)),
    };
    let accepts = Arc::clone(&host.accepts);
    let requests = Arc::clone(&host.requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            accepts.fetch_add(1, Ordering::SeqCst);
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // Bytes read past the current request: the start of the next.
                let mut pending = Vec::new();
                loop {
                    let head_end = loop {
                        if let Some(i) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => pending.extend_from_slice(&buf[..n]),
                        }
                    };
                    let want = head_end + content_length(&pending[..head_end]);
                    while pending.len() < want {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => pending.extend_from_slice(&buf[..n]),
                        }
                    }
                    pending.drain(..want);
                    requests.fetch_add(1, Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                        body.len()
                    );
                    if sock.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    host
}

/// Open one pooled connection from `http` to `host` and prove the pool hands
/// it back before returning. hyper returns an idle HTTP/1 connection to the
/// pool from a spawned task, so a request issued before that task ran opens a
/// second socket; the yields let it run, and the second GET is the premise
/// check that reuse works here at all.
pub(crate) async fn warm_pool(http: &HttpClient, host: &CountingHost) {
    for _ in 0..2 {
        http.get_bytes(&format!("{}/warm", host.base))
            .await
            .unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }
    assert_eq!(
        host.accepts(),
        1,
        "premise: the pool did not reuse its idle connection"
    );
}

/// A POST body is framed by `Content-Length`, never mistaken for the next
/// request head — even one that contains a blank line. A bare reqwest client
/// with its own pool, since `HttpClient` exposes no POST.
#[tokio::test]
async fn post_body_is_consumed_with_its_request() {
    let host = counting_host("ok").await;
    let client = reqwest::Client::new();
    let settle = || async {
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    };

    client
        .get(format!("{}/warm", host.base))
        .send()
        .await
        .unwrap();
    settle().await;
    let status = client
        .post(format!("{}/post", host.base))
        .body("x\r\n\r\ny")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200);
    settle().await;
    client
        .get(format!("{}/after", host.base))
        .send()
        .await
        .unwrap();

    assert_eq!(
        host.requests(),
        3,
        "the body was answered as a request of its own"
    );
    assert_eq!(
        host.accepts(),
        1,
        "the desynchronised connection was dropped"
    );
}
