use std::error::Error;
use std::time::Duration;

use becky_engine::FxAccounting;
use becky_engine::control::FxControl;
use becky_engine::empy_implementations::Metadataless;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::ResourceConstraintless;
use becky_engine::storage::Storageless;
use becky_fx_docker::{DockerNetwork, FxContainerDocker};
use becky_fx_id::FxId;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Run or reattach to a Docker container through becky-fx-docker")]
struct Args {
    #[arg(long, help = "Docker container name to create, start, or reattach to")]
    name: String,

    #[arg(long, help = "Docker image to pull during allocation and use when creating a missing container")]
    image: String,

    #[arg(long, help = "Optional Docker network to create and attach at container creation time")]
    network: Option<String>,

    #[arg(long = "env", help = "Environment variable passed to docker create, formatted as KEY=VALUE")]
    env: Vec<String>,

    #[arg(long, default_value_t = 3, help = "Seconds to let the monitor observe the container before printing final state")]
    observe_secs: u64,

    #[arg(last = true, help = "Command appended after the image in docker create. Use -- before the command")]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let env = parse_env(args.env)?;

    let mut container = FxContainerDocker::from_image(args.name.clone(), args.image.clone());
    if let Some(network) = args.network {
        container = container.networked(DockerNetwork::new(network));
    }
    for (name, value) in env {
        container = container.env(name, value);
    }
    if !args.command.is_empty() {
        container = container.command(args.command);
    }

    let host_id = HostId::String("becky-fx-docker-example-host".to_string());
    let fx_id = FxId::String(args.name.clone());
    let mut metadata = Metadataless {};
    let constraints = ResourceConstraintless;
    let mut storage = Storageless {};

    println!("allocating docker resources");
    println!("  image: {}", args.image);
    println!("  container: {}", args.name);
    container.fx_allocate(&host_id, &fx_id, &mut metadata, &constraints, &mut storage).await?;

    println!("starting or reattaching to container");
    let mut handle = container.fx_start(&host_id, &fx_id, &mut metadata, &constraints, &mut storage).await?;

    if handle.created {
        println!("  action: created container from image and attached monitor");
    } else {
        println!("  action: reattached to existing container and attached monitor");
    }
    println!("  container id: {}", handle.id);
    println!("  container name: {}", handle.name);

    let status = container.fx_status(&mut handle).await?;
    println!("  immediate status: {:?}", status);
    println!("  monitor status: {:?}", handle.latest_state().await);

    if args.observe_secs > 0 {
        println!("observing for {} seconds", args.observe_secs);
        tokio::time::sleep(Duration::from_secs(args.observe_secs)).await;
    }

    let final_status = container.fx_status(&mut handle).await?;
    let memory = container.memory(&handle).await;
    let disk = container.disk_usage(&handle).await;

    println!("after observation");
    println!("  refreshed status: {:?}", final_status);
    println!("  monitor status: {:?}", handle.latest_state().await);
    println!("  memory bytes: {}", memory);
    println!("  disk read bytes: {}", disk.read_bytes);
    println!("  disk written bytes: {}", disk.written_bytes);

    handle.stop_monitor().await;
    Ok(())
}

fn parse_env(values: Vec<String>) -> Result<Vec<(String, String)>, String> {
    let mut parsed = Vec::with_capacity(values.len());
    for value in values {
        let Some((key, val)) = value.split_once('=') else {
            return Err(format!("invalid --env value {value:?}; expected KEY=VALUE"));
        };
        if key.is_empty() {
            return Err(format!("invalid --env value {value:?}; key cannot be empty"));
        }
        parsed.push((key.to_string(), val.to_string()));
    }
    Ok(parsed)
}
