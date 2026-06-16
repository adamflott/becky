mod engine;

use becky_engine::host_id::HostId;
use becky_engine::metadata::MetadataSource;
use becky_engine::state::FxDesiredExecutionState;
use becky_engine::*;
use becky_fx_system_command::FxSystemCommand;
use clap::Parser;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use wora::prelude::*;

use crate::engine::{BareEngine, BareEvent, BareMetric};

#[derive(Clone, Debug, Parser)]
#[command(name = "bare")]
#[command(author, version, about = "Minimal example to illustrate an engine", long_about = None, propagate_version = false)]
#[command(propagate_version = true)]
pub struct BareArgs {
    /// Logging level (trace, debug, info, warn, error)
    #[arg(long, short = 'l', value_name = "LEVEL", default_value_t=filter::LevelFilter::DEBUG)]
    pub log_level: filter::LevelFilter,

    /// Number of copies of the bare minimum VM to run
    #[arg(long, short = 'c', value_name = "N", default_value_t = 10)]
    pub copies: u32,

    /// Directory for per-command PID files
    #[arg(long = "pid-dir", short = 'p', value_name = "DIR")]
    pub pid_dir: Option<PathBuf>,

    /// Command to run
    #[arg(long, short = 'r', value_name = "CMD", default_value = "sleep")]
    pub cmd: String,

    /// Arguments to pass to the command
    #[arg(long, short = 'a', value_name = "ARGS", default_values = ["10"])]
    pub args: Vec<String>,
}

#[derive(Debug)]
pub struct BareRunningCommands {
    cmds: HashMap<u32, FxSystemCommand>,
}

impl BareRunningCommands {
    pub fn new(args: &BareArgs) -> Self {
        let pid_directory = args.pid_dir.clone().unwrap_or_else(|| std::env::temp_dir().join("becky-bare-pids"));
        let mut cmds = HashMap::new();
        for n in 0..args.copies {
            let mut cmd = FxSystemCommand::new(args.cmd.clone(), args.args.clone(), FxDesiredExecutionState::Running);
            cmd.pid_directory = Some(pid_directory.clone());
            cmds.insert(n, cmd);
        }
        BareRunningCommands { cmds }
    }
}

#[async_trait]
impl MetadataSource for BareRunningCommands {
    type MetadataSourceHandle = ();
    type MetadataConnectError = ();

    async fn reconnect(&mut self) -> Result<Self::MetadataSourceHandle, Self::MetadataConnectError> {
        Ok(())
    }

    async fn disconnect(&mut self) -> () {}
}

#[derive(Debug)]
pub struct BareState {
    args: BareArgs,
    cmds: BareRunningCommands,
}

impl BareState {
    pub fn new(args: BareArgs) -> Self {
        Self {
            args: args.clone(),
            cmds: BareRunningCommands::new(&args),
        }
    }
}

type BareSharedState = Arc<RwLock<BareState>>;

#[derive(Default)]
pub struct BareConfig {}

impl Config for BareConfig {
    type ConfigT = BareConfig;
    fn parse_main_config_file(_data: String) -> Result<BareConfig, Box<dyn std::error::Error>> {
        Ok(BareConfig {})
    }
    fn parse_supplemental_config_file(_file_path: PathBuf, _data: String) -> Result<BareConfig, Box<dyn std::error::Error>> {
        Ok(BareConfig {})
    }
}

struct BareFnEngine<BareEvent: Send + 'static, BareMetric, Engine: FxEngine<BareEvent, BareMetric, BareRunningCommands>> {
    pub args: BareArgs,
    pub state: BareSharedState,
    pub log_reload_handle: reload::Handle<filter::LevelFilter, Registry>,
    pub config: BareConfig,
    pub host: Arc<HostId>,
    pub engine: Engine,
    pub ev: PhantomData<BareEvent>,
    pub m: PhantomData<BareMetric>,
    pub mdsrc: PhantomData<BareRunningCommands>,
}

#[derive(Error, Debug)]
pub enum SetupError {
    #[error("todo")]
    TODO,
}

#[async_trait]
impl<BareMetric: Send + Sync + 'static, Engine: FxEngine<EngineEvent<BareEvent>, BareMetric, BareRunningCommands>> App<EngineEvent<BareEvent>, BareMetric>
    for BareFnEngine<EngineEvent<BareEvent>, BareMetric, Engine>
{
    type AppConfig = BareConfig;
    type AppSecrets = NoSecrets;
    type Setup = ();

    fn name(&self) -> &'static str {
        "bare"
    }

    async fn setup(
        &mut self,
        wora: &Wora<EngineEvent<BareEvent>, BareMetric>,
        exec: impl AsyncExecutor<EngineEvent<BareEvent>, BareMetric>,
        fs: impl WFS + 'static,
        _metrics: Sender<O11yEvent<BareMetric>>,
        is_first_boot: bool,
    ) -> Result<Self::Setup, Box<dyn std::error::Error>> {
        let args = BareArgs::try_parse()?;

        let host_id = self.host.clone().to_string();
        trace!("engine[{}]:setup:starting args:{:?}", &host_id, &args);

        let mut md_src = BareRunningCommands::new(&args);

        self.args = args;

        info!("engine[{}]:setup:mdsrc action:connecting", &host_id);
        let maybe_ts = md_src.reconnect().await;
        info!("engine[{}]:setup:mdsrc action:connected", &host_id);
        match maybe_ts {
            Ok(_ts) => {
                debug!("engine[{}]:setup:mdsrc connected", &host_id);

                info!("engine[{}]:setup:starting", &host_id);

                let hi = &wora.host.info;

                if is_first_boot {
                    info!("engine[{}]:setup:host:system_boot action:run", &host_id);
                    match self.engine.host_system_boot(&exec, &fs, hi).await {
                        Ok(result) => {
                            debug!("engine[{}]:setup:host:system_boot result:{:?}", &host_id, result);
                        }
                        Err(err) => {
                            debug!("engine[{}]:setup:host:system_boot error:{:?}", &host_id, err);
                            return Err(Box::new(SetupError::TODO));
                        }
                    }
                } else {
                    warn!("engine[{}]:setup:host:system_boot action:skip cause:already_ran", &host_id);
                }

                debug!(
                    "engine[{}]:setup:host:type:{} is_registered:{}",
                    &host_id,
                    self.engine.register_type(),
                    self.engine.is_registered(&self.host).await.unwrap()
                );
                self.engine.register(&self.host).await.unwrap();
                debug!(
                    "engine[{}]:setup:host:type:{} is_registered:{}",
                    &host_id,
                    self.engine.register_type(),
                    self.engine.is_registered(&self.host).await.unwrap()
                );

                match self.engine.host_system_setup(&exec, &fs, hi).await {
                    Ok(result) => {
                        debug!("engine[{}:setup:host:system_setup result:{:?}", &host_id, result);
                    }
                    Err(err) => {
                        error!("engine[{}]:setup:host:system_setup error:{:?}", &host_id, err);
                        return Err(Box::new(SetupError::TODO));
                    }
                }

                match self.engine.metadata_init().await {
                    Ok(result) => {
                        info!("engine[{}]:setup:metadata:init result:{:?}", &host_id, result);
                    }
                    Err(md_init_err) => {
                        error!("engine[{}]:setup:metadata:init error:{:?}", &host_id, md_init_err);
                        return Err(Box::new(SetupError::TODO));
                    }
                }

                Ok(())
            }
            Err(err) => {
                error!("engine[{}]:setup:mdsrc error:{:?}", &host_id, err);
                return Err(Box::new(SetupError::TODO));
            }
        }
    }

    async fn main(
        &mut self,
        wora: &mut Wora<EngineEvent<BareEvent>, BareMetric>,
        exec: impl AsyncExecutor<EngineEvent<BareEvent>, BareMetric>,
        fs: impl WFS + 'static,
        metrics: Sender<O11yEvent<BareMetric>>,
    ) -> MainRetryAction {
        let host_id = self.host.clone().to_string();

        info!("engine[{}]:main:mainloop:start", &host_id);
        let rc = self.engine.mainloop(wora, exec, fs, metrics).await;
        info!("engine[{}]:main:mainloop:ended", &host_id);
        rc
    }

    async fn end(
        &mut self,
        _wora: &Wora<EngineEvent<BareEvent>, BareMetric>,
        _exec: impl AsyncExecutor<EngineEvent<BareEvent>, BareMetric>,
        _fs: impl WFS + 'static,
        _metrics: Sender<O11yEvent<BareMetric>>,
    ) {
        let host_id = self.host.clone().to_string();

        info!("engine[{}]:end:ending", &host_id);

        let mut st = self.state.write().await;
        info!("engine[{}]:end:mdsrc action:disconnecting", &host_id);
        st.cmds.disconnect().await;
        info!("engine[{}]:end:mdsrc action:disconnected", &host_id);

        info!("engine[{}]:end:ended", &host_id);
    }
}

#[tokio::main]
async fn main() -> Result<(), MainEarlyReturn> {
    let args = BareArgs::parse();

    let filter = args.log_level;
    let (filter, reload_handle) = reload::Layer::new(filter);

    let format = tracing_subscriber::fmt::format()
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true);

    let span_layer = tracing_subscriber::fmt::layer()
        .event_format(format)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

    tracing_subscriber::registry().with(filter).with(span_layer).init();

    let app_state = BareState::new(args.clone());
    let app_state = Arc::new(RwLock::new(app_state));
    let host_id = HostId::String("bare".to_string());
    let engine = BareEngine::new(host_id.clone(), app_state.clone());
    let host = Arc::new(host_id);
    let app = BareFnEngine {
        args: args.clone(),
        state: app_state.clone(),
        log_reload_handle: reload_handle,
        config: BareConfig::default(),
        host: host.clone(),
        engine,
        ev: PhantomData,
        m: PhantomData,
        mdsrc: PhantomData,
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<O11yEvent<BareMetric>>(10);

    let mp = O11yProcessorOptionsBuilder::default()
        .sender(tx)
        .status_interval(std::time::Duration::from_secs(3))
        .flush_interval(std::time::Duration::from_secs(3))
        .host_stats_interval(std::time::Duration::from_secs(3))
        .build()
        .unwrap();

    let fs = PhysicalVFS::new();

    let _o11y_consumer_task = tokio::spawn(o11y(rx));

    match UnixLikeUser::new(app.name(), fs.clone()).await {
        Ok(exec) => exec_async_runner(exec, app, fs.clone(), mp, None).await,
        Err(exec_err) => {
            error!("exec error:{}", exec_err);
            return Err(MainEarlyReturn::Vfs(exec_err));
        }
    }
}

async fn o11y(mut rx: tokio::sync::mpsc::Receiver<O11yEvent<BareMetric>>) {
    let _log_dir = PathBuf::new();
    let _fs = PhysicalVFS::new();

    while let Some(res) = rx.recv().await {
        info!("o11y:event:{:?}", res);
        match res.kind {
            O11yEventKind::Init(_) => {}
            O11yEventKind::Finish => {}
            O11yEventKind::Flush => {}
            O11yEventKind::Clear => {}
            O11yEventKind::Reconnect => {}
            O11yEventKind::Status(_, _) => {}
            O11yEventKind::HostInfo(_) => {}
            O11yEventKind::HostStats(_) => {}
            O11yEventKind::Span(_, _) => {}
            O11yEventKind::Log(_, _, _) => {}
            O11yEventKind::App(_) => {}
            O11yEventKind::ProcessStats(_) => {}
            O11yEventKind::RuntimeMetrics(_) => {}
        }
    }
}
