use crate::PluginError; // Assuming PluginError is in lib.rs or a shared module
use serde::Deserialize;
use solana_program::pubkey::Pubkey; // Needed for validation if done here
use std::fs; // Import fs directly for read_to_string
use std::path::Path;
use std::str::FromStr; // Required for Pubkey::from_str

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)] // Make parsing stricter
pub struct PluginConfig {
    pub libpath: String,
    pub redis_url: String,
    pub program_id: String,
    pub redis_stream_name: String,
    pub log_level: Option<String>, // Default will be handled in lib.rs or here if made non-optional
}

impl PluginConfig {
    pub fn load_from_file<P: AsRef<Path>>(file_path: P) -> Result<Self, PluginError> {
        let path_ref = file_path.as_ref();
        let contents = fs
            ::read_to_string(path_ref)
            .map_err(|e| {
                PluginError::ConfigFileReadError(format!("Reading/Opening '{:?}': {}", path_ref, e))
            })?;

        let config: Self = serde_json
            ::from_str(&contents)
            .map_err(|e| PluginError::ConfigFileParseError(e.to_string()))?;

        // Validate program_id after parsing
        Pubkey::from_str(&config.program_id).map_err(|e| {
            PluginError::InvalidPubkey(format!("program_id '{}': {}", config.program_id, e))
        })?;

        Ok(config)
    }
}
