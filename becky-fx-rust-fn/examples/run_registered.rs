use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

use async_trait::async_trait;
use becky_engine::control::FxControl;
use becky_engine::empy_implementations::Metadataless;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::ResourceConstraintless;
use becky_engine::storage::Storageless;
use becky_fx_id::FxId;
use becky_fx_rust_fn::{FxRustFn, RustFn, RustFnContext, RustFnExit, RustFnRegistry};

struct SleepFn;

#[async_trait]
impl RustFn for SleepFn {
    async fn run(&self, ctx: RustFnContext) -> RustFnExit {
        let seconds = ctx.args.first().and_then(|arg| arg.parse::<u64>().ok()).unwrap_or(10);
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        RustFnExit::success()
    }
}

#[tokio::main]
async fn main() -> Result<ExitCode, Box<dyn Error>> {
    let mut registry = RustFnRegistry::new();
    registry.register("sleep", SleepFn);

    if let Some(exit) = registry.run_worker_from_env().await? {
        return Ok(ExitCode::from(exit.code as u8));
    }

    let run_dir = std::env::temp_dir().join("becky-fx-rust-fn-example");
    let mut fx = FxRustFn::new("sleep", run_dir).with_args(["20"]);
    let host_id = HostId::String("example-host".to_string());
    let fx_id = FxId::String("sleep".to_string());
    let mut metadata = Metadataless {};
    let constraints = ResourceConstraintless;
    let mut storage = Storageless {};

    fx.fx_allocate(&host_id, &fx_id, &mut metadata, &constraints, &mut storage).await?;
    let mut handle = fx.fx_start(&host_id, &fx_id, &mut metadata, &constraints, &mut storage).await?;
    println!("started function={} pid={} reattached={}", handle.function, handle.pid, handle.reattached);
    println!("status={:?}", fx.fx_status(&mut handle).await?);

    let mut reattached = fx.fx_start(&host_id, &fx_id, &mut metadata, &constraints, &mut storage).await?;
    println!(
        "reattached function={} pid={} reattached={}",
        reattached.function, reattached.pid, reattached.reattached
    );
    println!("reattached_status={:?}", fx.fx_status(&mut reattached).await?);

    fx.fx_destroy(&mut handle).await?;
    reattached.stop_monitor().await;
    Ok(ExitCode::SUCCESS)
}
