use anyhow::Result;
use clap::{Parser, Subcommand};
use control_plane_host::update;
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    version,
    about = "Verify and stage publisher-signed Control Plane updates"
)]
struct Cli {
    #[arg(long)]
    feed: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Check,
    Resume {
        #[arg(long, required = true)]
        worker_data: Vec<PathBuf>,
    },
    Stage {
        #[arg(long)]
        directory: PathBuf,
    },
    Apply {
        artifact: PathBuf,
        #[arg(long, required = true)]
        worker_data: Vec<PathBuf>,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    if let Command::Resume { worker_data } = &cli.command {
        for data in worker_data {
            control_plane_host::update_drain::resume(data)?;
        }
        println!("Local worker maintenance cleared. Work is not replayed.");
        return Ok(());
    }
    let feed = match cli.feed.as_deref() {
        Some(v) => v,
        None => update::default_feed()?,
    };
    match cli.command {
        Command::Resume { .. } => unreachable!(),
        Command::Check => println!("{}", update::check(feed, env!("CARGO_PKG_VERSION")).await?),
        Command::Stage { directory } => {
            let v = update::fetch(feed, env!("CARGO_PKG_VERSION")).await?;
            let version = v.manifest.version.clone();
            let p = update::stage(&directory, v).await?;
            println!(
                "{}",
                serde_json::json!({"verified":true,"version":version,"artifact":p})
            );
        }
        Command::Apply {
            artifact,
            worker_data,
        } => {
            update::apply(&artifact, feed, env!("CARGO_PKG_VERSION"), &worker_data).await?;
            println!("OS installer opened. Complete its confirmation, then resume each worker after installation.");
        }
    }
    Ok(())
}
