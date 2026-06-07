//! Host filesystem configuration for Becky-managed data.

use std::path::PathBuf;

use bon::Builder;
use tracing::{debug, warn};

/// Local directories and executable search paths used by providers.
#[derive(Builder, Debug, Clone, Default)]
pub struct SystemConfiguration {
    /// Candidate emulator executable paths.
    pub emulator_paths: Vec<PathBuf>,
    /// Candidate provider binary paths.
    pub binary_paths: Vec<PathBuf>,
    /// Runtime state directory.
    pub run_path: PathBuf,
    /// Virtual machine runtime directory.
    pub vm_root_path: PathBuf,
    /// Virtual machine persistent data directory.
    pub vm_data_root_path: PathBuf,
    /// Operating-system image cache directory.
    pub os_cache_root_path: PathBuf,
}

impl SystemConfiguration {
    /// Creates a system configuration and best-effort creates the configured
    /// directories.
    ///
    /// Directory creation errors are currently ignored and should be handled by
    /// later storage operations.
    pub async fn new(
        run_path: PathBuf,
        emulator_paths: Vec<PathBuf>,
        vm_root_path: PathBuf,
        vm_data_root_path: PathBuf,
        os_cache_root_path: PathBuf,
    ) -> Result<Self, std::io::Error> {
        for path in &emulator_paths {
            if !path.exists() {
                warn!("system emulator path {:?} does not exist", path);
            }
        }

        tokio::fs::create_dir_all(&run_path).await?;
        debug!("created path {:?}", run_path);
        tokio::fs::create_dir_all(&vm_root_path).await?;
        debug!("created vm run path {:?}", vm_root_path);
        tokio::fs::create_dir_all(&vm_data_root_path).await?;
        debug!("created vm data path {:?}", vm_data_root_path);
        tokio::fs::create_dir_all(&os_cache_root_path).await?;
        debug!("created os cache data path {:?}", os_cache_root_path);

        Ok(SystemConfiguration {
            emulator_paths: emulator_paths.clone(),
            binary_paths: vec![],
            run_path: run_path.clone(),
            vm_root_path,
            vm_data_root_path,
            os_cache_root_path,
        })
    }
}
