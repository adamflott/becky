use becky_engine::control::FxControl;
use becky_engine::host_id::HostId;
use becky_engine::metadata::MetadataManager;
use becky_engine::state::FxExecutionState;
use becky_engine::storage::SysStorage;
use becky_engine::sys_conf::SystemConfiguration;
use becky_engine::verify::FxVerify;
use becky_fx_id::FxId;

use crate::handle::QemuHandle;
use crate::instance::QemuInstance;
use crate::{
    CreateError, GuestAgentCmd, QEMU_METADATA_PROVIDER, QemuFxResourceConstraints, QemuMachineConfiguration, QemuManagerStopError, QemuMetadataRecord, QmpCmd,
    WorkerCommand, WorkerEvent,
};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{Pid, System};
use tokio::sync::RwLock;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace};

pub const QEMU_WORKER_COMMAND_BUFFER_SIZE: usize = 32;
const QEMU_STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const QEMU_STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct ScheduledTask {
    cancel: Sender<()>,
    join: JoinHandle<()>,
}

struct WorkerControlHandle {
    sender: Sender<WorkerCommand>,
    join: JoinHandle<()>,
}

#[derive(Clone, Debug)]
struct QemuWorkerEvent {
    fx_id: FxId,
    event: WorkerEvent,
    state: FxExecutionState,
}

struct WorkerProcessContext {
    fx_id: FxId,
    event_tx: Sender<WorkerEvent>,
    cmd_tx: Sender<WorkerCommand>,
    qemu: Arc<RwLock<QemuRunningInstance>>,
    handle: QemuHandle,
    guest_agent_enabled: bool,
    status_poll_interval_secs: u64,
    metadata_event_tx: Sender<QemuWorkerEvent>,
}

/// Represents the running instance of QEMU
pub struct QemuRunningInstance {
    pub(crate) pid: u32,
    pub(crate) qemu: QemuInstance,
}

fn convert(system_configuration: &SystemConfiguration, rc: &impl QemuFxResourceConstraints) -> QemuMachineConfiguration {
    rc.qemu_machine_configuration(system_configuration)
}

/// Manager for running QEMU instance(s). Used to control their behavior and get status updates
pub struct QemuManager {
    // TODO change to &
    pub(crate) system_configuration: SystemConfiguration,
    pub(crate) vms: HashMap<FxId, Arc<RwLock<QemuRunningInstance>>>, // TODO is this a problem using uuid explicitly??
    /// channel workers send events from QEMU QMP on
    sender: Sender<WorkerEvent>,
    internal_event_tx: Sender<QemuWorkerEvent>,
    internal_event_rx: Receiver<QemuWorkerEvent>,
    /// channels and join handles for running workers
    vmctl: HashMap<FxId, WorkerControlHandle>,
}

impl QemuManager {
    pub fn new(system_configuration: SystemConfiguration, tx: Sender<WorkerEvent>) -> Self {
        let (internal_event_tx, internal_event_rx) = tokio::sync::mpsc::channel(QEMU_WORKER_COMMAND_BUFFER_SIZE);
        QemuManager {
            system_configuration,
            vms: Default::default(),
            sender: tx,
            internal_event_tx,
            internal_event_rx,
            vmctl: Default::default(),
        }
    }

    /// Registers a live QEMU runtime directory with this manager.
    ///
    /// This is a filesystem-based reconciliation hook for callers that already
    /// know the desired QEMU resource request for `fx_id`. It does not start a
    /// worker by itself; call [`QemuManager::run_watchers`] after registering
    /// discovered instances.
    pub fn register_existing(&mut self, rc: &impl QemuFxResourceConstraints, fx_id: FxId) -> Result<Option<u32>, CreateError> {
        let q = convert(&self.system_configuration, rc);
        let qemu = QemuInstance::existing_or_new(&self.system_configuration, q, &fx_id)?;
        let Some(pid) = qemu.existing_runtime_pid() else {
            return Ok(None);
        };

        self.vms.insert(fx_id, Arc::new(RwLock::new(QemuRunningInstance { pid, qemu })));
        Ok(Some(pid))
    }

    /// Scans the VM runtime root and registers live QEMU instances.
    ///
    /// Directory names are interpreted as [`FxId`] values. Each live pidfile is
    /// registered with the same supplied QEMU resource request; richer
    /// per-instance configuration still requires metadata persistence.
    pub async fn discover_existing_from_runtime_dir(&mut self, rc: &impl QemuFxResourceConstraints) -> Result<Vec<(FxId, u32)>, CreateError> {
        let mut discovered = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.system_configuration.vm_root_path).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(discovered),
            Err(err) => return Err(CreateError::Io(err)),
        };

        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }

            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let fx_id = match name.parse::<FxId>() {
                Ok(fx_id) => fx_id,
                Err(err) => match err {},
            };
            if let Some(pid) = self.register_existing(rc, fx_id.clone())? {
                discovered.push((fx_id, pid));
            }
        }

        Ok(discovered)
    }

    /// Loads QEMU inventory records from metadata and registers live instances.
    ///
    /// Unlike runtime-directory discovery, this path preserves the generated
    /// QEMU command line for each VM. That is enough to rebuild the provider
    /// handle and reattach monitoring after a manager restart even when VMs are
    /// heterogeneous.
    pub async fn discover_existing_from_metadata<T: MetadataManager>(&mut self, host_id: &HostId, mdt: &mut T) -> Result<Vec<(FxId, u32)>, CreateError> {
        let records = mdt
            .metadata_fx_record_list::<QemuMetadataRecord>(host_id, QEMU_METADATA_PROVIDER)
            .await
            .map_err(|err| CreateError::Metadata(format!("{err:?}")))?;

        let mut discovered = Vec::new();
        for (fx_id, record) in records {
            let qemu = QemuInstance::existing_from_metadata_record(&self.system_configuration, record, &fx_id)?;
            let Some(pid) = qemu.existing_runtime_pid() else {
                mdt.metadata_fx_state_update(host_id, &fx_id, becky_engine::state::FxExecutionState::Stopped)
                    .await
                    .map_err(|err| CreateError::Metadata(format!("{err:?}")))?;
                continue;
            };

            self.vms.insert(fx_id.clone(), Arc::new(RwLock::new(QemuRunningInstance { pid, qemu })));
            discovered.push((fx_id, pid));
        }

        Ok(discovered)
    }

    pub async fn run_watchers<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        mdt: &mut T,
        rc: &impl QemuFxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Vec<FxId> {
        let mut started = Vec::new();

        for (uuid, vm) in &self.vms {
            if self.vmctl.contains_key(uuid) {
                debug!("qemu:manager watcher already running vm_id:{uuid}");
                continue;
            }

            let (worker_tx, worker_rx) = tokio::sync::mpsc::channel::<WorkerCommand>(QEMU_WORKER_COMMAND_BUFFER_SIZE);

            let vm_event_tx = self.sender.clone();
            let metadata_event_tx = self.internal_event_tx.clone();
            let qemu_read = vm.read().await;
            let guest_agent_enabled = qemu_read.qemu.has_ga();
            let status_poll_interval_secs = qemu_read.qemu.status_poll_interval_secs();
            drop(qemu_read);
            let maybe_attached = vm.write().await.qemu.try_attach_existing(host_id, uuid, mdt).await;
            let handle_result = match maybe_attached {
                Ok(Some(handle)) => Ok(handle),
                Ok(None) => vm.write().await.qemu.fx_start(host_id, uuid, mdt, rc, storage).await,
                Err(err) => Err(err),
            };

            match handle_result {
                Ok(handle) => {
                    vm.write().await.pid = handle.process.pid;
                    let join = tokio::spawn(worker_process(
                        WorkerProcessContext {
                            fx_id: uuid.clone(),
                            event_tx: vm_event_tx,
                            cmd_tx: worker_tx.clone(),
                            qemu: vm.clone(),
                            handle,
                            guest_agent_enabled,
                            status_poll_interval_secs,
                            metadata_event_tx,
                        },
                        worker_rx,
                    ));
                    self.vmctl.insert(
                        uuid.clone(),
                        WorkerControlHandle {
                            sender: worker_tx.clone(),
                            join,
                        },
                    );
                    started.push(uuid.clone());
                }
                Err(e) => {
                    error!("qemu:manager:spawn error:{}", e);
                }
            }
        }

        started
    }

    pub async fn shutdown<T: MetadataManager>(&mut self, host_id: &HostId, mdt: &mut T) {
        let workers = std::mem::take(&mut self.vmctl);

        for (uuid, worker) in &workers {
            info!("qemu:manager shutting down vm:{:?}", uuid);
            let _ = worker.sender.send(WorkerCommand::Shutdown).await;
        }

        for (uuid, worker) in workers {
            match worker.join.await {
                Ok(()) => {
                    if let Some(vm) = self.vms.remove(&uuid) {
                        let qemu = vm.read().await;
                        let runtime_pid = process_exists(qemu.pid).then_some(qemu.pid);
                        let state = match runtime_pid {
                            Some(pid) => FxExecutionState::Running(pid),
                            None => FxExecutionState::Stopped,
                        };
                        if let Err(err) = mdt.metadata_fx_state_update(host_id, &uuid, state).await {
                            error!("qemu:manager metadata state update failed vm:{:?} error:{:?}", uuid, err);
                        }
                        if let Err(err) = mdt
                            .metadata_fx_record_upsert(host_id, QEMU_METADATA_PROVIDER, &uuid, qemu.qemu.metadata_record(runtime_pid))
                            .await
                        {
                            error!("qemu:manager metadata record update failed vm:{:?} error:{:?}", uuid, err);
                        }
                    }
                    info!("qemu:manager worker exited vm:{:?}", uuid);
                }
                Err(join_err) => {
                    error!("qemu:manager worker join error vm:{:?} error:{:?}", uuid, join_err);
                }
            }
        }
    }

    pub async fn allocate<T: MetadataManager>(
        &self,
        host_id: &HostId,
        mdt: &mut T,
        storage: &mut impl SysStorage,
        rc: &impl QemuFxResourceConstraints,
        fx_id: FxId,
    ) -> Result<(), CreateError> {
        let q = convert(&self.system_configuration, rc);
        let mut qemu = QemuInstance::existing_or_new(&self.system_configuration, q, &fx_id)?;

        match qemu.fx_allocate(host_id, &fx_id, mdt, rc, storage).await {
            Ok(_empty) => {
                mdt.metadata_fx_record_upsert(host_id, QEMU_METADATA_PROVIDER, &fx_id, qemu.metadata_record(qemu.existing_runtime_pid()))
                    .await
                    .map_err(|err| CreateError::Metadata(format!("{err:?}")))?;
                info!("qemu:manager successfully allocated");
                Ok(())
            }
            Err(alloc_error) => {
                error!("qemu:allocate error:{}", alloc_error);
                Err(CreateError::Allocate(alloc_error))
            }
        }
    }

    pub async fn create<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        mdt: &mut T,
        storage: &mut impl SysStorage,
        rc: impl QemuFxResourceConstraints,
        fx_id: FxId,
    ) -> Result<(), CreateError> {
        let q = convert(&self.system_configuration, &rc);
        let mut qemu = QemuInstance::existing_or_new(
            // TODO should be existing otherwise failure
            &self.system_configuration,
            q,
            &fx_id,
        )?;

        match qemu.fx_start(host_id, &fx_id, mdt, &rc, storage).await {
            Ok(mut handle) => match qemu.fx_op_verify(&mut handle).await {
                Ok(verification_result) => {
                    info!("qemu:verification result:{:?}", &verification_result);

                    let (worker_cmd_tx, worker_cmd_rx) = tokio::sync::mpsc::channel::<WorkerCommand>(QEMU_WORKER_COMMAND_BUFFER_SIZE);

                    let worker_event_tx = self.sender.clone();
                    let metadata_event_tx = self.internal_event_tx.clone();
                    let guest_agent_enabled = qemu.has_ga();
                    let status_poll_interval_secs = qemu.status_poll_interval_secs();
                    let pid = handle.process.pid;
                    let running_qemu_instance = Arc::new(RwLock::new(QemuRunningInstance { pid, qemu }));
                    let join = tokio::spawn(worker_process(
                        WorkerProcessContext {
                            fx_id: fx_id.clone(),
                            event_tx: worker_event_tx,
                            cmd_tx: worker_cmd_tx.clone(),
                            qemu: running_qemu_instance.clone(),
                            handle,
                            guest_agent_enabled,
                            status_poll_interval_secs,
                            metadata_event_tx,
                        },
                        worker_cmd_rx,
                    ));

                    self.vmctl.insert(fx_id.clone(), WorkerControlHandle { sender: worker_cmd_tx, join });
                    self.vms.insert(fx_id.clone(), running_qemu_instance);
                    Ok(())
                }
                Err(verify_err) => {
                    error!("qemu:manager verify error {:?}", &verify_err);
                    Err(CreateError::Verify(verify_err))
                }
            },
            Err(start_err) => {
                error!("qemu:manager spawn error:{:?}", &start_err);
                Err(CreateError::Spawn(start_err))
            }
        }
    }

    pub async fn stop<T: MetadataManager>(&mut self, host_id: &HostId, mdt: &mut T, fx_id: &FxId) -> Result<(), QemuManagerStopError> {
        let Some(vm) = self.vms.get(fx_id).cloned() else {
            return Err(QemuManagerStopError::NotFound(fx_id.clone()));
        };
        let pid = vm.read().await.pid;
        let worker = self.vmctl.get(fx_id).ok_or_else(|| QemuManagerStopError::NotFound(fx_id.clone()))?;

        worker.sender.send(WorkerCommand::Qmp(QmpCmd::SystemPowerdown)).await.map_err(|send_err| {
            error!("qemu:manager:stop:send vm_id:{} error:{:?}", fx_id, &send_err);
            QemuManagerStopError::Send(send_err.to_string())
        })?;
        debug!("qemu:manager:stop sent vm_id:{:?} pid:{}", fx_id, pid);

        wait_for_process_exit(pid, QEMU_STOP_WAIT_TIMEOUT)
            .await
            .map_err(|()| QemuManagerStopError::Timeout {
                fx_id: fx_id.clone(),
                pid,
                timeout_secs: QEMU_STOP_WAIT_TIMEOUT.as_secs(),
            })?;

        if let Some(worker) = self.vmctl.remove(fx_id) {
            let _ = worker.sender.send(WorkerCommand::Shutdown).await;
            worker.join.await.map_err(|join_err| QemuManagerStopError::Join(join_err.to_string()))?;
        }

        {
            let qemu = vm.read().await;
            mdt.metadata_fx_state_update(host_id, fx_id, FxExecutionState::Stopped)
                .await
                .map_err(|err| QemuManagerStopError::Metadata(format!("{err:?}")))?;
            mdt.metadata_fx_record_upsert(host_id, QEMU_METADATA_PROVIDER, fx_id, qemu.qemu.metadata_record(None))
                .await
                .map_err(|err| QemuManagerStopError::Metadata(format!("{err:?}")))?;
        }
        self.vms.remove(fx_id);
        Ok(())
    }

    pub async fn pause<T: MetadataManager>(&mut self, _host_id: &HostId, _mdt: &mut T) {}

    pub async fn drain_worker_events<T: MetadataManager>(&mut self, host_id: &HostId, mdt: &mut T) -> Result<usize, CreateError> {
        let mut updated = 0;

        while let Ok(worker_event) = self.internal_event_rx.try_recv() {
            mdt.metadata_fx_state_update(host_id, &worker_event.fx_id, worker_event.state.clone())
                .await
                .map_err(|err| CreateError::Metadata(format!("{err:?}")))?;

            if let Some(vm) = self.vms.get(&worker_event.fx_id) {
                let runtime_pid = match worker_event.state {
                    FxExecutionState::Running(pid) | FxExecutionState::Paused(pid) => Some(pid),
                    _ => None,
                };
                let qemu = vm.read().await;
                mdt.metadata_fx_record_upsert(host_id, QEMU_METADATA_PROVIDER, &worker_event.fx_id, qemu.qemu.metadata_record(runtime_pid))
                    .await
                    .map_err(|err| CreateError::Metadata(format!("{err:?}")))?;
            }

            debug!(
                "qemu:manager persisted worker event fx_id:{} event:{:?}",
                worker_event.fx_id, worker_event.event
            );
            updated += 1;
        }

        Ok(updated)
    }
}

async fn schedule_event(cmd_tx: Sender<WorkerCommand>, interval: u64, cmd: WorkerCommand) -> ScheduledTask {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = rx.recv() => {
                    info!("qemu:worker:scheduled_cmd received end");
                    break
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(interval)) => {
                    trace!("qemu:worker:scheduled_cmd elapsed_secs:{} sending:{:?}", interval, cmd);
                    match cmd_tx.send(cmd.clone()).await {
                        Ok(_empty) => {
                            trace!("qemu:worker:cmd sent");
                        },
                        Err(send_err) => {
                            error!("qemu:worker:cmd send error:{}", send_err);
                        }
                    }
                }
            }
        }
    });

    ScheduledTask { cancel: tx, join }
}

async fn stop_scheduled_tasks(tasks: Vec<ScheduledTask>) {
    for task in &tasks {
        let _ = task.cancel.send(()).await;
    }

    for task in tasks {
        let _ = task.join.await;
    }
}

async fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<(), ()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_exists(pid) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(QEMU_STOP_POLL_INTERVAL).await;
    }
}

fn process_exists(pid: u32) -> bool {
    System::new_all().process(Pid::from_u32(pid)).is_some()
}

async fn worker_process(ctx: WorkerProcessContext, mut cmd_rx: Receiver<WorkerCommand>) -> () {
    let WorkerProcessContext {
        fx_id,
        event_tx,
        cmd_tx,
        qemu,
        mut handle,
        guest_agent_enabled,
        status_poll_interval_secs,
        metadata_event_tx,
    } = ctx;

    info!("qemu:worker starting id:{}", fx_id);

    let mut event_rx = match qemu.write().await.qemu.take_qapi_event_rx() {
        Some(event_rx) => event_rx,
        None => {
            error!("qemu:worker missing QMP event receiver id:{}", fx_id);
            handle.stop_event_reader().await;
            return;
        }
    };

    let mut scheduled_tasks = vec![];

    scheduled_tasks.push(schedule_event(cmd_tx.clone(), status_poll_interval_secs, WorkerCommand::Qmp(QmpCmd::Status)).await);
    scheduled_tasks.push(
        schedule_event(
            cmd_tx.clone(),
            status_poll_interval_secs,
            WorkerCommand::Qmp(QmpCmd::QueryBlock), // TODO
        )
        .await,
    );

    if guest_agent_enabled {
        scheduled_tasks.push(schedule_event(cmd_tx.clone(), status_poll_interval_secs, WorkerCommand::GuestAgent(GuestAgentCmd::Ping)).await);

        scheduled_tasks.push(schedule_event(cmd_tx.clone(), status_poll_interval_secs, WorkerCommand::GuestAgent(GuestAgentCmd::Info)).await);
    }

    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => {
                if let Some(cmd) = &maybe_cmd {
                    info!("qemu:worker new cmd:{:?}", &cmd);
                    match &cmd {
                        WorkerCommand::Shutdown => {
                            stop_scheduled_tasks(scheduled_tasks).await;
                            handle.stop_event_reader().await;
                            return;
                        }
                        WorkerCommand::Qmp(qmp_cmd) => {
                            match qmp_cmd {
                                QmpCmd::Status => {
                                    if let Err(status_err) = qemu.write().await.qemu.fx_status(&mut handle).await {
                                        error!("qemu:qmp:status error:{:?}", status_err);
                                    }
                                }
                            QmpCmd::QueryBlock => {
                                    if !handle.supported_command("query-block") && !handle.supported_command("query_block") {
                                        error!("qemu:qmp:query_block unsupported by this QEMU");
                                    } else if let Ok(block) = qemu.write().await.qemu.call_api(&handle, qapi::qmp::query_block { }).await {
                                        info!("qemu:qmp:query_block:{:?}", &block);
                                    }
                                }
                                QmpCmd::SystemPowerdown => {
                                    match qemu.write().await.qemu.fx_stop(&mut handle).await {
                                        Ok(_empty) => {
                                            info!("qemu:qmp:system_powerdown result:success");
                                        }
                                        Err(stop_err) => {
                                            error!("qemu:qmp:system_powerdown error:{:?}", stop_err);
                                        }
                                    }
                                }
                            }
                        }
                        WorkerCommand::GuestAgent(ga_cmd) => {
                            match ga_cmd {
                                GuestAgentCmd::Ping => {
                                    if let Err(ping_err)= qemu.write().await.qemu.call_supported_ga_api(&handle, "guest-ping", qapi::qga::guest_ping { }).await {
                                        error!("qemu:qga:ping error:{}", ping_err);
                                    }

                                },
                                GuestAgentCmd::Info => {
                                    if let Ok(guest_info) = qemu.write().await.qemu.call_supported_ga_api(&handle, "guest-info", qapi::qga::guest_info { }).await {
                                        info!("qemu:qga:guest_info version:{}", guest_info.version);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    info!("qemu:worker command channel closed id:{}", fx_id);
                    stop_scheduled_tasks(scheduled_tasks).await;
                    handle.stop_event_reader().await;
                    return;
                }
            }
            rmsg = event_rx.recv() => {
                if let Some(ev) = rmsg {
                    handle_event(fx_id.clone(), handle.process.pid, ev, event_tx.clone(), metadata_event_tx.clone()).await;
                } else {
                    info!("qemu:worker QMP event channel closed id:{}", fx_id);
                    stop_scheduled_tasks(scheduled_tasks).await;
                    handle.stop_event_reader().await;
                    return;
                }
            }
        }
    }
}

fn state_from_worker_event(event: &WorkerEvent, pid: u32) -> FxExecutionState {
    match event {
        WorkerEvent::Panic => FxExecutionState::Error("guest panicked".to_string()),
        WorkerEvent::Watchdog => FxExecutionState::Error("guest watchdog fired".to_string()),
        WorkerEvent::BlockIoError => FxExecutionState::Error("qemu block I/O error".to_string()),
        WorkerEvent::BlockImageCorrupted => FxExecutionState::Error("qemu block image corrupted".to_string()),
        WorkerEvent::DeviceUnplugGuestError => FxExecutionState::Error("qemu device unplug guest error".to_string()),
        WorkerEvent::MemoryFailure => FxExecutionState::Error("qemu memory failure".to_string()),
        WorkerEvent::Shutdown | WorkerEvent::Powerdown => FxExecutionState::Stopped,
        WorkerEvent::Suspend | WorkerEvent::Stop => FxExecutionState::Paused(pid),
        WorkerEvent::Resume | WorkerEvent::Reset | WorkerEvent::DeviceDeleted | WorkerEvent::Migration => FxExecutionState::Running(pid),
    }
}

async fn forward_worker_event(fx_id: &FxId, pid: u32, event: WorkerEvent, event_tx: &Sender<WorkerEvent>, metadata_event_tx: &Sender<QemuWorkerEvent>) {
    let state = state_from_worker_event(&event, pid);
    let _ = event_tx.send(event.clone()).await;
    let _ = metadata_event_tx
        .send(QemuWorkerEvent {
            fx_id: fx_id.clone(),
            event,
            state,
        })
        .await;
}

async fn handle_event(fx_id: FxId, pid: u32, ev: qapi::qmp::Event, event_tx: Sender<WorkerEvent>, metadata_event_tx: Sender<QemuWorkerEvent>) {
    match ev {
        qapi::qmp::Event::SHUTDOWN { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Shutdown, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::POWERDOWN { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Powerdown, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::RESET { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Reset, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::STOP { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Stop, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::RESUME { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Resume, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::SUSPEND { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Suspend, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::SUSPEND_DISK { .. } => {}
        qapi::qmp::Event::WAKEUP { .. } => {}
        qapi::qmp::Event::WATCHDOG { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Watchdog, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::GUEST_PANICKED { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Panic, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::GUEST_CRASHLOADED { .. } => {}
        qapi::qmp::Event::GUEST_PVSHUTDOWN { .. } => {}
        qapi::qmp::Event::MEMORY_FAILURE { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::MemoryFailure, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::JOB_STATUS_CHANGE { .. } => {}
        qapi::qmp::Event::DEVICE_TRAY_MOVED { .. } => {}
        qapi::qmp::Event::PR_MANAGER_STATUS_CHANGED { .. } => {}
        qapi::qmp::Event::BLOCK_IMAGE_CORRUPTED { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::BlockImageCorrupted, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::BLOCK_IO_ERROR { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::BlockIoError, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::BLOCK_JOB_COMPLETED { .. } => {}
        qapi::qmp::Event::BLOCK_JOB_CANCELLED { .. } => {}
        qapi::qmp::Event::BLOCK_JOB_ERROR { .. } => {}
        qapi::qmp::Event::BLOCK_JOB_READY { .. } => {}
        qapi::qmp::Event::BLOCK_JOB_PENDING { .. } => {}
        qapi::qmp::Event::BLOCK_WRITE_THRESHOLD { .. } => {}
        qapi::qmp::Event::QUORUM_FAILURE { .. } => {}
        qapi::qmp::Event::QUORUM_REPORT_BAD { .. } => {}
        qapi::qmp::Event::BLOCK_EXPORT_DELETED { .. } => {}
        qapi::qmp::Event::VSERPORT_CHANGE { .. } => {}
        qapi::qmp::Event::DUMP_COMPLETED { .. } => {}
        qapi::qmp::Event::NIC_RX_FILTER_CHANGED { .. } => {}
        qapi::qmp::Event::FAILOVER_NEGOTIATED { .. } => {}
        qapi::qmp::Event::NETDEV_STREAM_CONNECTED { .. } => {}
        qapi::qmp::Event::NETDEV_STREAM_DISCONNECTED { .. } => {}
        qapi::qmp::Event::SPICE_CONNECTED { .. } => {}
        qapi::qmp::Event::SPICE_INITIALIZED { .. } => {}
        qapi::qmp::Event::SPICE_DISCONNECTED { .. } => {}
        qapi::qmp::Event::SPICE_MIGRATE_COMPLETED { .. } => {}
        qapi::qmp::Event::VNC_CONNECTED { .. } => {}
        qapi::qmp::Event::VNC_INITIALIZED { .. } => {}
        qapi::qmp::Event::VNC_DISCONNECTED { .. } => {}
        qapi::qmp::Event::MIGRATION { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Migration, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::MIGRATION_PASS { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::Migration, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::COLO_EXIT { .. } => {}
        qapi::qmp::Event::UNPLUG_PRIMARY { .. } => {}
        qapi::qmp::Event::DEVICE_DELETED { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::DeviceDeleted, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::DEVICE_UNPLUG_GUEST_ERROR { .. } => {
            forward_worker_event(&fx_id, pid, WorkerEvent::DeviceUnplugGuestError, &event_tx, &metadata_event_tx).await;
        }
        qapi::qmp::Event::BALLOON_CHANGE { .. } => {}
        qapi::qmp::Event::HV_BALLOON_STATUS_REPORT { .. } => {}
        qapi::qmp::Event::MEMORY_DEVICE_SIZE_CHANGE { .. } => {}
        qapi::qmp::Event::CPU_POLARIZATION_CHANGE { .. } => {}
        qapi::qmp::Event::RTC_CHANGE { .. } => {}
        qapi::qmp::Event::VFU_CLIENT_HANGUP { .. } => {}
        qapi::qmp::Event::ACPI_DEVICE_OST { .. } => {}
        qapi::qmp::Event::VFIO_MIGRATION { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_events_map_to_execution_state() {
        assert_eq!(state_from_worker_event(&WorkerEvent::Shutdown, 42), FxExecutionState::Stopped);
        assert_eq!(state_from_worker_event(&WorkerEvent::Powerdown, 42), FxExecutionState::Stopped);
        assert_eq!(state_from_worker_event(&WorkerEvent::Stop, 42), FxExecutionState::Paused(42));
        assert_eq!(state_from_worker_event(&WorkerEvent::Suspend, 42), FxExecutionState::Paused(42));
        assert_eq!(state_from_worker_event(&WorkerEvent::Resume, 42), FxExecutionState::Running(42));
        assert_eq!(state_from_worker_event(&WorkerEvent::Reset, 42), FxExecutionState::Running(42));
        assert_eq!(
            state_from_worker_event(&WorkerEvent::Panic, 42),
            FxExecutionState::Error("guest panicked".to_string())
        );
        assert_eq!(
            state_from_worker_event(&WorkerEvent::BlockIoError, 42),
            FxExecutionState::Error("qemu block I/O error".to_string())
        );
        assert_eq!(state_from_worker_event(&WorkerEvent::DeviceDeleted, 42), FxExecutionState::Running(42));
        assert_eq!(state_from_worker_event(&WorkerEvent::Migration, 42), FxExecutionState::Running(42));
    }

    #[tokio::test]
    async fn wait_for_process_exit_returns_for_missing_pid() {
        assert!(wait_for_process_exit(u32::MAX, Duration::from_millis(1)).await.is_ok());
    }
}
