use agave_geyser_plugin_interface::geyser_plugin_interface::{
    GeyserPlugin,
    GeyserPluginError,
    ReplicaAccountInfoVersions,
    ReplicaBlockInfoVersions,
    ReplicaTransactionInfoVersions,
    Result,
};
use solana_program::pubkey::Pubkey;
use solana_logger;
use std::str::FromStr;
use std::sync::{ Arc, atomic::{ AtomicU64, Ordering } };
use tokio::runtime::{ Builder, Runtime };
use tokio::sync::Mutex as TokioMutex;

use redis::aio::MultiplexedConnection;

use thiserror::Error;
use log::{ info, error };

mod config; // Declare the config module
use config::PluginConfig; // Use PluginConfig from the module

// Helper function for thread naming
fn get_thread_name() -> String {
    static ATOMIC_ID: AtomicU64 = AtomicU64::new(0);
    let id = ATOMIC_ID.fetch_add(1, Ordering::Relaxed);
    format!("solGeyserRedisPlgn{id:02}") // Changed prefix for clarity
}

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("Configuration file error: {0}")] ConfigFileReadError(String),
    #[error("Configuration parse error: {0}")] ConfigFileParseError(String),
    #[error("Configuration missing field: {0}")] ConfigMissingField(String),
    #[error("Invalid Pubkey string: {0}")] InvalidPubkey(String),
    #[error("Redis connection error: {0}")] RedisConnectionError(String),
    #[error("Redis command error: {0}")] RedisCommandError(String),
    #[error("Runtime error: {0}")] RuntimeError(String),
    #[error("Plugin not loaded or inner components missing")] PluginNotLoaded,
    #[error(
        "Failed to convert account data at slot {slot}: Invalid length"
    )] AccountDataConversionError {
        slot: u64,
    },
}

impl From<PluginError> for GeyserPluginError {
    fn from(err: PluginError) -> Self {
        GeyserPluginError::Custom(Box::new(err))
    }
}

#[derive(Debug)]
struct PluginInner {
    runtime: Arc<Runtime>,
    redis_connection: Arc<TokioMutex<MultiplexedConnection>>,
    config: PluginConfig, // This will now be crate::config::PluginConfig
}

#[derive(Debug)]
struct AgentFeedPlugin {
    inner: Option<PluginInner>,
}

impl AgentFeedPlugin {
    fn new() -> Self {
        Self { inner: None }
    }
}

impl GeyserPlugin for AgentFeedPlugin {
    fn name(&self) -> &'static str {
        // Consider using concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION")) later
        "AgentFeedRedisStreamPlugin"
    }

    fn on_load(&mut self, config_file_path: &str, is_reload: bool) -> Result<()> {
        solana_logger::setup_with_default("info");
        info!("Loading AgentFeedRedisStreamPlugin...");
        if is_reload {
            info!("Reloading {}...", self.name());
            self.inner = None;
        }

        let parsed_config = PluginConfig::load_from_file(config_file_path)?;
        info!("Parsed config: {}", parsed_config.libpath);

        // Build Tokio runtime
        let runtime = Arc::new(
            Builder::new_multi_thread()
                .thread_name_fn(get_thread_name)
                .enable_all()
                .build()
                .map_err(|e| PluginError::RuntimeError(e.to_string()))?
        );
        info!("Tokio runtime created.");

        let redis_url = parsed_config.redis_url.clone();
        let redis_connection_result = runtime.block_on(async {
            let client = redis::Client::open(redis_url.as_str())?;
            client.get_multiplexed_async_connection().await
        });

        let redis_conn = match redis_connection_result {
            Ok(conn) => Arc::new(TokioMutex::new(conn)),
            Err(e) => {
                let err_msg = format!("Failed to connect to Redis: {}", e);
                error!("{}", err_msg);
                return Err(PluginError::RedisConnectionError(err_msg).into());
            }
        };
        info!("Successfully connected to Redis.");

        self.inner = Some(PluginInner {
            runtime,
            redis_connection: redis_conn,
            config: parsed_config.clone(),
        });

        info!(
            "{} loaded successfully. Monitoring program ID: {}. Stream: {}",
            self.name(),
            parsed_config.program_id,
            parsed_config.redis_stream_name
        );

        Ok(())
    }

    fn on_unload(&mut self) {
        info!("Unloading {}. Dropping inner components.", self.name());
        self.inner = None; // This will drop PluginInner, runtime, and Redis connection implicitly
    }

    fn update_account(
        &self,
        account_info_versions: ReplicaAccountInfoVersions,
        slot: u64,
        _is_startup: bool
    ) -> Result<()> {
        let inner = self.inner.as_ref().ok_or(PluginError::PluginNotLoaded)?;
        let current_config = &inner.config; // Borrow config from inner

        let target_program_id = match Pubkey::from_str(&current_config.program_id) {
            Ok(id) => id,
            Err(e) => {
                error!(
                    "Invalid target program ID '{}' in stored config ({}): skipping account update.",
                    current_config.program_id,
                    e
                );
                return Ok(());
            }
        };
        let redis_stream_key = &current_config.redis_stream_name;

        let (account_pubkey_slice, owner_slice, account_data_slice) = match account_info_versions {
            ReplicaAccountInfoVersions::V0_0_1(info) => (info.pubkey, info.owner, info.data),
            ReplicaAccountInfoVersions::V0_0_2(info) => (info.pubkey, info.owner, info.data),
            ReplicaAccountInfoVersions::V0_0_3(info) => (info.pubkey, info.owner, info.data),
        };

        let owner_pubkey_bytes: [u8; 32] = owner_slice
            .try_into()
            .map_err(|_| PluginError::AccountDataConversionError { slot })?;
        let owner_pubkey = Pubkey::new_from_array(owner_pubkey_bytes);

        if owner_pubkey == target_program_id {
            let conn_clone = Arc::clone(&inner.redis_connection);
            let runtime_handle = Arc::clone(&inner.runtime);

            let account_pubkey_bytes_array: [u8; 32] = account_pubkey_slice
                .try_into()
                .map_err(|_| PluginError::AccountDataConversionError { slot })?;
            let account_pubkey_str = Pubkey::new_from_array(account_pubkey_bytes_array).to_string();
            let owned_raw_data = account_data_slice.to_vec();
            let stream_key_clone = redis_stream_key.clone(); // Clone for the async block

            runtime_handle.spawn(async move {
                let mut locked_conn = conn_clone.lock().await;
                let result: redis::RedisResult<String> = redis
                    ::cmd("XADD")
                    .arg(&stream_key_clone)
                    .arg("*")
                    .arg("slot")
                    .arg(slot.to_string())
                    .arg("account_pubkey")
                    .arg(account_pubkey_str)
                    .arg("raw_data")
                    .arg(&owned_raw_data)
                    .query_async(&mut *locked_conn).await;

                if let Err(e) = result {
                    error!(
                        "Slot {}: Failed to XADD account {} to Redis stream {}: {:?}",
                        slot,
                        Pubkey::new_from_array(account_pubkey_bytes_array).to_string(),
                        stream_key_clone,
                        e
                    );
                }
            });
        } else {
            // This case can be noisy if not desired; consider removing log or making it debug level
            // info!("Account {} not owned by target program {}, owner {}. Skipping.", Pubkey::new_from_array(account_pubkey_bytes_array), target_program_id, owner_pubkey);
        }
        Ok(())
    }

    fn notify_transaction(
        &self,
        _transaction: ReplicaTransactionInfoVersions,
        _slot: u64
    ) -> Result<()> {
        Ok(())
    }
    fn notify_block_metadata(&self, _blockinfo: ReplicaBlockInfoVersions) -> Result<()> {
        Ok(())
    }
    fn account_data_notifications_enabled(&self) -> bool {
        true
    }
    fn transaction_notifications_enabled(&self) -> bool {
        false
    }
}

#[no_mangle]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    Box::into_raw(Box::new(AgentFeedPlugin::new()))
}
