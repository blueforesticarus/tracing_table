//! Mock service work is isolated from capture/load generation for easy tuning.
use crate::capture_support::{Result, setting};
use actix_web::{HttpRequest, HttpResponse, error, http::StatusCode, web};
use std::{hint::black_box, time::Duration};
use tracing::field::Empty;

#[derive(Clone, Copy)]
pub struct Workload {
    pub batches: u64,
    pub cpu_rounds: u64,
    pub io_micros: u64,
}
impl Workload {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            batches: setting("TRACING_CAPTURE_BATCHES", 8)?,
            cpu_rounds: setting("TRACING_CAPTURE_CPU_ROUNDS", 4096)?,
            io_micros: setting("TRACING_CAPTURE_IO_MICROS", 100)?,
        })
    }
}

pub async fn health() -> HttpResponse {
    tracing::debug!(target: "actix_workload::health", ready = true, "readiness probe");
    HttpResponse::Ok().body("ready")
}

pub async fn request(
    req: HttpRequest,
    body: web::Bytes,
    settings: web::Data<Workload>,
) -> actix_web::Result<HttpResponse> {
    // Treat malformed input as a request error instead of panicking a server worker.
    let id = req
        .headers()
        .get("x-workload-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| error::ErrorBadRequest("invalid x-workload-id"))?;
    handle(
        id,
        req.method().as_str(),
        &req.match_pattern().unwrap_or_default(),
        body.len(),
        *settings.get_ref(),
    )
    .await
}

#[tracing::instrument(name = "service_request", skip_all,
    fields(request = id, tenant = id % 32, user = id % 4096, route, status = Empty, result_bytes = Empty))]
async fn handle(
    id: u64,
    method: &str,
    route: &str,
    body_bytes: usize,
    settings: Workload,
) -> actix_web::Result<HttpResponse> {
    tracing::info!(method, body_bytes, "request accepted");
    tracing::debug!(
        role = if id % 8 == 0 { "operator" } else { "customer" },
        "authorization passed"
    );
    if !cache_lookup(id, settings.io_micros).await || route != "/catalog/{id}" {
        query(id, settings).await;
    }
    // Blocking work retains its request parent across the thread-pool boundary.
    let parent = tracing::Span::current();
    let checksum = web::block(move || parent.in_scope(|| compute(id, settings)))
        .await
        .map_err(error::ErrorInternalServerError)?;
    let (status, message) = outcome(id, route);
    let result = format!("{{\"request\":{id},\"checksum\":{checksum},\"result\":\"{message}\"}}");
    tracing::Span::current()
        .record("status", status.as_u16())
        .record("result_bytes", result.len());
    tracing::info!(
        status = status.as_u16(),
        response_bytes = result.len(),
        "request completed"
    );
    Ok(HttpResponse::build(status)
        .content_type("application/json")
        .body(result))
}

#[tracing::instrument(name = "cache.get", level = "debug", skip_all, fields(shard = id % 8))]
async fn cache_lookup(id: u64, io_micros: u64) -> bool {
    let hit = id % 4 != 0;
    tracing::debug!(key = %format_args!("catalog:{}", id % 2048), hit, "cache lookup");
    actix_web::rt::time::sleep(Duration::from_micros(io_micros)).await;
    hit
}

#[tracing::instrument(name = "database.query", skip_all, fields(transaction = id, isolation = "read_committed"))]
async fn query(id: u64, settings: Workload) {
    tracing::debug!(
        statement = "SELECT sku, price, stock FROM inventory WHERE tenant = $1",
        pool = "primary",
        "database query started"
    );
    actix_web::rt::time::sleep(Duration::from_micros(settings.io_micros)).await;
    for page in 0..settings.batches {
        tracing::trace!(
            page,
            rows = 128,
            cursor = id * 128 + page,
            "database page decoded"
        );
    }
    tracing::debug!(rows = settings.batches * 128, "database query finished");
}

#[tracing::instrument(name = "compute", level = "debug", skip_all,
    fields(batches = settings.batches, rounds = settings.cpu_rounds))]
fn compute(id: u64, settings: Workload) -> u64 {
    let mut checksum = id;
    for batch in 0..settings.batches {
        for item in 0..settings.cpu_rounds {
            // Keep the mock CPU load observable even under release optimization.
            checksum = black_box(
                checksum
                    .rotate_left(7)
                    .wrapping_add(item ^ batch)
                    .wrapping_mul(6364136223846793005),
            );
        }
        // Mixed signs exercise two's-complement compression. Checksum-derived
        // signs avoid a trivial alternating cycle; keep the recorded type i64.
        let signed_unit: i64 = if checksum & (1 << 63) == 0 { 1 } else { -1 };
        tracing::trace!(
            batch,
            checksum,
            signed_unit,
            items = settings.cpu_rounds,
            "work batch transformed"
        );
        tracing::debug!(
            partition = batch % 8,
            codec = "json",
            bytes = 2048 + checksum % 4096,
            "response partition encoded"
        );
    }
    checksum
}

fn outcome(id: u64, route: &str) -> (StatusCode, &'static str) {
    // Deterministic failures exercise warning/error callsites without external services.
    if id % 101 == 0 {
        tracing::error!(
            upstream = "payment-sandbox",
            retryable = true,
            "mock upstream unavailable"
        );
        (StatusCode::SERVICE_UNAVAILABLE, "upstream unavailable")
    } else if id % 37 == 0 {
        tracing::warn!(sku = id % 2048, available = 0, "mock inventory conflict");
        (StatusCode::CONFLICT, "inventory conflict")
    } else {
        if route == "/orders" {
            tracing::info!(
                topic = "order.created",
                partition = id % 16,
                offset = id,
                "outbox message queued"
            );
        }
        (StatusCode::OK, "completed")
    }
}
