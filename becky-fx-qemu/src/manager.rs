use becky_engine::control::FxControl;
use becky_engine::host_id::HostId;
use becky_engine::metadata::MetadataManager;
use becky_engine::storage::SysStorage;
use becky_engine::sys_conf::SystemConfiguration;
use becky_engine::verify::FxVerify;
use becky_fx_id::FxId;

use crate::handle::QemuHandle;
use crate::instance::QemuInstance;
use crate::{CreateError, GuestAgentCmd, QemuFxResourceConstraints, QemuMachineConfiguration, QmpCmd, WorkerCommand, WorkerEvent};

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace};

pub const QEMU_STATUS_POLL_SECONDS: u64 = 10;
pub const QEMU_WORKER_COMMAND_BUFFER_SIZE: usize = 32;

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
    /// channel to send commands to workers
    vmctl: HashMap<FxId, Sender<WorkerCommand>>,
}

impl QemuManager {
    pub fn new(system_configuration: SystemConfiguration, tx: Sender<WorkerEvent>) -> Self {
        QemuManager {
            system_configuration,
            vms: Default::default(),
            sender: tx,
            vmctl: Default::default(),
        }
    }

    pub async fn run_watchers<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        mdt: &mut T,
        rc: &impl QemuFxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Vec<JoinHandle<()>> {
        let mut join_handles = Vec::new();

        for (uuid, vm) in &self.vms {
            let (worker_tx, worker_rx) = tokio::sync::mpsc::channel::<WorkerCommand>(QEMU_WORKER_COMMAND_BUFFER_SIZE);

            let vm_event_tx = self.sender.clone();
            let guest_agent_enabled = vm.read().await.qemu.has_ga();
            match vm.write().await.qemu.fx_start(host_id, &uuid.clone(), mdt, rc, storage).await {
                Ok(handle) => {
                    let handle = tokio::spawn(worker_process(
                        uuid.clone(),
                        vm_event_tx,
                        worker_tx.clone(),
                        worker_rx,
                        vm.clone(),
                        handle,
                        guest_agent_enabled,
                    ));
                    self.vmctl.insert(uuid.clone(), worker_tx.clone());
                    join_handles.push(handle);
                }
                Err(e) => {
                    error!("qemu:manager:spawn error:{}", e);
                }
            }
        }

        join_handles
    }

    pub async fn shutdown(&mut self) {
        for (uuid, vmctl) in &self.vmctl {
            info!("qemu:manager shutting down vm:{:?}", uuid);
            let _ = vmctl.send(WorkerCommand::Shutdown).await;
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
    ) -> Result<JoinHandle<()>, CreateError> {
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
                    let guest_agent_enabled = qemu.has_ga();
                    let running_qemu_instance = Arc::new(RwLock::new(QemuRunningInstance { pid: 0, qemu }));
                    let spawn_handle = tokio::spawn(worker_process(
                        fx_id.clone(),
                        worker_event_tx,
                        worker_cmd_tx.clone(),
                        worker_cmd_rx,
                        running_qemu_instance.clone(),
                        handle,
                        guest_agent_enabled,
                    ));

                    self.vmctl.insert(fx_id.clone(), worker_cmd_tx);
                    self.vms.insert(fx_id.clone(), running_qemu_instance);
                    Ok(spawn_handle)
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

    pub async fn stop<T: MetadataManager>(&mut self, _host_id: &HostId, _mdt: &mut T, fx_id: &FxId) -> Result<(), ()> {
        match self.vmctl.get(fx_id) {
            None => Err(()),
            Some(vm) => match vm.send(WorkerCommand::Qmp(QmpCmd::SystemPowerdown)).await {
                Ok(_empty) => {
                    debug!("qemu:manager:stop sent vm_id:{:?}", fx_id);
                    Ok(())
                }
                Err(send_err) => {
                    error!("qemu:manager:stop:send vm_id:{} error:{:?}", fx_id, &send_err);
                    Err(())
                }
            },
        }
    }

    pub async fn pause<T: MetadataManager>(&mut self, _host_id: &HostId, _mdt: &mut T) {}
}

async fn schedule_event(cmd_tx: Sender<WorkerCommand>, interval: u64, cmd: WorkerCommand) -> Sender<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    tokio::spawn(async move {
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

    tx
}

async fn worker_process(
    fx_id: FxId,
    event_tx: Sender<WorkerEvent>,
    cmd_tx: Sender<WorkerCommand>,
    mut cmd_rx: Receiver<WorkerCommand>,
    qemu: Arc<RwLock<QemuRunningInstance>>,
    mut handle: QemuHandle,
    guest_agent_enabled: bool,
) -> () {
    info!("qemu:worker starting id:{}", fx_id);

    let mut write_lock = qemu.write().await;

    let mut cancelers = vec![];

    cancelers.push(schedule_event(cmd_tx.clone(), QEMU_STATUS_POLL_SECONDS, WorkerCommand::Qmp(QmpCmd::Status)).await);
    cancelers.push(
        schedule_event(
            cmd_tx.clone(),
            QEMU_STATUS_POLL_SECONDS,
            WorkerCommand::Qmp(QmpCmd::QueryBlock), // TODO
        )
        .await,
    );

    if guest_agent_enabled {
        cancelers.push(schedule_event(cmd_tx.clone(), QEMU_STATUS_POLL_SECONDS, WorkerCommand::GuestAgent(GuestAgentCmd::Ping)).await);

        cancelers.push(schedule_event(cmd_tx.clone(), QEMU_STATUS_POLL_SECONDS, WorkerCommand::GuestAgent(GuestAgentCmd::Info)).await);
    }

    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => {
                if let Some(cmd) = &maybe_cmd {
                    info!("qemu:worker new cmd:{:?}", &cmd);
                    match &cmd {
                        WorkerCommand::Shutdown => {
                            for cancel in cancelers {
                                let _ = cancel.send(()).await;
                            }
                            return ;
                        }
                        WorkerCommand::Qmp(qmp_cmd) => {
                            match qmp_cmd {
                                QmpCmd::Status => {
                                    if let Err(status_err) = write_lock.qemu.fx_status(&mut handle).await {
                                        error!("qemu:qmp:status error:{:?}", status_err);
                                    }
                                }
                            QmpCmd::QueryBlock => {
                                    if let Ok(block) = write_lock.qemu.call_api(&handle, qapi::qmp::query_block { }).await {
                                        info!("qemu:qmp:query_block:{:?}", &block);
                                    }
                                }
                                QmpCmd::SystemPowerdown => {
                                    if let Ok(_empty) = write_lock.qemu.fx_stop(&mut handle).await {
                                        info!("qemu:qmp:system_powerdown result:success");
                                    }
                                }
                            }
                        }
                        WorkerCommand::GuestAgent(ga_cmd) => {
                            match ga_cmd {
                                GuestAgentCmd::Ping => {
                                    if let Err(ping_err)= write_lock.qemu.call_ga_api(&handle, qapi::qga::guest_ping { }).await {
                                        error!("qemu:qga:ping error:{}", ping_err);
                                    }

                                },
                                GuestAgentCmd::Info => {
                                    if let Ok(guest_info) = write_lock.qemu.call_ga_api(&handle, qapi::qga::guest_info { }).await {
                                        info!("qemu:qga:guest_info version:{}", guest_info.version);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            rmsg = write_lock.qemu.qapi_event_rx.recv() => {
                if let Some(ev) = rmsg {
                    handle_event(ev, event_tx.clone()).await;
                }
            }
        }
    }
}

async fn handle_event(ev: qapi::qmp::Event, event_tx: Sender<WorkerEvent>) {
    match ev {
        qapi::qmp::Event::SHUTDOWN { .. } => {
            let _ = event_tx.send(WorkerEvent::Shutdown).await;
        }
        qapi::qmp::Event::POWERDOWN { .. } => {
            let _ = event_tx.send(WorkerEvent::Powerdown).await;
        }
        qapi::qmp::Event::RESET { .. } => {
            let _ = event_tx.send(WorkerEvent::Reset).await;
        }
        qapi::qmp::Event::STOP { .. } => {
            let _ = event_tx.send(WorkerEvent::Stop).await;
        }
        qapi::qmp::Event::RESUME { .. } => {
            let _ = event_tx.send(WorkerEvent::Resume).await;
        }
        qapi::qmp::Event::SUSPEND { .. } => {}
        qapi::qmp::Event::SUSPEND_DISK { .. } => {}
        qapi::qmp::Event::WAKEUP { .. } => {}
        qapi::qmp::Event::WATCHDOG { .. } => {}
        qapi::qmp::Event::GUEST_PANICKED { .. } => {
            let _ = event_tx.send(WorkerEvent::Panic).await;
        }
        qapi::qmp::Event::GUEST_CRASHLOADED { .. } => {}
        qapi::qmp::Event::GUEST_PVSHUTDOWN { .. } => {}
        qapi::qmp::Event::MEMORY_FAILURE { .. } => {}
        qapi::qmp::Event::JOB_STATUS_CHANGE { .. } => {}
        qapi::qmp::Event::DEVICE_TRAY_MOVED { .. } => {}
        qapi::qmp::Event::PR_MANAGER_STATUS_CHANGED { .. } => {}
        qapi::qmp::Event::BLOCK_IMAGE_CORRUPTED { .. } => {}
        qapi::qmp::Event::BLOCK_IO_ERROR { .. } => {}
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
        qapi::qmp::Event::MIGRATION { .. } => {}
        qapi::qmp::Event::MIGRATION_PASS { .. } => {}
        qapi::qmp::Event::COLO_EXIT { .. } => {}
        qapi::qmp::Event::UNPLUG_PRIMARY { .. } => {}
        qapi::qmp::Event::DEVICE_DELETED { .. } => {}
        qapi::qmp::Event::DEVICE_UNPLUG_GUEST_ERROR { .. } => {}
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
