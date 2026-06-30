use std::path::PathBuf;

use clap::Parser;
use fuse3::raw::Session;
use fuse3::MountOptions;

mod scanner;
mod tagfs;

#[derive(Parser)]
#[command(name = "rptagfs", about = "Tag-based virtual filesystem")]
struct Args {
    /// Source directory to mirror
    source_dir: PathBuf,
    /// Mount point
    mount_point: PathBuf,
    /// Show hidden files (starting with '.')
    #[arg(long)]
    show_hidden: bool,
    /// Enable debug logging
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let default_filter = if args.debug { "debug" } else { "info" };
    env_logger::Builder::new()
        .filter_level(default_filter.parse().unwrap())
        .parse_default_env()
        .init();

    if !args.source_dir.is_dir() {
        eprintln!(
            "Error: source directory {:?} does not exist or is not a directory",
            args.source_dir
        );
        std::process::exit(1);
    }

    let source_dir = args.source_dir.canonicalize()?;

    log::info!("Mounting at {:?}, show_hidden={}", args.mount_point, args.show_hidden);
    log::info!("Scanning source directory: {:?}", source_dir);
    let scan_result = scanner::scan_tree(&source_dir, args.show_hidden);
    log::info!(
        "Found {} files, {} tag sets, {} unique tags",
        scan_result.files.len(),
        scan_result.tagdirs.len(),
        scan_result.by_tags.len()
    );

    let fs = tagfs::TagFs::new(source_dir, scan_result);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    let mut mount_options = MountOptions::default();
    mount_options.uid(uid).gid(gid);

    let mut mount_handle = Session::new(mount_options)
        .mount_with_unprivileged(fs, &args.mount_point)
        .await?;

    // Wait for either unmount or ctrl+c
    tokio::select! {
        res = &mut mount_handle => {
            res?;
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received interrupt, unmounting...");
            mount_handle.unmount().await?;
        }
    }

    Ok(())
}
