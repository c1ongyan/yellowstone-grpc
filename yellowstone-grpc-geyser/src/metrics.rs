//!
//! Prometheus metrics.
//!
use {
    crate::{config::ConfigPrometheus, version::VERSION as VERSION_INFO},
    agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus as GeyserSlosStatus,
    http_body_util::{combinators::BoxBody, BodyExt, Empty as BodyEmpty, Full as BodyFull},
    hyper::{
        body::{Bytes, Incoming as BodyIncoming},
        service::service_fn,
        Request, Response, StatusCode,
    },
    hyper_util::{
        rt::tokio::{TokioExecutor, TokioIo},
        server::conn::auto::Builder as ServerBuilder,
    },
    log::{error, info},
    prometheus::{IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder},
    solana_clock::Slot,
    std::{
        collections::{hash_map::Entry as HashMapEntry, HashMap},
        convert::Infallible,
        sync::{Arc, Once},
    },
    tokio::{
        net::TcpListener,
        sync::{mpsc, oneshot, Notify},
        task::JoinHandle,
    },
    yellowstone_grpc_proto::plugin::{filter::Filter, message::SlotStatus},
};

lazy_static::lazy_static! {
    /// The global Prometheus registry.
    pub static ref REGISTRY: Registry = Registry::new();

    /// Exposes version information as a metric.
    static ref VERSION: IntCounterVec = IntCounterVec::new(
        Opts::new("version", "Plugin version info"),
        &["buildts", "git", "package", "proto", "rustc", "solana", "version"]
    ).unwrap();

    /// Tracks the latest slot number received from Geyser for each status.
    static ref SLOT_STATUS: IntGaugeVec = IntGaugeVec::new(
        Opts::new("slot_status", "Lastest received slot from Geyser"),
        &["status"]
    ).unwrap();

    /// Tracks the latest slot number processed internally by the plugin for each status.
    static ref SLOT_STATUS_PLUGIN: IntGaugeVec = IntGaugeVec::new(
        Opts::new("slot_status_plugin", "Latest processed slot in the plugin to client queues"),
        &["status"]
    ).unwrap();

    /// Counts the total number of failures to construct a full block.
    static ref INVALID_FULL_BLOCKS: IntGaugeVec = IntGaugeVec::new(
        Opts::new("invalid_full_blocks_total", "Total number of fails on constructin full blocks"),
        &["reason"]
    ).unwrap();

    /// The current size of the message queue between the plugin and the gRPC service.
    static ref MESSAGE_QUEUE_SIZE: IntGauge = IntGauge::new(
        "message_queue_size", "Size of geyser message queue"
    ).unwrap();

    /// The total number of active connections to the gRPC service.
    static ref CONNECTIONS_TOTAL: IntGauge = IntGauge::new(
        "connections_total", "Total number of connections to gRPC service"
    ).unwrap();

    /// The total number of active subscriptions, broken down by endpoint and subscription type.
    static ref SUBSCRIPTIONS_TOTAL: IntGaugeVec = IntGaugeVec::new(
        Opts::new("subscriptions_total", "Total number of subscriptions to gRPC service"),
        &["endpoint", "subscription"]
    ).unwrap();

    /// Counts the number of times a slot status was missed and had to be inferred.
    static ref MISSED_STATUS_MESSAGE: IntCounterVec = IntCounterVec::new(
        Opts::new("missed_status_message_total", "Number of missed messages by commitment"),
        &["status"]
    ).unwrap();
}

/// A message sent to the `DebugClientStatuses` actor to update client state.
#[derive(Debug)]
pub enum DebugClientMessage {
    UpdateFilter { id: usize, filter: Box<Filter> },
    UpdateSlot { id: usize, slot: Slot },
    Removed { id: usize },
}

impl DebugClientMessage {
    /// A helper to send a debug message only if the sender exists.
    pub fn maybe_send(tx: &Option<mpsc::UnboundedSender<Self>>, get_msg: impl FnOnce() -> Self) {
        if let Some(tx) = tx {
            let _ = tx.send(get_msg());
        }
    }
}

/// Holds the live status (filter and last processed slot) for a single client.
#[derive(Debug)]
struct DebugClientStatus {
    filter: Box<Filter>,
    processed_slot: Slot,
}

/// An actor-like service to track the status of all connected clients for debugging purposes.
#[derive(Debug)]
struct DebugClientStatuses {
    requests_tx: mpsc::UnboundedSender<oneshot::Sender<String>>,
    jh: JoinHandle<()>,
}

impl Drop for DebugClientStatuses {
    fn drop(&mut self) {
        self.jh.abort();
    }
}

impl DebugClientStatuses {
    /// Creates a new `DebugClientStatuses` service and spawns its background task.
    fn new(clients_rx: mpsc::UnboundedReceiver<DebugClientMessage>) -> Arc<Self> {
        let (requests_tx, requests_rx) = mpsc::unbounded_channel();
        let jh = tokio::spawn(Self::run(clients_rx, requests_rx));
        Arc::new(Self { requests_tx, jh })
    }

    /// The main loop for the `DebugClientStatuses` actor.
    /// It listens for updates and requests to dump the current state.
    async fn run(
        mut clients_rx: mpsc::UnboundedReceiver<DebugClientMessage>,
        mut requests_rx: mpsc::UnboundedReceiver<oneshot::Sender<String>>,
    ) {
        let mut clients = HashMap::<usize, DebugClientStatus>::new();
        loop {
            tokio::select! {
                Some(message) = clients_rx.recv() => match message {
                    DebugClientMessage::UpdateFilter { id, filter } => {
                        match clients.entry(id) {
                            HashMapEntry::Occupied(mut entry) => {
                                entry.get_mut().filter = filter;
                            }
                            HashMapEntry::Vacant(entry) => {
                                entry.insert(DebugClientStatus {
                                    filter,
                                    processed_slot: 0,
                                });
                            }
                        }
                    }
                    DebugClientMessage::UpdateSlot { id, slot } => {
                        if let Some(status) = clients.get_mut(&id) {
                            status.processed_slot = slot;
                        }
                    }
                    DebugClientMessage::Removed { id } => {
                        clients.remove(&id);
                    }
                },
                Some(tx) = requests_rx.recv() => {
                    let mut statuses: Vec<(usize, String)> = clients.iter().map(|(id, status)| {
                        (*id, format!("client#{id:06}, {}, {:?}", status.processed_slot, status.filter))
                    }).collect();
                    statuses.sort();

                    let mut status = statuses.into_iter().fold(String::new(), |mut acc: String, (_id, status)| {
                        if !acc.is_empty() {
                            acc += "\n";
                        }
                        acc + &status
                    });
                    if !status.is_empty() {
                        status += "\n";
                    }

                    let _ = tx.send(status);
                },
            }
        }
    }

    /// Sends a request to the actor to get the current statuses as a formatted string.
    async fn get_statuses(&self) -> anyhow::Result<String> {
        let (tx, rx) = oneshot::channel();
        self.requests_tx
            .send(tx)
            .map_err(|_error| anyhow::anyhow!("failed to send request"))?;
        rx.await
            .map_err(|_error| anyhow::anyhow!("failed to wait response"))
    }
}

/// The service that runs the Prometheus HTTP server.
#[derive(Debug)]
pub struct PrometheusService {
    debug_clients_statuses: Option<Arc<DebugClientStatuses>>,
    shutdown: Arc<Notify>,
}

impl PrometheusService {
    /// Creates a new `PrometheusService`.
    /// This function registers all metrics and starts the HTTP server if configured.
    pub async fn new(
        config: Option<ConfigPrometheus>,
        debug_clients_rx: Option<mpsc::UnboundedReceiver<DebugClientMessage>>,
    ) -> std::io::Result<Self> {
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
            macro_rules! register {
                ($collector:ident) => {
                    REGISTRY
                        .register(Box::new($collector.clone()))
                        .expect("collector can't be registered");
                };
            }
            register!(VERSION);
            register!(SLOT_STATUS);
            register!(SLOT_STATUS_PLUGIN);
            register!(INVALID_FULL_BLOCKS);
            register!(MESSAGE_QUEUE_SIZE);
            register!(CONNECTIONS_TOTAL);
            register!(SUBSCRIPTIONS_TOTAL);
            register!(MISSED_STATUS_MESSAGE);

            VERSION
                .with_label_values(&[
                    VERSION_INFO.buildts,
                    VERSION_INFO.git,
                    VERSION_INFO.package,
                    VERSION_INFO.proto,
                    VERSION_INFO.rustc,
                    VERSION_INFO.solana,
                    VERSION_INFO.version,
                ])
                .inc();
        });

        let shutdown = Arc::new(Notify::new());
        let mut debug_clients_statuses = None;
        if let Some(ConfigPrometheus { address }) = config {
            if let Some(debug_clients_rx) = debug_clients_rx {
                debug_clients_statuses = Some(DebugClientStatuses::new(debug_clients_rx));
            }
            let debug_clients_statuses2 = debug_clients_statuses.clone();

            let shutdown = Arc::clone(&shutdown);
            let listener = TcpListener::bind(&address).await?;
            info!("start prometheus server: {address}");
            tokio::spawn(async move {
                loop {
                    let stream = tokio::select! {
                        () = shutdown.notified() => break,
                        maybe_conn = listener.accept() => {
                            match maybe_conn {
                                Ok((stream, _addr)) => stream,
                                Err(error) => {
                                    error!("failed to accept new connection: {error}");
                                    break;
                                }
                            }
                        }
                    };
                    let debug_clients_statuses = debug_clients_statuses2.clone();
                    tokio::spawn(async move {
                        if let Err(error) = ServerBuilder::new(TokioExecutor::new())
                            .serve_connection(
                                TokioIo::new(stream),
                                service_fn(move |req: Request<BodyIncoming>| {
                                    let debug_clients_statuses = debug_clients_statuses.clone();
                                    async move {
                                        match req.uri().path() {
                                            "/metrics" => metrics_handler(),
                                            "/debug_clients" => {
                                                if let Some(debug_clients_statuses) =
                                                    &debug_clients_statuses
                                                {
                                                    let (status, body) =
                                                        match debug_clients_statuses
                                                            .get_statuses()
                                                            .await
                                                        {
                                                            Ok(body) => (StatusCode::OK, body),
                                                            Err(error) => (
                                                                StatusCode::INTERNAL_SERVER_ERROR,
                                                                error.to_string(),
                                                            ),
                                                        };
                                                    Response::builder().status(status).body(
                                                        BodyFull::new(Bytes::from(body)).boxed(),
                                                    )
                                                } else {
                                                    not_found_handler()
                                                }
                                            }
                                            _ => not_found_handler(),
                                        }
                                    }
                                }),
                            )
                            .await
                        {
                            error!("failed to handle request: {error}");
                        }
                    });
                }
            });
        }

        Ok(PrometheusService {
            debug_clients_statuses,
            shutdown,
        })
    }

    /// Shuts down the Prometheus server.
    pub fn shutdown(self) {
        drop(self.debug_clients_statuses);
        self.shutdown.notify_one();
    }
}

/// Handles requests to the `/metrics` endpoint.
fn metrics_handler() -> http::Result<Response<BoxBody<Bytes, Infallible>>> {
    let metrics = TextEncoder::new()
        .encode_to_string(&REGISTRY.gather())
        .unwrap_or_else(|error| {
            error!("could not encode custom metrics: {}", error);
            String::new()
        });
    Response::builder()
        .status(StatusCode::OK)
        .body(BodyFull::new(Bytes::from(metrics)).boxed())
}

/// Handles requests to unknown endpoints.
fn not_found_handler() -> http::Result<Response<BoxBody<Bytes, Infallible>>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(BodyEmpty::new().boxed())
}

/// Updates the `slot_status` metric.
pub fn update_slot_status(status: &GeyserSlosStatus, slot: u64) {
    SLOT_STATUS
        .with_label_values(&[status.as_str()])
        .set(slot as i64);
}

/// Updates the `slot_status_plugin` metric.
pub fn update_slot_plugin_status(status: SlotStatus, slot: u64) {
    SLOT_STATUS_PLUGIN
        .with_label_values(&[status.as_str()])
        .set(slot as i64);
}

/// Increments the `invalid_full_blocks_total` metric.
pub fn update_invalid_blocks(reason: impl AsRef<str>) {
    INVALID_FULL_BLOCKS
        .with_label_values(&[reason.as_ref()])
        .inc();
    INVALID_FULL_BLOCKS.with_label_values(&["all"]).inc();
}

/// Increments the `message_queue_size` metric.
pub fn message_queue_size_inc() {
    MESSAGE_QUEUE_SIZE.inc()
}

/// Decrements the `message_queue_size` metric.
pub fn message_queue_size_dec() {
    MESSAGE_QUEUE_SIZE.dec()
}

/// Increments the `connections_total` metric.
pub fn connections_total_inc() {
    CONNECTIONS_TOTAL.inc()
}

/// Decrements the `connections_total` metric.
pub fn connections_total_dec() {
    CONNECTIONS_TOTAL.dec()
}

/// Updates the `subscriptions_total` metric when a filter changes.
pub fn update_subscriptions(endpoint: &str, old: Option<&Filter>, new: Option<&Filter>) {
    for (multiplier, filter) in [(-1, old), (1, new)] {
        if let Some(filter) = filter {
            SUBSCRIPTIONS_TOTAL
                .with_label_values(&[endpoint, "grpc_total"])
                .add(multiplier);

            for (name, value) in filter.get_metrics() {
                SUBSCRIPTIONS_TOTAL
                    .with_label_values(&[endpoint, name])
                    .add((value as i64) * multiplier);
            }
        }
    }
}

/// Increments the `missed_status_message_total` metric.
pub fn missed_status_message_inc(status: SlotStatus) {
    MISSED_STATUS_MESSAGE
        .with_label_values(&[status.as_str()])
        .inc()
}
