mod face;
mod transform;
mod vips;

use std::error::Error;
use std::fmt::Display;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use prometheus::{
    Encoder as _, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};
use tokio::sync::Semaphore;

use crate::transform::Params;

struct AppState {
    /// Bounds concurrent libvips pipelines so bursts queue instead of exhausting memory.
    permits: Semaphore,
    /// How long a request may wait for a permit before getting 503.
    queue_timeout: Duration,
    config: transform::Config,
    registry: Registry,
    requests: IntCounterVec,
    duration: HistogramVec,
    input_bytes: IntCounter,
    output_bytes: IntCounter,
    vips_memory: IntGauge,
}

impl AppState {
    fn new(concurrency: usize, queue_timeout: Duration, config: transform::Config) -> prometheus::Result<Self> {
        let registry = Registry::new_custom(Some("citlali".into()), None)?;
        let requests =
            IntCounterVec::new(Opts::new("requests_total", "Transform requests by HTTP status."), &["status"])?;
        let duration = HistogramVec::new(
            HistogramOpts::new(
                "transform_duration_seconds",
                "Successful transforms by output format, including queueing.",
            ),
            &["format"],
        )?;
        let input_bytes = IntCounter::new("input_bytes_total", "Bytes of successfully transformed input.")?;
        let output_bytes = IntCounter::new("output_bytes_total", "Bytes of transformed output.")?;
        let vips_memory = IntGauge::new("vips_tracked_memory_bytes", "Pixel memory currently held by libvips.")?;
        registry.register(Box::new(requests.clone()))?;
        registry.register(Box::new(duration.clone()))?;
        registry.register(Box::new(input_bytes.clone()))?;
        registry.register(Box::new(output_bytes.clone()))?;
        registry.register(Box::new(vips_memory.clone()))?;
        // CPU seconds, RSS, fds: /proc-based, so Linux only.
        #[cfg(target_os = "linux")]
        registry.register(Box::new(prometheus::process_collector::ProcessCollector::for_self()))?;
        Ok(Self {
            permits: Semaphore::new(concurrency),
            queue_timeout,
            config,
            registry,
            requests,
            duration,
            input_bytes,
            output_bytes,
            vips_memory,
        })
    }
}

fn status_of(err: &transform::Error) -> StatusCode {
    match err {
        transform::Error::UnsupportedFormat => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        transform::Error::InvalidParam(_) => StatusCode::BAD_REQUEST,
        transform::Error::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        // Almost always an undecodable input.
        transform::Error::Vips(_) => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

async fn transform(State(state): State<Arc<AppState>>, Query(params): Query<Params>, body: Bytes) -> Response {
    let started = Instant::now();
    let input_len = body.len() as u64;
    let config = state.config;
    let response = match tokio::time::timeout(state.queue_timeout, state.permits.acquire()).await {
        // Shed load instead of queueing work the client (or a proxy timeout) will give up on.
        // The semaphore is never closed, so the inner error can't happen.
        Err(_) | Ok(Err(_)) => (StatusCode::SERVICE_UNAVAILABLE, "server busy").into_response(),
        Ok(Ok(_permit)) => {
            match tokio::task::spawn_blocking(move || transform::transform(&body, &params, config)).await {
                Ok(Ok(out)) => {
                    state.input_bytes.inc_by(input_len);
                    state.output_bytes.inc_by(out.bytes.len() as u64);
                    state.duration.with_label_values(&[out.format.name()]).observe(started.elapsed().as_secs_f64());
                    ([(header::CONTENT_TYPE, out.format.mime())], out.bytes).into_response()
                }
                Ok(Err(e)) => (status_of(&e), e.to_string()).into_response(),
                Err(e) => {
                    eprintln!("transform task failed: {e}");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
    };
    state.requests.with_label_values(&[response.status().as_str()]).inc();
    response
}

async fn metrics(State(state): State<Arc<AppState>>) -> Result<Vec<u8>, StatusCode> {
    state.vips_memory.set(i64::try_from(vips::tracked_mem()).unwrap_or(i64::MAX));
    let mut buf = Vec::new();
    TextEncoder::new().encode(&state.registry.gather(), &mut buf).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(buf)
}

fn env_or<T: FromStr>(key: &str, default: T) -> Result<T, String>
where
    T::Err: Display,
{
    std::env::var(key).map_or(Ok(default), |v| v.parse().map_err(|e| format!("{key}: {e}")))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    vips::init()?;
    let listen = env_or("CITLALI_LISTEN", "0.0.0.0:8080".to_owned())?;
    let max_body = env_or("CITLALI_MAX_BODY_BYTES", 32 << 20)?;
    let default_concurrency = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let concurrency = env_or("CITLALI_CONCURRENCY", default_concurrency)?;
    // Well under common proxy timeouts (Cloudflare gives up after 100 s).
    let queue_timeout = Duration::from_secs(env_or("CITLALI_QUEUE_TIMEOUT_SECS", 30)?);
    let avif_speed = env_or("CITLALI_AVIF_SPEED", transform::DEFAULT_AVIF_SPEED)?;
    if avif_speed > transform::MAX_AVIF_SPEED {
        return Err(format!("CITLALI_AVIF_SPEED: must be 0..={}", transform::MAX_AVIF_SPEED).into());
    }
    let max_input_pixels = env_or("CITLALI_MAX_INPUT_PIXELS", transform::DEFAULT_MAX_INPUT_PIXELS)?;
    let config = transform::Config { avif_speed, max_input_pixels };

    let app = Router::new()
        .route("/transform", post(transform))
        .route("/metrics", get(metrics))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(Arc::new(AppState::new(concurrency.get(), queue_timeout, config)?));

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    println!(
        "citlali listening on {listen} (concurrency {concurrency}, queue timeout {queue_timeout:?}, \
         max body {max_body} bytes, max input {max_input_pixels} px, avif speed {avif_speed})"
    );
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

/// Resolves on SIGTERM (`docker stop`, Kubernetes) or Ctrl-C; in-flight requests then drain.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        return tokio::signal::ctrl_c().await.unwrap_or_default();
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
