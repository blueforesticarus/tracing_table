//! Finite HTTP load generation and its capture audit, separate from handlers.
use crate::capture_support::{Result, Session, setting};
use awc::{Client, http::Method};
use futures_util::future::join_all;
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use tracing_table::subscriber::dump::{Fixture, Operation};

pub struct Settings {
    pub concurrency: u64,
    pub max_requests: u64,
}
impl Settings {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            concurrency: setting("TRACING_CAPTURE_CONCURRENCY", 32)?,
            max_requests: setting("TRACING_CAPTURE_MAX_REQUESTS", 1_000_000)?,
        })
    }
}

#[derive(Default)]
pub struct Traffic {
    pub requests: u64,
    pub statuses: BTreeMap<u16, usize>,
}

pub async fn run(session: &Session, address: SocketAddr, settings: &Settings) -> Result<Traffic> {
    let client = Client::builder().timeout(Duration::from_secs(60)).finish();
    let mut traffic = Traffic::default();
    let mut batches = 0;
    // Drain each batch before stopping; every request and middleware span completes.
    while session.capture.uncompressed_bytes() < session.target {
        let count = settings
            .concurrency
            .min(settings.max_requests - traffic.requests);
        if count == 0 {
            return Err("capture did not reach target before request limit".into());
        }
        let responses = join_all(
            (traffic.requests..traffic.requests + count)
                .map(|id| send_request(&client, address, id)),
        )
        .await;
        for response in responses {
            *traffic.statuses.entry(response?).or_default() += 1;
        }
        traffic.requests += count;
        batches += 1;
        if batches % 32 == 0 {
            session.progress(traffic.requests, "requests");
        }
    }
    Ok(traffic)
}

async fn send_request(client: &Client, address: SocketAddr, id: u64) -> Result<u16> {
    let (method, route) = match id % 10 {
        0 => (Method::GET, "/health".into()),
        1..=5 => (Method::GET, format!("/catalog/{}", id % 2048)),
        6..=8 => (Method::POST, "/orders".into()),
        _ => (Method::POST, "/reports".into()),
    };
    let mut response = client
        .request(method, format!("http://{address}{route}"))
        .insert_header(("x-workload-id", id.to_string()))
        .insert_header(("user-agent", "tracing-table-capture/1"))
        .send_body(format!(
            "{{\"tenant\":{},\"sku\":{},\"quantity\":{}}}",
            id % 32,
            id % 2048,
            1 + id % 5
        ))
        .await?;
    let status = response.status().as_u16();
    // Keep this expectation independent of the handler's outcome selection.
    let expected = if id % 10 == 0 {
        200
    } else if id % 101 == 0 {
        503
    } else if id % 37 == 0 {
        409
    } else {
        200
    };
    if status != expected {
        return Err(format!("request {id}: expected {expected}, got {status}").into());
    }
    // Consume bodies so middleware completion and connection reuse run.
    if response.body().await?.is_empty() {
        return Err(format!("request {id}: empty response").into());
    }
    Ok(status)
}

/// Validate every request's middleware lifecycle after graceful server shutdown.
pub fn validate(fixture: &Fixture, requests: u64) -> Result<u64> {
    let mut middleware_requests = 0u64;
    let mut open_requests = std::collections::BTreeSet::new();
    for record in &fixture.records {
        match &record.operation {
            Operation::Span { site, id, .. }
                if fixture.callsites[*site]
                    .metadata
                    .target()
                    .starts_with("tracing_actix_web") =>
            {
                middleware_requests += 1;
                open_requests.insert(*id);
            }
            Operation::Close { id } => {
                open_requests.remove(id);
            }
            _ => {}
        }
    }
    if middleware_requests != requests {
        return Err(
            format!("expected {requests} middleware spans, found {middleware_requests}").into(),
        );
    }
    if !open_requests.is_empty() {
        return Err("request spans still open at shutdown".into());
    }
    if !fixture.records.iter().any(|record| {
        matches!(
            record.operation,
            Operation::Event {
                parent: Some(_),
                ..
            }
        )
    }) {
        return Err("missing request parent links".into());
    }
    Ok(middleware_requests)
}
