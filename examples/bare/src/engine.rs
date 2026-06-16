use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sysinfo::DiskUsage;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tracing::{error, info};
use wora::Wora;
use wora::prelude::*;

use becky_engine::control::{ControlEngine, ControlEngineFxRestartPolicy, FxControl};
use becky_engine::empty_implementations::Metadataless;
use becky_engine::host::*;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::{FxResourceConstraints, ResourceConstraintless};
use becky_engine::metadata::{MetadataInit, MetadataInventory, MetadataJobUpdate, MetadataManager, MetadataSource, MetadataUpdate, OsImage};
use becky_engine::os::SupportedOs;
use becky_engine::state::{FxExecutionState, StateCollect, StateUpdate};
use becky_engine::storage::{Storageless, SysStorage};
use becky_engine::sys::{SysScanCollect, SysScanUpdate};
use becky_engine::verify::{FxVerify, Verification};
use becky_engine::*;
use becky_fx_id::FxId;
use becky_fx_system_command::FxSystemProcessRunning;

use crate::{BareRunningCommands, BareState};

pub struct BareEngine {
    host_id: HostId,
    registry: Option<HostId>,
    state: Arc<RwLock<BareState>>,
    tasks: Arc<RwLock<HashMap<u32, FxSystemProcessRunning>>>,
}

impl BareEngine {
    pub fn new(host_id: HostId, state: Arc<RwLock<BareState>>) -> Self {
        BareEngine {
            host_id,
            registry: None,
            state,
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn running_task_count(&self) -> usize {
        self.tasks.read().await.len()
    }
}

#[derive(Clone, Debug)]
pub struct BareMetric {}

#[async_trait]
impl HostSysInit<EngineEvent<BareEvent>, BareMetric> for BareEngine {
    async fn host_system_boot(
        &mut self,
        _exec: &impl AsyncExecutor<EngineEvent<BareEvent>, BareMetric>,
        _fs: &(impl WFS + 'static),
        _hi: &HostInfo,
    ) -> Result<(), ()> {
        info!("engine[{}]:host_system_boot()", self.host_id);
        Ok(())
    }

    async fn host_system_setup(
        &mut self,
        _exec: &impl AsyncExecutor<EngineEvent<BareEvent>, BareMetric>,
        _fs: &(impl WFS + 'static),
        _hi: &HostInfo,
    ) -> Result<(), ()> {
        info!("engine:host_system_setup()");
        Ok(())
    }
}

#[async_trait]
impl HostSysEnd<EngineEvent<BareEvent>, BareMetric> for BareEngine {
    async fn host_system_end(
        &mut self,
        _exec: impl AsyncExecutor<EngineEvent<BareEvent>, BareMetric>,
        _fs: impl WFS + 'static,
        _hi: &HostInfo,
    ) -> Result<(), ()> {
        info!("engine:host_system_end()");
        Ok(())
    }
}

#[derive(Clone, Debug, Error)]
pub enum BareError {}

#[async_trait]
impl RegisterHost for BareEngine {
    type RegisterError = BareError;

    async fn register(&mut self, host: &HostId) -> Result<(), Self::RegisterError> {
        let _ = self.registry = Some(host.clone());
        Ok(())
    }
    async fn unregister(&mut self, _host: &HostId) -> Result<(), Self::RegisterError> {
        let _ = self.registry = None;
        Ok(())
    }
    async fn is_registered(&self, _host: &HostId) -> Result<bool, Self::RegisterError> {
        Ok(self.registry.is_some())
    }
}

impl Display for BareEngine {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "hostid:{}", self.host_id)
    }
}

impl Debug for BareEngine {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {:?}", self.host_id, self.state)
    }
}

#[async_trait]
impl ControlEngine for BareEngine {
    type FxPingErr = ();
    type FxShutdownErr = ();
    type FxList = ();
    type FxListErr = ();
    type FxInfo = ();
    type FxInfoErr = ();

    async fn ctl_ping(&mut self, _deadline: Duration) -> Result<(), Self::FxPingErr> {
        Ok(())
    }

    async fn ctl_fx_restart_policy(&self) -> ControlEngineFxRestartPolicy {
        ControlEngineFxRestartPolicy::AttachFxOnResume
    }

    async fn ctl_fx_list(&mut self, _deadline: Duration) -> Result<Self::FxList, Self::FxInfoErr> {
        Ok(())
    }

    async fn ctl_fx_info(&mut self, _deadline: Duration) -> Result<Self::FxInfo, Self::FxInfoErr> {
        Ok(())
    }

    async fn ctl_fx_shutdown(&mut self, _deadline: Duration) -> Result<(), ()> {
        info!("engine:ctl_fx_shutdown action:detach");
        Ok(())
    }
}

#[async_trait]
impl MainLoop<EngineEvent<BareEvent>, BareMetric> for BareEngine {
    async fn mainloop(
        &mut self,
        wora: &mut Wora<EngineEvent<BareEvent>, BareMetric>,
        _exec: impl AsyncExecutor<EngineEvent<BareEvent>, BareMetric>,
        _fs: impl WFS,
        _metrics: Sender<O11yEvent<BareMetric>>,
    ) -> MainRetryAction {
        //wora.schedule_event(tokio::time::Duration::from_secs(2), Event::App(EngineEvent::Rescan)).await;

        let x = wora.sender.clone();
        let _rescan_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = x.send(Event::App(EngineEvent::Rescan)).await;
            }
        });

        let fx_id = FxId::String(self.state.read().await.args.cmd.clone());
        let mut mdt = Metadataless {};
        let mut storage = Storageless {};

        let host_id = self.host_id.clone();
        let rc = ResourceConstraintless {};
        match self.fx_start(&host_id, &fx_id, &mut mdt, &rc, &mut storage).await {
            Ok(tasks) => {
                self.tasks = tasks;
            }
            Err(_) => {
                error!("fx_start failed");
                return MainRetryAction::UseExitCode(2);
            }
        }

        while let Some(ev) = wora.receiver.recv().await {
            info!("worker:event new:{:?}", &ev);
            match ev {
                Event::SystemResource(_) => {}
                Event::App(ev) => match ev {
                    EngineEvent::App(bare_ev) => match bare_ev {
                        BareEvent::Event1 => {}
                        BareEvent::Event2 => {}
                    },
                    EngineEvent::Rescan => {
                        let _ = self.ctl_ping(Duration::from_secs(2)).await;
                        let _ = self.ctl_fx_info(Duration::from_secs(2)).await;

                        let mut tasks = self.tasks.clone();
                        if let Err(()) = self.fx_status(&mut tasks).await {
                            error!("fx_status failed");
                        }

                        let x = self.tasks.read().await;
                        for (_id, c) in &*x {
                            let cpu_time = c.accumulated_cpu_time(&()).await;
                            info!("accumulated cpu time: {}", cpu_time);
                            info!("disk usage: {:?}", c.disk_usage(&()).await);
                            info!("memory usage: {}", c.memory(&()).await);
                            info!("virtual memory usage: {}", c.virtual_memory(&()).await);
                            info!("run time: {}", c.run_time(&()).await);
                        }

                        if self.running_task_count().await == 0 {
                            info!("all commands exited");
                            return MainRetryAction::Success;
                        }
                    }
                    EngineEvent::FxStart => {}
                    EngineEvent::FxUpdate => {}
                    EngineEvent::FxStop => {}
                    EngineEvent::FxDelete => {}
                },
                Event::SystemResourceCPUThreshold(_, _) => {}
                Event::SystemResourceLoadThreshold(_, _) => {}
                Event::SystemResourceMemoryThreshold(_, _) => {}
                Event::Control(ev_ch) => match ev_ch {
                    ControlEvent::ReloadConfiguration => {}
                    ControlEvent::Suspend(_) => {}
                    ControlEvent::Shutdown(_ts) => {
                        info!("shutdown requested; detaching from running commands");
                        return MainRetryAction::Success;
                    }
                    ControlEvent::LogRotation => {}
                },
                Event::ConfigChanged(_) => {}
                Event::SecretChanged(_) => {}
                Event::LeadershipChanged(_, _) => {}
            }
        }

        MainRetryAction::Success
    }
}

#[async_trait]
impl MetadataInit for BareEngine {
    type MetadataInitError = ();

    async fn metadata_init(&self) -> Result<(), Self::MetadataInitError> {
        Ok(())
    }
}

#[async_trait]
impl MetadataInventory for BareEngine {
    type MetadataInventoryResult = ();
    type MetadataInventoryError = ();

    async fn metadata_fx_record_upsert<T>(
        &mut self,
        _host_id: &HostId,
        _provider: &str,
        _fxid: &FxId,
        _record: T,
    ) -> Result<Self::MetadataInventoryResult, Self::MetadataInventoryError>
    where
        T: Serialize + serde::de::DeserializeOwned + Send + Sync + Debug,
    {
        Ok(())
    }

    async fn metadata_fx_record_get<T>(&mut self, _host_id: &HostId, _provider: &str, _fxid: &FxId) -> Result<Option<T>, Self::MetadataInventoryError>
    where
        T: Serialize + serde::de::DeserializeOwned + Send + Sync + Debug,
    {
        todo!()
    }

    async fn metadata_fx_record_list<T>(&mut self, _host_id: &HostId, _provider: &str) -> Result<Vec<(FxId, T)>, Self::MetadataInventoryError>
    where
        T: Serialize + serde::de::DeserializeOwned + Send + Sync + Debug,
    {
        todo!()
    }

    async fn metadata_fx_record_delete(
        &mut self,
        _host_id: &HostId,
        _provider: &str,
        _fxid: &FxId,
    ) -> Result<Self::MetadataInventoryResult, Self::MetadataInventoryError> {
        Ok(())
    }
}

#[async_trait]
impl MetadataUpdate for BareEngine {
    type MetadataUpdateResult = ();
    type MetadataUpdateError = ();

    async fn metadata_fx_state_update(
        &mut self,
        _host_id: &HostId,
        _fxid: &FxId,
        _state: FxExecutionState,
    ) -> Result<Self::MetadataUpdateResult, Self::MetadataUpdateError> {
        Ok(())
    }
}

#[async_trait]
impl SysScanUpdate for BareEngine {}

#[async_trait]
impl MetadataManager for BareEngine {}

#[async_trait]
impl MetadataSource for BareEngine {
    type MetadataSourceHandle = ();
    type MetadataConnectError = ();

    async fn reconnect(&mut self) -> Result<Self::MetadataSourceHandle, Self::MetadataConnectError> {
        Ok(())
    }

    async fn disconnect(&mut self) -> () {
        ()
    }
}

#[async_trait]
impl MetadataJobUpdate for BareEngine {
    async fn metadata_fx_job_update(&mut self, _host_id: &HostId, _state: FxExecutionState, _job_id: Self::MetadataUpdateResult) {}
}

#[async_trait]
impl OsImage for BareEngine {
    type ImageDef = ();
    type SyncError = ();

    async fn sync_images(&mut self, _cache_root_dir: &Path) -> Result<(), Self::SyncError> {
        Ok(())
    }

    fn get_filename(&self, _image: &SupportedOs) -> PathBuf {
        todo!()
    }
}

#[async_trait]
impl SysScanCollect for BareEngine {
    type SysScanCollectResult = ();
    type SysScanCollectError = ();

    async fn sys_scan_collect<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
    ) -> Result<Self::SysScanCollectResult, Self::SysScanCollectError> {
        todo!()
    }
}

#[async_trait]
impl StateCollect for BareEngine {
    type FxStateCollectResult = ();
    type FxStateCollectError = ();

    async fn state_collect(&mut self, _handle: &mut Self::FxSpawnResult) -> Result<Self::FxStateCollectResult, Self::FxStateCollectError> {
        todo!()
    }
}

#[async_trait]
impl FxControl for BareEngine {
    type Id = String;

    fn id(&self) -> Self::Id {
        self.host_id.to_string()
    }

    type FxAllocateResult = ();
    type FxAllocateError = ();

    async fn fx_allocate<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        Ok(())
    }

    type FxBootstrapResult = ();
    type FxBootstrapError = ();

    async fn fx_bootstrap<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        Ok(())
    }

    type FxSpawnResult = Arc<RwLock<HashMap<u32, FxSystemProcessRunning>>>;
    type FxSpawnError = ();

    async fn fx_start<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        fx_id: &FxId,
        mdt: &mut T,
        rc: &impl FxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Result<Self::FxSpawnResult, Self::FxSpawnError> {
        let command_specs = {
            let state = self.state.read().await;
            state.cmds.cmds.iter().map(|(id, cmd_spec)| (*id, cmd_spec.clone())).collect::<Vec<_>>()
        };

        let mut tasks = HashMap::with_capacity(command_specs.len());
        for (id, mut cmd_spec) in command_specs {
            let instance_fx_id = FxId::String(format!("{fx_id}-{id}"));
            match cmd_spec.fx_start(host_id, &instance_fx_id, mdt, rc, storage).await {
                Ok(c) => {
                    info!("cmd id {id} started as pid {}", c.get_pid());
                    let mut state = self.state.write().await;
                    state.cmds.cmds.insert(id, cmd_spec);
                    tasks.insert(id, c);
                }
                Err(e) => {
                    error!("cmd id {id} failed to start: {:?}", e);
                }
            }
        }
        Ok(Arc::new(RwLock::new(tasks)))
    }

    type FxStatusResult = ();
    type FxStatusError = ();

    async fn fx_status(&mut self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxStatusResult, Self::FxStatusError> {
        let ids = {
            let tasks = fnr.read().await;
            tasks.keys().copied().collect::<Vec<_>>()
        };

        for id in ids {
            let Some(mut process) = ({
                let mut tasks = fnr.write().await;
                tasks.remove(&id)
            }) else {
                continue;
            };

            let Some(mut command) = ({
                let state = self.state.read().await;
                state.cmds.cmds.get(&id).cloned()
            }) else {
                let mut tasks = fnr.write().await;
                tasks.insert(id, process);
                continue;
            };

            match command.fx_status(&mut process).await {
                Ok(status) => {
                    info!("cmd id {id} status: {:?}", status);
                    let is_running = matches!(status, FxExecutionState::Running(_));
                    {
                        let mut state = self.state.write().await;
                        if let Some(cmd_spec) = state.cmds.cmds.get_mut(&id) {
                            cmd_spec.state = status;
                        }
                    }
                    if is_running {
                        let mut tasks = fnr.write().await;
                        tasks.insert(id, process);
                    }
                }
                Err(status) => {
                    error!("cmd id {id} status failed: {:?}", status);
                    let mut state = self.state.write().await;
                    if let Some(cmd_spec) = state.cmds.cmds.get_mut(&id) {
                        cmd_spec.state = status;
                    }
                }
            }
        }

        Ok(())
    }

    type FxStopResult = ();
    type FxStopError = ();

    async fn fx_stop(&mut self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError> {
        let tasks = fnr.read().await;
        for (id, c) in tasks.iter() {
            info!("stopping id {}", id);
            if let Err(err) = c.stop() {
                error!("cmd id {id} failed to stop: {:?}", err);
            }
        }
        Ok(())
    }

    type FxDestroyResult = ();
    type FxDestroyError = ();

    async fn fx_destroy(&self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxDestroyResult, Self::FxDestroyError> {
        let ids = {
            let tasks = fnr.read().await;
            tasks.keys().copied().collect::<Vec<_>>()
        };

        for id in ids {
            let Some(mut process) = ({
                let mut tasks = fnr.write().await;
                tasks.remove(&id)
            }) else {
                continue;
            };

            info!("destroying id {}", id);
            if let Err(err) = process.destroy().await {
                error!("cmd id {id} failed to destroy: {:?}", err);
            }
        }

        Ok(())
    }

    type FxArchiveResult = ();
    type FxArchiveError = ();

    async fn fx_archive(&self, _fnr: &mut Self::FxSpawnResult) -> Result<Self::FxArchiveResult, Self::FxArchiveError> {
        todo!()
    }
}

#[async_trait]
impl StateUpdate for BareEngine {}

#[async_trait]
impl FxVerify for BareEngine {
    type FxOpVerifyError = ();

    async fn fx_op_verify(&mut self, _handle: &mut Self::FxSpawnResult) -> Result<Verification, Self::FxOpVerifyError> {
        todo!()
    }
}

#[async_trait]
impl FxEngine<EngineEvent<BareEvent>, BareMetric, BareRunningCommands> for BareEngine {}

#[derive(Debug)]
pub enum BareEvent {
    Event1,
    Event2,
}

#[async_trait]
impl FxAccounting for BareEngine {
    type Instance = ();

    async fn accumulated_cpu_time(&self, _i: &Self::Instance) -> u64 {
        let mut sum = 0;
        let tasks = self.tasks.read().await;
        for (_id, task) in &*tasks {
            sum = sum + task.accumulated_cpu_time(&()).await
        }
        sum
    }

    async fn disk_usage(&self, _i: &Self::Instance) -> DiskUsage {
        todo!()
    }

    async fn memory(&self, _i: &Self::Instance) -> u64 {
        let mut sum = 0;
        let tasks = self.tasks.read().await;
        for (_id, task) in &*tasks {
            sum = sum + task.memory(&()).await
        }
        sum
    }

    async fn virtual_memory(&self, _i: &Self::Instance) -> u64 {
        let mut sum = 0;
        let tasks = self.tasks.read().await;
        for (_id, task) in &*tasks {
            sum = sum + task.virtual_memory(&()).await
        }
        sum
    }

    async fn run_time(&self, _i: &Self::Instance) -> u64 {
        let mut sum = 0;
        let tasks = self.tasks.read().await;
        for (_id, task) in &*tasks {
            sum = sum + task.run_time(&()).await
        }
        sum
    }
}
