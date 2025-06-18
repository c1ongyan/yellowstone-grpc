//!
//! Plugin configuration.
//!
use {
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPluginError, Result as PluginResult,
    },
    serde::{de, Deserialize, Deserializer},
    std::{
        collections::HashSet, fmt, fs::read_to_string, net::SocketAddr, path::Path, str::FromStr,
        time::Duration,
    },
    tokio::sync::Semaphore,
    tonic::codec::CompressionEncoding,
    yellowstone_grpc_proto::plugin::filter::limits::FilterLimits,
};

/// The main configuration for the plugin.
/// It is loaded from a JSON file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Path to the plugin's shared library.
    pub libpath: String,
    /// Logging configuration.
    #[serde(default)]
    pub log: ConfigLog,
    /// Tokio runtime configuration.
    #[serde(default)]
    pub tokio: ConfigTokio,
    /// gRPC service configuration.
    pub grpc: ConfigGrpc,
    /// Prometheus metrics endpoint configuration.
    #[serde(default)]
    pub prometheus: Option<ConfigPrometheus>,
    /// Collect client filters, processed slot and make it available on prometheus port `/debug_clients`
    #[serde(default)]
    pub debug_clients_http: bool,
}

impl Config {
    /// Loads the configuration from a string.
    fn load_from_str(config: &str) -> PluginResult<Self> {
        serde_json::from_str(config).map_err(|error| GeyserPluginError::ConfigFileReadError {
            msg: error.to_string(),
        })
    }

    /// Loads the configuration from a file.
    pub fn load_from_file<P: AsRef<Path>>(file: P) -> PluginResult<Self> {
        let config = read_to_string(file).map_err(GeyserPluginError::ConfigFileOpenError)?;
        Self::load_from_str(&config)
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigLog {
    /// Log level.
    #[serde(default = "ConfigLog::default_level")]
    pub level: String,
}

impl Default for ConfigLog {
    fn default() -> Self {
        Self {
            level: Self::default_level(),
        }
    }
}

impl ConfigLog {
    fn default_level() -> String {
        "info".to_owned()
    }
}

/// Tokio runtime configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigTokio {
    /// Number of worker threads in Tokio runtime.
    pub worker_threads: Option<usize>,
    /// Threads affinity. A list of CPU cores to which the Tokio worker threads will be bound.
    /// Can be specified as a comma-separated list of cores and ranges (e.g., "0,1,2-4").
    #[serde(deserialize_with = "ConfigTokio::deserialize_affinity")]
    pub affinity: Option<Vec<usize>>,
}

impl ConfigTokio {
    /// Custom deserializer for the `affinity` field.
    fn deserialize_affinity<'de, D>(deserializer: D) -> Result<Option<Vec<usize>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<&str>::deserialize(deserializer)? {
            Some(taskset) => parse_taskset(taskset).map(Some).map_err(de::Error::custom),
            None => Ok(None),
        }
    }
}

/// Parses a taskset string into a vector of core IDs.
/// For example, "0,1,5-7" will be parsed into `[0, 1, 5, 6, 7]`.
/// 将类似 "0,1,2-4" 的字符串解析为用于线程亲和性的 CPU 核心 ID 列表。
fn parse_taskset(taskset: &str) -> Result<Vec<usize>, String> {
    let mut set = HashSet::new();
    for taskset2 in taskset.split(',') {
        match taskset2.split_once('-') {
            Some((start, end)) => {
                let start: usize = start
                    .parse()
                    .map_err(|_error| format!("failed to parse {start:?} from {taskset:?}"))?;
                let end: usize = end
                    .parse()
                    .map_err(|_error| format!("failed to parse {end:?} from {taskset:?}"))?;
                if start > end {
                    return Err(format!("invalid interval {taskset2:?} in {taskset:?}"));
                }
                for idx in start..=end {
                    set.insert(idx);
                }
            }
            None => {
                set.insert(
                    taskset2.parse().map_err(|_error| {
                        format!("failed to parse {taskset2:?} from {taskset:?}")
                    })?,
                );
            }
        }
    }

    let mut vec = set.into_iter().collect::<Vec<usize>>();
    vec.sort();

    if let Some(set_max_index) = vec.last().copied() {
        let max_index = affinity::get_thread_affinity()
            .map_err(|_err| "failed to get affinity".to_owned())?
            .into_iter()
            .max()
            .unwrap_or(0);

        if set_max_index > max_index {
            return Err(format!("core index must be in the range [0, {max_index}]"));
        }
    }

    Ok(vec)
}

/// gRPC service configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGrpc {
    /// Address of Grpc service.
    pub address: SocketAddr,
    /// TLS configuration for the gRPC server.
    pub tls_config: Option<ConfigGrpcServerTls>,
    /// Possible compression options
    #[serde(default)]
    pub compression: ConfigGrpcCompression,
    /// Limits the maximum size of a decoded message, default is 4MiB
    #[serde(
        default = "ConfigGrpc::max_decoding_message_size_default",
        deserialize_with = "deserialize_int_str"
    )]
    pub max_decoding_message_size: usize,
    /// Capacity of the channel used for accounts from snapshot,
    /// on reaching the limit Sender block validator startup.
    #[serde(
        default = "ConfigGrpc::snapshot_plugin_channel_capacity_default",
        deserialize_with = "deserialize_int_str_maybe"
    )]
    pub snapshot_plugin_channel_capacity: Option<usize>,
    /// Capacity of the client channel, applicable only with snapshot
    #[serde(
        default = "ConfigGrpc::snapshot_client_channel_capacity_default",
        deserialize_with = "deserialize_int_str"
    )]
    pub snapshot_client_channel_capacity: usize,
    /// Capacity of the channel per connection
    #[serde(
        default = "ConfigGrpc::channel_capacity_default",
        deserialize_with = "deserialize_int_str"
    )]
    pub channel_capacity: usize,
    /// Concurrency limit for unary requests
    #[serde(
        default = "ConfigGrpc::unary_concurrency_limit_default",
        deserialize_with = "deserialize_int_str"
    )]
    pub unary_concurrency_limit: usize,
    /// Enable/disable unary methods
    #[serde(default)]
    pub unary_disabled: bool,
    /// Limits for possible filters
    #[serde(default, alias = "filters")]
    pub filter_limits: FilterLimits,
    /// x_token to enforce on connections
    pub x_token: Option<String>,
    /// Filter name size limit
    #[serde(default = "ConfigGrpc::default_filter_name_size_limit")]
    pub filter_name_size_limit: usize,
    /// Number of cached filter names before doing cleanup
    #[serde(default = "ConfigGrpc::default_filter_names_size_limit")]
    pub filter_names_size_limit: usize,
    /// Cleanup interval once filter names reached `filter_names_size_limit`
    #[serde(
        default = "ConfigGrpc::default_filter_names_cleanup_interval",
        with = "humantime_serde"
    )]
    pub filter_names_cleanup_interval: Duration,
    /// Number of slots stored for re-broadcast (replay)
    #[serde(
        default = "ConfigGrpc::default_replay_stored_slots",
        deserialize_with = "deserialize_int_str"
    )]
    pub replay_stored_slots: u64,
    /// Enables or disables the HTTP/2 adaptive window size.
    #[serde(default)]
    pub server_http2_adaptive_window: Option<bool>,
    /// The interval for sending HTTP/2 keepalive pings.
    #[serde(default, with = "humantime_serde")]
    pub server_http2_keepalive_interval: Option<Duration>,
    /// The timeout for receiving an acknowledgement of a keepalive ping.
    #[serde(default, with = "humantime_serde")]
    pub server_http2_keepalive_timeout: Option<Duration>,
    /// The initial connection-level window size for HTTP/2.
    #[serde(default)]
    pub server_initial_connection_window_size: Option<u32>,
    /// The initial stream-level window size for HTTP/2.
    #[serde(default)]
    pub server_initial_stream_window_size: Option<u32>,
}

impl ConfigGrpc {
    const fn max_decoding_message_size_default() -> usize {
        4 * 1024 * 1024
    }

    const fn snapshot_plugin_channel_capacity_default() -> Option<usize> {
        None
    }

    const fn snapshot_client_channel_capacity_default() -> usize {
        50_000_000
    }

    const fn channel_capacity_default() -> usize {
        250_000
    }

    const fn unary_concurrency_limit_default() -> usize {
        Semaphore::MAX_PERMITS
    }

    const fn default_filter_name_size_limit() -> usize {
        128
    }

    const fn default_filter_names_size_limit() -> usize {
        4_096
    }

    const fn default_filter_names_cleanup_interval() -> Duration {
        Duration::from_secs(1)
    }

    const fn default_replay_stored_slots() -> u64 {
        0
    }
}

/// TLS configuration for the gRPC server.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGrpcServerTls {
    /// Path to the server's certificate file.
    pub cert_path: String,
    /// Path to the server's private key file.
    pub key_path: String,
}

/// gRPC compression configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGrpcCompression {
    /// A list of compression encodings that the server will accept from clients.
    #[serde(
        deserialize_with = "ConfigGrpcCompression::deserialize_compression",
        default = "ConfigGrpcCompression::default_compression"
    )]
    pub accept: Vec<CompressionEncoding>,
    /// A list of compression encodings that the server will use to send messages.
    #[serde(
        deserialize_with = "ConfigGrpcCompression::deserialize_compression",
        default = "ConfigGrpcCompression::default_compression"
    )]
    pub send: Vec<CompressionEncoding>,
}

impl Default for ConfigGrpcCompression {
    fn default() -> Self {
        Self {
            accept: Self::default_compression(),
            send: Self::default_compression(),
        }
    }
}

impl ConfigGrpcCompression {
    /// Custom deserializer for compression settings.
    fn deserialize_compression<'de, D>(
        deserializer: D,
    ) -> Result<Vec<CompressionEncoding>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<&str>::deserialize(deserializer)?
            .into_iter()
            .map(|value| match value {
                "gzip" => Ok(CompressionEncoding::Gzip),
                "zstd" => Ok(CompressionEncoding::Zstd),
                value => Err(de::Error::custom(format!(
                    "Unknown compression format: {value}"
                ))),
            })
            .collect::<Result<_, _>>()
    }

    /// Default compression settings (Gzip).
    fn default_compression() -> Vec<CompressionEncoding> {
        vec![CompressionEncoding::Gzip, CompressionEncoding::Zstd]
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPrometheus {
    /// Address of Prometheus service.
    pub address: SocketAddr,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ValueIntStr<'a, T> {
    Int(T),
    Str(&'a str),
}

fn deserialize_int_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + FromStr,
    <T as FromStr>::Err: fmt::Display,
{
    match ValueIntStr::<T>::deserialize(deserializer)? {
        ValueIntStr::Int(value) => Ok(value),
        ValueIntStr::Str(value) => value
            .replace('_', "")
            .parse::<T>()
            .map_err(de::Error::custom),
    }
}

/// Deserializes a value that can be an integer or a string into an `Option` of a numeric type.
fn deserialize_int_str_maybe<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + FromStr,
    <T as FromStr>::Err: fmt::Display,
{
    match Option::<ValueIntStr<T>>::deserialize(deserializer)? {
        Some(ValueIntStr::Int(value)) => Ok(Some(value)),
        Some(ValueIntStr::Str(value)) => value
            .replace('_', "")
            .parse::<T>()
            .map(Some)
            .map_err(de::Error::custom),
        None => Ok(None),
    }
}
