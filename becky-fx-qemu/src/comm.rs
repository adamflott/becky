use crate::SpawnError;
use crate::handle::QemuHandle;
use becky_utils::get_process;
use futures::StreamExt;
use qapi::futures::{QapiEvents, QapiService, QgaStreamTokio, QmpStreamTokio};
use qapi::qmp::{Event, query_qmp_schema, query_version};
use std::path::PathBuf;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;
use tracing::{debug, error, info, warn};

pub async fn try_connect_ctl_socket(
    path_socket_ctl: &PathBuf,
    timeout_secs: u64,
    retry_interval_millis: u64,
) -> Result<
    (
        QapiService<QmpStreamTokio<WriteHalf<UnixStream>>>,
        QapiEvents<QmpStreamTokio<ReadHalf<UnixStream>>>,
    ),
    Elapsed,
> {
    debug!(
        "qemu:qmp:socket waiting up to {} seconds for control socket at {} to be available...",
        timeout_secs,
        &path_socket_ctl.as_path().display(),
    );

    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            match qapi::futures::QmpStreamTokio::open_uds(path_socket_ctl).await {
                Ok(stream) => {
                    info!("qemu:qmp:socket state:connected version:{:?}", stream.capabilities.QMP.version);
                    match stream.negotiate().await {
                        Ok(stream) => {
                            let (api, events) = stream.into_parts();
                            return (api, events);
                        }
                        Err(negotiate_err) => {
                            warn!(
                                "qemu:qmp:socket negotiate failed with error:{}, trying again after {} milliseconds",
                                negotiate_err, retry_interval_millis
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(retry_interval_millis)).await;
                        }
                    }
                }
                Err(open_uds_err) => {
                    warn!(
                        "qemu:qmp:socket open_uds() failed with error:{}, trying again after {} milliseconds",
                        open_uds_err, retry_interval_millis
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(retry_interval_millis)).await;
                }
            }
        }
    })
    .await
}

pub async fn try_monitor_qemu_with_api(
    api: QapiService<QmpStreamTokio<WriteHalf<UnixStream>>>,
    mut events: QapiEvents<QmpStreamTokio<ReadHalf<UnixStream>>>,
    tx: Sender<Event>,
    pidfile: PathBuf,
) -> Result<QemuHandle, SpawnError> {
    let event_reader = tokio::spawn(async move {
        // read events from the API control socket and send to our worker process
        while let Some(event) = events.next().await {
            match event {
                Ok(ev) => match tx.send(ev).await {
                    Ok(_empty) => {}
                    Err(send_err) => {
                        error!("qemu:event send failed: {:?}", send_err);
                    }
                },
                Err(event_err) => {
                    error!("qemu:event read error: {:?}", event_err);
                }
            }
        }
    });

    let content = tokio::fs::read_to_string(&pidfile).await?;
    let content = content.trim();
    let pid = content.parse::<u32>().map_err(SpawnError::ParsePid)?;
    let proc = get_process(pid).ok_or(SpawnError::PidNotFound)?;
    let version = api.execute(query_version {}).await.map_err(SpawnError::Qmp)?;
    let schema = api.execute(query_qmp_schema {}).await.map_err(SpawnError::Qmp)?;
    info!("qemu:monitoring pid:{}", pid);
    info!("qemu:qmp:version {:?}", version);
    debug!("qemu:qmp:schema loaded command_count:{}", schema.len());
    Ok(QemuHandle {
        process: proc,
        ctl: api,
        version: Some(version),
        schema,
        event_reader,
        ga: None,
        ga_info: None,
        ga_task: None,
    })
}

pub async fn try_connect_ga_socket(
    path_socket_ga: &PathBuf,
    timeout_secs: u64,
    retry_interval_millis: u64,
) -> Result<(QapiService<QgaStreamTokio<WriteHalf<UnixStream>>>, JoinHandle<()>), Elapsed> {
    debug!(
        "qemu:qga:socket waiting up to {} seconds for guest agent socket at {} to be available...",
        timeout_secs,
        &path_socket_ga.as_path().display(),
    );

    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            match qapi::futures::QgaStreamTokio::open_uds(path_socket_ga).await {
                Ok(stream) => {
                    info!("qemu:qga:connected version",);
                    let (qga, handle) = stream.spawn_tokio();
                    return (qga, handle);
                }
                Err(open_err) => {
                    warn!(
                        "qemu:qga:open_uds failed with error:{}, trying again after {} milliseconds",
                        open_err, retry_interval_millis,
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(retry_interval_millis)).await;
                }
            }
        }
    })
    .await
}
