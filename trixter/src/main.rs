use std::{error::Error, fmt, io, net::SocketAddr, sync::Arc, time};

use ahash::RandomState;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
};
use clap::Parser;
use dashmap::DashMap;
use nanoid::nanoid;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    signal,
    sync::oneshot,
    time::timeout,
};
use tokio_netem::{
    corrupter::Corrupter,
    delayer::{Duration, DynamicDuration},
    io::{NetEmWriteExt, ResetLinger},
    probability::{DynamicProbability, Probability},
    shutdowner::Shutdowner,
    slicer::{DynamicSize, Size},
    terminator::{TerminatedError, Terminator},
    throttler::{DynamicRate, Rate},
};

use tracing::{Instrument, error, info};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Config {
    /// Address to listen on, e.g. 0.0.0.0:8080
    #[arg(short = 'l', long)]
    listen: SocketAddr,

    /// Address to connect upstream to, e.g. 127.0.0.1:8181
    #[arg(short = 'c', long)]
    upstream: SocketAddr,

    /// Address to listen for API, e.g. 127.0.0.1:8888
    #[arg(short = 'a', long)]
    api: SocketAddr,

    /// Optional latency delay in milliseconds
    #[arg(long, value_name = "ms", default_value_t = 0)]
    delay_ms: u64,

    /// Optional bandwidth throttle in bytes per second
    #[arg(long, value_name = "bytes/s", default_value_t = 0)]
    throttle_rate_bytes: usize,

    /// Optional slice write data into smaller chunks, set max size in bytes
    #[arg(long, value_name = "bytes", default_value_t = 0)]
    slice_size_bytes: usize,

    /// Probability [0.0..=1.0] to terminate on each read/write operation
    #[arg(long, value_name = "0..1", default_value_t = 0.0)]
    terminate_probability_rate: f64,

    /// Probability [0.0..=1.0] to data corrupt on each read/write operation
    #[arg(long, value_name = "0..1", default_value_t = 0.0)]
    corrupt_probability_rate: f64,

    /// Timeout for a proxy connection
    #[arg(long, value_name = "ms", default_value_t = 0)]
    connection_duration_ms: u64,

    /// Optional seed used for deterministic randomization
    #[arg(long)]
    random_seed: Option<u64>,
}

type ConnId = String;

impl Config {
    fn validate(&self) -> anyhow::Result<()> {
        if !(0.0..=1.0).contains(&self.terminate_probability_rate) {
            anyhow::bail!(
                "--terminate-probability-rate must be between 0.0 and 1.0 (got {})",
                self.terminate_probability_rate
            );
        }

        if !(0.0..=1.0).contains(&self.corrupt_probability_rate) {
            anyhow::bail!(
                "--corrupt-probability-rate must be between 0.0 and 1.0 (got {})",
                self.corrupt_probability_rate
            );
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
struct ConnectionInfo {
    id: ConnId,
    downstream: SocketAddr,
    upstream: SocketAddr,
}

#[derive(Debug, Clone, Serialize)]
struct ConnectionStatus {
    conn_info: ConnectionInfo,
    delay: time::Duration,
    throttle_rate: usize,
    slice_size: usize,
    terminate_probability_rate: f64,
    corrupt_probability_rate: f64,
}

#[derive(Debug)]
struct ConnectionState {
    conn_info: ConnectionInfo,

    delay: Arc<DynamicDuration>,
    throttle_rate: Arc<DynamicRate>,
    slice_size: Arc<DynamicSize>,
    terminate_probability: Arc<DynamicProbability>,
    corrupt_probability: Arc<DynamicProbability>,
    tx: oneshot::Sender<Box<dyn Error + Send + Sync>>,
}

#[derive(Clone, Default)]
struct ApiState {
    connections: Arc<DashMap<ConnId, ConnectionState, RandomState>>,
}

impl ApiState {
    fn all(&self) -> Vec<ConnectionStatus> {
        self.connections
            .iter()
            .map(|c| ConnectionStatus {
                conn_info: c.conn_info.clone(),
                delay: c.delay.duration(),
                throttle_rate: c.throttle_rate.rate(),
                slice_size: c.slice_size.size(),
                terminate_probability_rate: c.terminate_probability.probability(),
                corrupt_probability_rate: c.corrupt_probability.probability(),
            })
            .collect()
    }

    fn remove_connection(&self, id: &str) -> Result<ConnectionState, ApiError> {
        self.connections
            .remove(id)
            .ok_or(ApiError::NotFound)
            .map(|c| c.1)
    }

    fn remove_all(&self) -> Vec<ConnectionState> {
        let ids: Vec<ConnId> = self
            .connections
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        ids.into_iter()
            .filter_map(|id| self.connections.remove(&id).map(|(_, state)| state))
            .collect()
    }

    fn set_delay(&self, id: &str, delay: time::Duration) -> Result<(), ApiError> {
        let conn = self.connections.get_mut(id).ok_or(ApiError::NotFound)?;
        conn.delay.set(delay);
        Ok(())
    }

    fn set_throttle_rate(&self, id: &str, rate: usize) -> Result<(), ApiError> {
        let conn = self.connections.get_mut(id).ok_or(ApiError::NotFound)?;
        conn.throttle_rate.set(rate);
        Ok(())
    }

    fn set_slice_size(&self, id: &str, size: usize) -> Result<(), ApiError> {
        let conn = self.connections.get_mut(id).ok_or(ApiError::NotFound)?;
        conn.slice_size.set(size);
        Ok(())
    }

    fn set_terminate_probability_rate(&self, id: &str, rate: f64) -> Result<(), ApiError> {
        let conn = self.connections.get_mut(id).ok_or(ApiError::NotFound)?;
        conn.terminate_probability
            .set(rate)
            .map_err(|_| ApiError::BadProbability)
    }

    fn set_corrupt_probability_rate(&self, id: &str, rate: f64) -> Result<(), ApiError> {
        let conn = self.connections.get_mut(id).ok_or(ApiError::NotFound)?;
        conn.corrupt_probability
            .set(rate)
            .map_err(|_| ApiError::BadProbability)
    }
}

#[derive(Debug, ThisError)]
pub enum ApiError {
    #[error("connection not found")]
    NotFound,
    #[error("invalid probability; must be between 0.0 and 1.0")]
    BadProbability,
    #[error("internal error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadProbability => StatusCode::BAD_REQUEST,
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = serde_json::json!({ "error": self.to_string() });
        (status, Json(body)).into_response()
    }
}

async fn list_connections(
    State(state): State<ApiState>,
) -> Result<Json<Vec<ConnectionStatus>>, ApiError> {
    Ok(Json(state.all()))
}

async fn set_delay(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<DelayReq>,
) -> Result<StatusCode, ApiError> {
    let duration = time::Duration::from_millis(req.delay_ms);
    state.set_delay(&id, duration)?;

    Ok(StatusCode::ACCEPTED)
}

async fn set_throttle(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ThrottleReq>,
) -> Result<StatusCode, ApiError> {
    state.set_throttle_rate(&id, req.rate_bytes)?;
    Ok(StatusCode::ACCEPTED)
}

async fn set_slice(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<SliceReq>,
) -> Result<StatusCode, ApiError> {
    state.set_slice_size(&id, req.size_bytes)?;
    Ok(StatusCode::ACCEPTED)
}

async fn set_termination(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<TerminationReq>,
) -> Result<StatusCode, ApiError> {
    state.set_terminate_probability_rate(&id, req.probability_rate)?;
    Ok(StatusCode::ACCEPTED)
}

async fn set_corruption(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<CorruptionReq>,
) -> Result<StatusCode, ApiError> {
    state.set_corrupt_probability_rate(&id, req.probability_rate)?;
    Ok(StatusCode::ACCEPTED)
}

async fn shutdown_connection(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(req): Json<ShutdownReq>,
) -> Result<StatusCode, ApiError> {
    let conn = state.remove_connection(&id)?;
    conn.tx
        .send(Box::new(io::Error::other(req.reason)))
        .map_err(|_| ApiError::Internal)?;

    Ok(StatusCode::ACCEPTED)
}
async fn shutdown_all_connections(
    State(state): State<ApiState>,
    Json(req): Json<ShutdownReq>,
) -> Result<StatusCode, ApiError> {
    let connections = state.remove_all();
    let mut send_failed = false;

    for conn in connections {
        if conn
            .tx
            .send(Box::new(io::Error::other(req.reason.clone())))
            .is_err()
        {
            send_failed = true;
        }
    }

    if send_failed {
        Err(ApiError::Internal)
    } else {
        Ok(StatusCode::ACCEPTED)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownReq {
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayReq {
    pub delay_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThrottleReq {
    pub rate_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceReq {
    pub size_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminationReq {
    pub probability_rate: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorruptionReq {
    pub probability_rate: f64,
}

async fn handle_connection(
    downstream: TcpStream,
    addr: SocketAddr,
    config: Arc<Config>,
    connections: Arc<DashMap<ConnId, ConnectionState, RandomState>>,
) {
    let upstream = match TcpStream::connect(config.upstream).await {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "Failed to connect upstream {}", config.upstream.clone());
            return;
        }
    };

    // best effort
    _ = downstream.set_nodelay(true);
    _ = upstream.set_nodelay(true);

    info!("connected {addr} → {}", config.upstream.clone());

    let delay = DynamicDuration::new(time::Duration::from_millis(config.delay_ms));
    let throttle_rate = DynamicRate::new(config.throttle_rate_bytes);
    let slice_size = DynamicSize::new(config.slice_size_bytes);
    let terminate_probability_rate = DynamicProbability::new(config.terminate_probability_rate)
        .expect("probability validated during config parsing");
    let corrupt_probability_rate = DynamicProbability::new(config.corrupt_probability_rate)
        .expect("probability validated during config parsing");

    let (tx, rx) = oneshot::channel();

    let seed = config
        .random_seed
        .expect("random seed initialized during startup");
    let mut base_rng = SmallRng::seed_from_u64(seed);

    let downstream = downstream
        .delay_writes_dyn(delay.clone())
        .throttle_writes_dyn(throttle_rate.clone())
        .slice_writes_dyn(slice_size.clone());
    let downstream = Terminator::from_rng(
        downstream,
        terminate_probability_rate.clone(),
        &mut base_rng,
    );
    let downstream =
        Corrupter::from_rng(downstream, corrupt_probability_rate.clone(), &mut base_rng);
    let mut downstream = Shutdowner::new(downstream, rx);

    let upstream = upstream
        .delay_writes_dyn(delay.clone())
        .throttle_writes_dyn(throttle_rate.clone())
        .slice_writes_dyn(slice_size.clone());
    let upstream =
        Terminator::from_rng(upstream, terminate_probability_rate.clone(), &mut base_rng);
    let mut upstream =
        Corrupter::from_rng(upstream, corrupt_probability_rate.clone(), &mut base_rng);

    let id = nanoid!();

    connections.insert(
        id.clone(),
        ConnectionState {
            conn_info: ConnectionInfo {
                id: id.clone(),
                downstream: addr,
                upstream: config.upstream,
            },
            tx,
            delay,
            throttle_rate,
            slice_size,
            terminate_probability: terminate_probability_rate,
            corrupt_probability: corrupt_probability_rate,
        },
    );

    let span = tracing::info_span!("conn", %id, client=%addr, upstream=%config.upstream);
    async move {
        let fut = copy_bidirectional(&mut downstream, &mut upstream);

        let timeout_duration = if config.connection_duration_ms > 0 {
            Some(time::Duration::from_millis(config.connection_duration_ms))
        } else {
            None
        };

        let res: io::Result<(u64, u64)> = match timeout_duration {
            Some(timeout_duration) => match timeout(timeout_duration, fut).await {
                Ok(res) => res,
                Err(_) => Err(io::Error::other(TimeoutError)),
            },
            None => fut.await,
        };

        // clean up
        connections.remove(&id);

        if let Err(err) = res {
            error!(error = %err, "proxy error {} -> {}", addr, config.upstream);

            if let Some(e) = err.get_ref()
                && (e.downcast_ref::<TerminatedError>().is_some()
                    || e.downcast_ref::<TimeoutError>().is_some())
            {
                // best effort
                _ = downstream.set_reset_linger();
                _ = upstream.set_reset_linger();
            }
        }
    }
    .instrument(span)
    .await
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let mut cfg = Config::parse();
    cfg.validate()?;

    if cfg.random_seed.is_none() {
        let seed = rand::random::<u64>();
        cfg.random_seed = Some(seed);
    }
    info!(
        "random seed: {}",
        cfg.random_seed
            .expect("random seed initialized during startup")
    );

    let config = Arc::new(cfg);

    let connections = Arc::new(DashMap::with_hasher(RandomState::new()));

    let api_state = ApiState {
        connections: connections.clone(),
    };

    let api_addr = config.api;

    tokio::spawn(async move {
        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/connections", get(list_connections))
            .route("/connections/{id}/shutdown", post(shutdown_connection))
            .route("/connections/{id}/delay", patch(set_delay))
            .route("/connections/{id}/throttle", patch(set_throttle))
            .route("/connections/{id}/slice", patch(set_slice))
            .route("/connections/{id}/termination", patch(set_termination))
            .route("/connections/{id}/corruption", patch(set_corruption))
            .route("/connections/_all/shutdown", post(shutdown_all_connections))
            .with_state(api_state);

        if let Err(err) =
            axum::serve(tokio::net::TcpListener::bind(api_addr).await.unwrap(), app).await
        {
            panic!("server error: {err}");
        }
    });

    // Proxy
    let listener = TcpListener::bind(config.listen).await?;
    info!(listen = %config.listen, connect = %config.upstream, api=%api_addr, "Listening");

    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, addr)) => {
                    tokio::spawn(handle_connection(
                        stream,
                        addr,
                        config.clone(),
                        connections.clone(),
                    ));
                }
                Err(e) => {
                    error!(%e, "accept failed");
                }
            },
            _ = signal::ctrl_c() => {
                info!("Received Ctrl-C, shutting down");
                break;
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct TimeoutError;

impl fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TimeoutError")
    }
}

impl Error for TimeoutError {}
