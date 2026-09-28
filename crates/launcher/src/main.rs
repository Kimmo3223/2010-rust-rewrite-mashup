use std::path::PathBuf;

use assets::{ensure_artifacts_dir, games_root_from_env};

#[cfg(windows)]
mod first_run;

#[global_allocator]
static PROCESS_ALLOCATOR: diag::ProcessCountingAllocator = diag::ProcessCountingAllocator;

fn main() {
    bootstrap::bench::arm();
    prepare_process_root().unwrap_or_else(|e| {
        diag::exit_launch_error(&e);
    });
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    {
        // Also for a shortcut that names a map, so it works on first launch.
        first_run::prepare().unwrap_or_else(|e| first_run::fail(&e));
        if args.is_empty() {
            args.push("menu".into());
        }
    }
    let artifacts = ensure_artifacts_dir().unwrap_or_else(|e| diag::exit_launch_error(&e));
    announce_log(diag::init_log(&artifacts));
    let (mode, acceptance) =
        bootstrap::parse_cli(args.into_iter()).unwrap_or_else(|e| diag::exit_launch_error(&e));
    let games = games_root_from_env().unwrap_or_else(|e| diag::exit_launch_error(&e));
    // On Windows the first-run setup exports the board and rig; elsewhere the
    // converted Skate 3 data comes from `.env` (scripts/setup-skate-linux.sh),
    // so export them here on first launch. Skating stays off if this fails.
    #[cfg(not(windows))]
    if let Some(skate) = std::env::var_os("IW4L_SKATE_ASSETS")
        && let Err(error) = assets::skate_board::ensure(std::path::Path::new(&skate))
    {
        diag::warn!(Launch, "skate: {error}");
    }
    bootstrap::launch(games, artifacts, mode, acceptance);
}

fn prepare_process_root() -> Result<(), String> {
    #[cfg(windows)]
    {
        let exe =
            std::env::current_exe().map_err(|error| format!("cannot locate iw4l.exe: {error}"))?;
        let root = exe
            .parent()
            .ok_or_else(|| format!("iw4l.exe has no parent directory: {}", exe.display()))?;
        std::env::set_current_dir(root).map_err(|error| {
            format!(
                "cannot enter launcher directory {}: {error}",
                root.display()
            )
        })?;
    }
    Ok(())
}

fn announce_log(path: PathBuf) {
    diag::announce_log_stdout(&path, diag::latest_log_path().as_deref());
    diag::info!(Launch, "log: {}", path.display());
}
