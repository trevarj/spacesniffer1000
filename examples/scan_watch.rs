use crossbeam::channel::RecvTimeoutError;
use spacesniffer1000::scan::{ActiveScan, ScanOptions, ScanWake, SharedSummaryCache};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("current dir"));
    let max_seconds = args
        .next()
        .and_then(|arg| arg.parse::<u64>().ok())
        .unwrap_or(30);

    let (wake_tx, wake_rx) = crossbeam::channel::unbounded();
    let wake = ScanWake::new(move || {
        let _ = wake_tx.send(());
    });
    let options = ScanOptions {
        apparent_size: false,
        cross_filesystems: false,
    };
    let mut scan = ActiveScan::start_with_cache_and_wake(
        path.clone(),
        options,
        SharedSummaryCache::default(),
        Some(wake),
    )
    .expect("start scan");

    let started = Instant::now();
    let deadline = Duration::from_secs(max_seconds);
    let mut last_entries = 0;
    let mut last_total = 0;
    let mut entry_increases = 0;
    let mut size_increases = 0;
    let mut wake_count = 0;
    let mut idle_timeouts = 0;

    println!("watching={} max_seconds={max_seconds}", path.display());
    while !scan.finished && started.elapsed() < deadline {
        match wake_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => wake_count += 1,
            Err(RecvTimeoutError::Timeout) => {
                idle_timeouts += 1;
                println!(
                    "idle_timeout elapsed_ms={} active={} queued={} background={}",
                    started.elapsed().as_millis(),
                    scan.progress().active_jobs,
                    scan.progress().queued_jobs,
                    scan.progress().background_jobs
                );
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let changed = scan.drain_events(512);
        let progress = scan.progress();
        let total = scan.stats.total_bytes.unwrap_or(0);
        if progress.entries_traversed > last_entries {
            entry_increases += 1;
            last_entries = progress.entries_traversed;
        }
        if total > last_total {
            size_increases += 1;
            last_total = total;
        }

        println!(
            "wake={wake_count} changed={changed} elapsed_ms={} entries={} total_bytes={} active={} queued={} background={} finished={}",
            started.elapsed().as_millis(),
            progress.entries_traversed,
            total,
            progress.active_jobs,
            progress.queued_jobs,
            progress.background_jobs,
            progress.finished
        );
    }

    scan.drain_events(usize::MAX);
    println!(
        "result finished={} elapsed_ms={} entries={} total_bytes={} wakes={} entry_increases={} size_increases={} idle_timeouts={}",
        scan.finished,
        started.elapsed().as_millis(),
        scan.stats.entries_traversed,
        scan.stats.total_bytes.unwrap_or(0),
        wake_count,
        entry_increases,
        size_increases,
        idle_timeouts
    );
}
