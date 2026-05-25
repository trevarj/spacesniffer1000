use spacesniffer1000::scan::{self, ActiveScan, ScanOptions};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const DIRS: usize = 72;
const FILES_PER_DIR: usize = 180;
const BYTES_PER_FILE: usize = 64;
const ITERATIONS: usize = 7;

fn main() {
    let root = bench_root();
    if !root.exists() {
        create_tree(&root);
    }

    let mut full_scan_times = Vec::with_capacity(ITERATIONS);
    let mut first_children_times = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let measurement = measure_scan(&root);
        full_scan_times.push(measurement.full_scan);
        first_children_times.push(measurement.first_children);
    }

    println!("tree={}", root.display());
    println!("dirs={DIRS} files={}", DIRS * FILES_PER_DIR);
    println!(
        "first_children_avg_ms={:.2}",
        average_ms(&first_children_times)
    );
    println!("full_scan_avg_ms={:.2}", average_ms(&full_scan_times));
}

#[derive(Debug, Clone, Copy)]
struct Measurement {
    first_children: Duration,
    full_scan: Duration,
}

fn measure_scan(root: &Path) -> Measurement {
    let options = ScanOptions {
        apparent_size: true,
        cross_filesystems: false,
    };
    let start = Instant::now();
    let mut scan = ActiveScan::start(root.to_path_buf(), options).expect("start scan");
    let mut first_children = None;

    while !scan.finished {
        scan.drain_events(1024);
        if first_children.is_none() && !scan::children(&scan, 0).is_empty() {
            first_children = Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    Measurement {
        first_children: first_children.unwrap_or_else(|| start.elapsed()),
        full_scan: start.elapsed(),
    }
}

fn create_tree(root: &Path) {
    fs::create_dir_all(root).expect("create bench root");
    let payload = vec![b'x'; BYTES_PER_FILE];
    for dir_index in 0..DIRS {
        let dir = root.join(format!("dir-{dir_index:03}"));
        fs::create_dir_all(&dir).expect("create child dir");
        for file_index in 0..FILES_PER_DIR {
            fs::write(dir.join(format!("file-{file_index:03}.bin")), &payload)
                .expect("write bench file");
        }
    }
}

fn average_ms(values: &[Duration]) -> f64 {
    let total = values.iter().map(Duration::as_secs_f64).sum::<f64>();
    total * 1000.0 / values.len() as f64
}

fn bench_root() -> PathBuf {
    let stamp = SystemTime::UNIX_EPOCH
        .elapsed()
        .map(|elapsed| elapsed.as_secs() / 86_400)
        .unwrap_or_default();
    std::env::temp_dir().join(format!("spacesniffer1000-scan-bench-{stamp}"))
}
