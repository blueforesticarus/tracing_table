//! Finite loopback HTTP capture using the same MessagePack/LZ4 format as Bevy.
#[path = "../../support/capture.rs"]
mod capture_support;
mod driver;
mod workload;

use actix_web::{App, HttpServer, web};
use capture_support::{Result, Session, setting};
use std::net::TcpListener;
use tracing_actix_web::TracingLogger;

#[actix_web::main]
async fn main() -> Result<()> {
    // Export before initializing a renderer, server, or capture subscriber.
    if capture_support::export_callsites()? {
        return Ok(());
    }

    let load = driver::Settings::from_env()?;
    let work = workload::Workload::from_env()?;
    let workers = usize::try_from(setting("TRACING_CAPTURE_WORKERS", 4)?)?;
    let session = Session::start("actix")?;
    // Port zero avoids conflicts; no external services or public bind are needed.
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let server = HttpServer::new(move || {
        App::new()
            .wrap(TracingLogger::default())
            .app_data(web::Data::new(work))
            .route("/health", web::get().to(workload::health))
            .route("/catalog/{id}", web::get().to(workload::request))
            .route("/orders", web::post().to(workload::request))
            .route("/reports", web::post().to(workload::request))
    })
    .workers(workers)
    .disable_signals()
    .listen(listener)?
    .run();
    let handle = server.handle();
    let task = actix_web::rt::spawn(server);
    let traffic = driver::run(&session, address, &load).await;
    // Also shut down cleanly when the driver reports an HTTP error or size limit.
    handle.stop(true).await;
    task.await??;
    let traffic = traffic?;
    let fixture = session.finish()?;
    let middleware_requests = driver::validate(&fixture, traffic.requests)?;
    session.write_report(
        &fixture,
        &format!(
            "requests={}\n\
         middleware_requests={middleware_requests}\n\
         concurrency={}\nworkers={workers}\n\
         batches={}\ncpu_rounds={}\nio_micros={}\n\
         max_requests={}\nstatuses={:?}\n",
            traffic.requests,
            load.concurrency,
            work.batches,
            work.cpu_rounds,
            work.io_micros,
            load.max_requests,
            traffic.statuses
        ),
    )
}
