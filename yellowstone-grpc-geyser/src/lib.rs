//!
//! [`GeyserPlugin`] implementation for gRPC.
//!
pub mod config;
pub mod grpc;
pub mod metrics;
pub mod plugin;
pub mod version;

/// Creates a thread name for the plugin.
/// Each thread gets a unique name with an incremental ID.
/// 为插件的每个线程生成一个唯一的名称，使用一个递增的 ID。
pub fn get_thread_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static ATOMIC_ID: AtomicU64 = AtomicU64::new(0);
    let id = ATOMIC_ID.fetch_add(1, Ordering::Relaxed);
    format!("solGeyserGrpc{id:02}")
}
