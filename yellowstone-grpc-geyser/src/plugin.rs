//!
//! [`GeyserPlugin`] implementation.
//!
use {
    crate::{
        config::Config,
        grpc::GrpcService,
        metrics::{self, PrometheusService},
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, GeyserPluginError, ReplicaAccountInfoVersions, ReplicaBlockInfoVersions,
        ReplicaEntryInfoVersions, ReplicaTransactionInfoVersions, Result as PluginResult,
        SlotStatus,
    },
    std::{
        concat, env,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    },
    tokio::{
        runtime::{Builder, Runtime},
        sync::{mpsc, Notify},
    },
    yellowstone_grpc_proto::plugin::message::{
        Message, MessageAccount, MessageBlockMeta, MessageEntry, MessageSlot, MessageTransaction,
    },
};

/// This is the main object for the plugin.
/// It contains the Tokio runtime and communication channels for the gRPC service.
/// 这是插件的主要对象。它包含 Tokio 运行时和 gRPC 服务的通信通道。
#[derive(Debug)]
pub struct PluginInner {
    /// Tokio runtime to run the gRPC service.
    runtime: Runtime,
    /// A channel to send accounts data during the startup snapshot.
    /// This channel is blocking to apply backpressure to the validator,
    /// preventing it from overwhelming the plugin with startup data.
    snapshot_channel: Mutex<Option<crossbeam_channel::Sender<Box<Message>>>>,
    /// A flag to indicate that the snapshot channel is closed.
    /// Used to prevent logging multiple errors if the channel is already closed.
    snapshot_channel_closed: AtomicBool,
    /// A channel to send notifications to the gRPC service.
    /// This channel is non-blocking, and if it's full, messages will be dropped.
    grpc_channel: mpsc::UnboundedSender<Message>,
    /// A notification handle to shut down the gRPC service.
    grpc_shutdown: Arc<Notify>,
    /// The Prometheus service for metrics.
    prometheus: PrometheusService,
}

impl PluginInner {
    /// Sends a message to the gRPC service.
    /// If the send is successful, it increments the message queue size metric.
    /// 发送消息到 gRPC 服务。如果发送成功，它会增加消息队列大小的指标。
    fn send_message(&self, message: Message) {
        if self.grpc_channel.send(message).is_ok() {
            metrics::message_queue_size_inc();
        }
    }
}

/// The main plugin struct, which wraps the inner state in an `Option`.
/// This allows for safe initialization and destruction of the plugin's state.
/// 这是插件的主要结构体。它包含一个 `Option` 类型的 `inner` 字段，用于安全地初始化和销毁插件的状态。
#[derive(Debug, Default)]
pub struct Plugin {
    inner: Option<PluginInner>,
}

impl Plugin {
    /// A helper function to safely access the inner state of the plugin.
    /// It panics if the inner state is not initialized.
    fn with_inner<F>(&self, f: F) -> PluginResult<()>
    where
        F: FnOnce(&PluginInner) -> PluginResult<()>,
    {
        let inner = self.inner.as_ref().expect("initialized");
        f(inner)
    }
}

impl GeyserPlugin for Plugin {
    /// Returns the name of the plugin.
    fn name(&self) -> &'static str {
        concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"))
    }

    /// Called when the plugin is loaded.
    /// This function initializes the plugin's state, including the Tokio runtime,
    /// gRPC service, and Prometheus service.
    /// 插件加载时调用。它会读取配置、设置日志、创建 Tokio 运行时，并启动 gRPC 和 Prometheus 服务。
    fn on_load(&mut self, config_file: &str, is_reload: bool) -> PluginResult<()> {
        let config = Config::load_from_file(config_file)?;

        // Setup logger
        solana_logger::setup_with_default(&config.log.level);

        // Create inner
        let mut builder = Builder::new_multi_thread();
        if let Some(worker_threads) = config.tokio.worker_threads {
            builder.worker_threads(worker_threads);
        }
        if let Some(tokio_cpus) = config.tokio.affinity.clone() {
            builder.on_thread_start(move || {
                affinity::set_thread_affinity(&tokio_cpus).expect("failed to set affinity")
            });
        }
        let runtime = builder
            .thread_name_fn(crate::get_thread_name)
            .enable_all()
            .build()
            .map_err(|error| GeyserPluginError::Custom(Box::new(error)))?;

        let (snapshot_channel, grpc_channel, grpc_shutdown, prometheus) =
            runtime.block_on(async move {
                let (debug_client_tx, debug_client_rx) = mpsc::unbounded_channel();
                let (snapshot_channel, grpc_channel, grpc_shutdown) = GrpcService::create(
                    config.tokio,
                    config.grpc,
                    config.debug_clients_http.then_some(debug_client_tx),
                    is_reload,
                )
                .await
                .map_err(|error| GeyserPluginError::Custom(format!("{error:?}").into()))?;
                let prometheus = PrometheusService::new(
                    config.prometheus,
                    config.debug_clients_http.then_some(debug_client_rx),
                )
                .await
                .map_err(|error| GeyserPluginError::Custom(Box::new(error)))?;
                Ok::<_, GeyserPluginError>((
                    snapshot_channel,
                    grpc_channel,
                    grpc_shutdown,
                    prometheus,
                ))
            })?;

        self.inner = Some(PluginInner {
            runtime,
            snapshot_channel: Mutex::new(snapshot_channel),
            snapshot_channel_closed: AtomicBool::new(false),
            grpc_channel,
            grpc_shutdown,
            prometheus,
        });

        Ok(())
    }

    /// Called when the plugin is unloaded.
    /// This function gracefully shuts down the gRPC service, Prometheus service,
    /// and the Tokio runtime.
    /// 插件卸载时调用。它会平滑地关闭 gRPC 服务、Prometheus 服务和 Tokio 运行时。
    fn on_unload(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.grpc_shutdown.notify_one();
            drop(inner.grpc_channel);
            inner.prometheus.shutdown();
            inner.runtime.shutdown_timeout(Duration::from_secs(30));
        }
    }

    /// Called when an account is updated.
    /// It converts the account information into a `Message` and sends it to the gRPC service.
    /// During startup, it uses a blocking channel (`snapshot_channel`) to handle backpressure.
    /// After startup, it uses a non-blocking channel (`grpc_channel`).
    /// 插件收到账户更新时调用。它会根据账户信息创建一个 `Message` 并发送给 gRPC 服务。
    /// 在启动期间，它使用一个阻塞的通道 (`snapshot_channel`) 来处理背压，防止验证器被过多的账户更新淹没。
    /// 在启动完成后，它使用一个非阻塞的通道 (`grpc_channel`) 来发送账户更新。
    fn update_account(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: u64,
        is_startup: bool,
    ) -> PluginResult<()> {
        self.with_inner(|inner| {
            let account = match account {
                ReplicaAccountInfoVersions::V0_0_1(_info) => {
                    unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported")
                }
                ReplicaAccountInfoVersions::V0_0_2(_info) => {
                    unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported")
                }
                ReplicaAccountInfoVersions::V0_0_3(info) => info,
            };

            if is_startup {
                if let Some(channel) = inner.snapshot_channel.lock().unwrap().as_ref() {
                    let message =
                        Message::Account(MessageAccount::from_geyser(account, slot, is_startup));
                    match channel.send(Box::new(message)) {
                        Ok(()) => metrics::message_queue_size_inc(),
                        Err(_) => {
                            if !inner.snapshot_channel_closed.swap(true, Ordering::Relaxed) {
                                log::error!(
                                    "failed to send message to startup queue: channel closed"
                                )
                            }
                        }
                    }
                }
            } else {
                let message =
                    Message::Account(MessageAccount::from_geyser(account, slot, is_startup));
                inner.send_message(message);
            }

            Ok(())
        })
    }

    /// Called when the validator has finished sending the initial snapshot of account data.
    /// This function closes the `snapshot_channel`.
    /// 插件收到账户更新结束时调用。它会关闭 `snapshot_channel`。
    fn notify_end_of_startup(&self) -> PluginResult<()> {
        self.with_inner(|inner| {
            let _snapshot_channel = inner.snapshot_channel.lock().unwrap().take();
            Ok(())
        })
    }

    /// Called when the status of a slot changes.
    /// It converts the slot status information into a `Message` and sends it to the gRPC service.
    /// 插件收到 slot 状态更新时调用。它会根据 slot 状态信息创建一个 `Message` 并发送给 gRPC 服务。
    fn update_slot_status(
        &self,
        slot: u64,
        parent: Option<u64>,
        status: &SlotStatus,
    ) -> PluginResult<()> {
        self.with_inner(|inner| {
            let message = Message::Slot(MessageSlot::from_geyser(slot, parent, status));
            inner.send_message(message);
            metrics::update_slot_status(status, slot);
            Ok(())
        })
    }

    /// Called when a transaction is processed.
    /// It converts the transaction information into a `Message` and sends it to the gRPC service.
    fn notify_transaction(
        &self,
        transaction: ReplicaTransactionInfoVersions<'_>,
        slot: u64,
    ) -> PluginResult<()> {
        self.with_inner(|inner| {
            let transaction = match transaction {
                ReplicaTransactionInfoVersions::V0_0_1(_info) => {
                    unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported")
                }
                ReplicaTransactionInfoVersions::V0_0_2(info) => info,
            };

            let message = Message::Transaction(MessageTransaction::from_geyser(transaction, slot));
            inner.send_message(message);

            Ok(())
        })
    }

    /// Called when a new entry is observed.
    /// It converts the entry information into a `Message` and sends it to the gRPC service.
    fn notify_entry(&self, entry: ReplicaEntryInfoVersions) -> PluginResult<()> {
        self.with_inner(|inner| {
            #[allow(clippy::infallible_destructuring_match)]
            let entry = match entry {
                ReplicaEntryInfoVersions::V0_0_1(_entry) => {
                    unreachable!("ReplicaEntryInfoVersions::V0_0_1 is not supported")
                }
                ReplicaEntryInfoVersions::V0_0_2(entry) => entry,
            };

            let message = Message::Entry(Arc::new(MessageEntry::from_geyser(entry)));
            inner.send_message(message);

            Ok(())
        })
    }

    /// Called to notify the plugin of block metadata.
    /// It converts the block information into a `Message` and sends it to the gRPC service.
    fn notify_block_metadata(&self, blockinfo: ReplicaBlockInfoVersions<'_>) -> PluginResult<()> {
        self.with_inner(|inner| {
            let blockinfo = match blockinfo {
                ReplicaBlockInfoVersions::V0_0_1(_info) => {
                    unreachable!("ReplicaBlockInfoVersions::V0_0_1 is not supported")
                }
                ReplicaBlockInfoVersions::V0_0_2(_info) => {
                    unreachable!("ReplicaBlockInfoVersions::V0_0_2 is not supported")
                }
                ReplicaBlockInfoVersions::V0_0_3(_info) => {
                    unreachable!("ReplicaBlockInfoVersions::V0_0_3 is not supported")
                }
                ReplicaBlockInfoVersions::V0_0_4(info) => info,
            };

            let message = Message::BlockMeta(Arc::new(MessageBlockMeta::from_geyser(blockinfo)));
            inner.send_message(message);

            Ok(())
        })
    }

    /// Checks if account data notifications are enabled.
    /// This plugin is interested in account data, so it returns true.
    fn account_data_notifications_enabled(&self) -> bool {
        true
    }

    /// Checks if account data notifications for the initial snapshot are enabled.
    /// This is required to get the initial state of accounts.
    fn account_data_snapshot_notifications_enabled(&self) -> bool {
        if let Some(inner) = self.inner.as_ref() {
            inner.snapshot_channel.lock().unwrap().is_some()
        } else {
            false
        }
    }

    /// Checks if transaction notifications are enabled.
    /// This plugin is interested in transactions, so it returns true.
    fn transaction_notifications_enabled(&self) -> bool {
        true
    }

    /// Checks if entry notifications are enabled.
    /// This plugin is interested in entries, so it returns true.
    fn entry_notifications_enabled(&self) -> bool {
        true
    }
}

/// The entry point for the Geyser plugin, which creates a new instance of the plugin.
/// This function is called by the Solana validator to load the plugin.
#[no_mangle]
#[allow(improper_ctypes_definitions)]
/// # Safety
///
/// This function returns the Plugin pointer as trait GeyserPlugin.
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    let plugin = Plugin::default();
    let plugin: Box<dyn GeyserPlugin> = Box::new(plugin);
    Box::into_raw(plugin)
}
