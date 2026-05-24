use anyhow::{Context, Result};
use crossbeam::channel::{Receiver, Sender};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub type TreeIndex = usize;
const MAX_SCAN_JOBS: usize = 2;

#[derive(Debug)]
pub struct ActiveScan {
    nodes: Vec<Node>,
    event_rx: Receiver<ScanEvent>,
    event_tx: Sender<ScanEvent>,
    queued: VecDeque<(TreeIndex, ScanOptions)>,
    pub root: PathBuf,
    pub finished: bool,
    pub stats: ScanStats,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOptions {
    pub apparent_size: bool,
    pub cross_filesystems: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub entries_traversed: u64,
    pub io_errors: u64,
    pub total_bytes: Option<u128>,
    pub active_jobs: usize,
}

#[derive(Debug, Clone)]
struct Node {
    name: String,
    path: PathBuf,
    size: u128,
    modified: SystemTime,
    is_dir: bool,
    entry_count: Option<u64>,
    metadata_error: bool,
    parent: Option<TreeIndex>,
    children: Vec<TreeIndex>,
    scanned: bool,
    scanning: bool,
}

#[derive(Debug)]
struct ScanEvent {
    parent: TreeIndex,
    children: Vec<ScannedEntry>,
    stats: ScanStats,
}

#[derive(Debug)]
struct ScannedEntry {
    name: String,
    path: PathBuf,
    size: u128,
    modified: SystemTime,
    is_dir: bool,
    entry_count: Option<u64>,
    metadata_error: bool,
}

#[derive(Debug, Clone)]
pub struct EntryView {
    pub index: TreeIndex,
    pub name: String,
    pub path: PathBuf,
    pub size: u128,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub entry_count: Option<u64>,
    pub metadata_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryScanState {
    NotDirectory,
    Pending,
    Scanning,
    Scanned,
}

impl ActiveScan {
    pub fn start(root: PathBuf, options: ScanOptions) -> Result<Self> {
        let metadata = fs::symlink_metadata(&root)
            .with_context(|| format!("failed to read metadata for {}", root.display()))?;
        let (event_tx, event_rx) = crossbeam::channel::unbounded();
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let root_node = Node {
            name: root.display().to_string(),
            path: root.clone(),
            size: disk_size(&metadata, options.apparent_size),
            modified,
            is_dir: metadata.is_dir(),
            entry_count: Some(0),
            metadata_error: false,
            parent: None,
            children: Vec::new(),
            scanned: false,
            scanning: false,
        };

        let mut scan = Self {
            nodes: vec![root_node],
            event_rx,
            event_tx,
            queued: VecDeque::new(),
            root,
            finished: false,
            stats: ScanStats::default(),
        };
        scan.scan_children(0, options);
        Ok(scan)
    }

    pub fn drain_events(&mut self, limit: usize) -> bool {
        let mut changed = false;
        for _ in 0..limit {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            changed = true;
            self.integrate_event(event);
        }
        self.start_queued_jobs();
        self.finished = self.stats.active_jobs == 0;
        changed
    }

    pub fn ensure_scanned(&mut self, index: TreeIndex, options: ScanOptions) {
        if let Some(node) = self.nodes.get(index)
            && node.is_dir
            && !node.scanned
            && !node.scanning
        {
            self.enqueue_scan(index, options);
        }
    }

    pub fn prefetch_children(&mut self, index: TreeIndex, options: ScanOptions) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        let children = node.children.clone();
        for child in children {
            if self.stats.active_jobs + self.queued.len() >= MAX_SCAN_JOBS {
                break;
            }
            self.ensure_scanned(child, options);
        }
    }

    fn enqueue_scan(&mut self, index: TreeIndex, options: ScanOptions) {
        if self
            .queued
            .iter()
            .any(|(queued_index, _)| *queued_index == index)
        {
            return;
        }
        if self.stats.active_jobs < MAX_SCAN_JOBS {
            self.scan_children(index, options);
        } else {
            self.queued.push_back((index, options));
        }
    }

    fn start_queued_jobs(&mut self) {
        while self.stats.active_jobs < MAX_SCAN_JOBS {
            let Some((index, options)) = self.queued.pop_front() else {
                break;
            };
            self.scan_children(index, options);
        }
    }

    fn scan_children(&mut self, index: TreeIndex, options: ScanOptions) {
        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        if !node.is_dir || node.scanned || node.scanning {
            return;
        }

        node.scanning = true;
        self.finished = false;
        self.stats.active_jobs += 1;
        let path = node.path.clone();
        let tx = self.event_tx.clone();

        std::thread::Builder::new()
            .name("spacesniffer-level-scan".into())
            .spawn(move || {
                let event = scan_directory_level(index, path, options);
                let _ = tx.send(event);
            })
            .expect("spawn directory scanner");
    }

    fn integrate_event(&mut self, event: ScanEvent) {
        if self.nodes.get(event.parent).is_none() {
            return;
        }

        let parent_size = event.children.iter().map(|child| child.size).sum();
        let parent_count = event.children.len() as u64;
        let mut child_indices = Vec::with_capacity(event.children.len());

        for child in event.children {
            let index = self.nodes.len();
            child_indices.push(index);
            self.nodes.push(Node {
                name: child.name,
                path: child.path,
                size: child.size,
                modified: child.modified,
                is_dir: child.is_dir,
                entry_count: child.entry_count,
                metadata_error: child.metadata_error,
                parent: Some(event.parent),
                children: Vec::new(),
                scanned: false,
                scanning: false,
            });
        }

        let parent = &mut self.nodes[event.parent];
        parent.children = child_indices;
        parent.scanned = true;
        parent.scanning = false;
        parent.size = parent_size;
        parent.entry_count = Some(parent_count);

        self.stats.entries_traversed += event.stats.entries_traversed;
        self.stats.io_errors += event.stats.io_errors;
        self.stats.total_bytes = self.nodes.first().map(|node| node.size);
        self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
        self.start_queued_jobs();
    }
}

pub fn children(scan: &ActiveScan, parent: TreeIndex) -> Vec<EntryView> {
    let mut entries = scan
        .nodes
        .get(parent)
        .into_iter()
        .flat_map(|node| node.children.iter())
        .filter_map(|idx| entry_view(scan, *idx))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    entries
}

pub fn entry_view(scan: &ActiveScan, index: TreeIndex) -> Option<EntryView> {
    let node = scan.nodes.get(index)?;
    Some(EntryView {
        index,
        name: node.name.clone(),
        path: node.path.clone(),
        size: node.size,
        modified: node.modified,
        is_dir: node.is_dir,
        entry_count: node.entry_count,
        metadata_error: node.metadata_error,
    })
}

pub fn parent(scan: &ActiveScan, index: TreeIndex) -> Option<TreeIndex> {
    scan.nodes.get(index)?.parent
}

pub fn directory_scan_state(scan: &ActiveScan, index: TreeIndex) -> Option<DirectoryScanState> {
    let node = scan.nodes.get(index)?;
    if !node.is_dir {
        return Some(DirectoryScanState::NotDirectory);
    }
    if node.scanning {
        Some(DirectoryScanState::Scanning)
    } else if node.scanned {
        Some(DirectoryScanState::Scanned)
    } else {
        Some(DirectoryScanState::Pending)
    }
}

pub fn path_for_node(scan: &ActiveScan, index: TreeIndex) -> Option<PathBuf> {
    Some(scan.nodes.get(index)?.path.clone())
}

pub fn is_root_path(path: &Path) -> bool {
    path.parent().is_none()
}

fn scan_directory_level(parent: TreeIndex, path: PathBuf, options: ScanOptions) -> ScanEvent {
    let root_dev = fs::symlink_metadata(&path).map(|meta| meta.dev()).ok();
    let mut children = Vec::new();
    let mut stats = ScanStats {
        active_jobs: 1,
        ..Default::default()
    };

    let Ok(read_dir) = fs::read_dir(&path) else {
        stats.io_errors += 1;
        return ScanEvent {
            parent,
            children,
            stats,
        };
    };

    for entry in read_dir {
        stats.entries_traversed += 1;
        let Ok(entry) = entry else {
            stats.io_errors += 1;
            continue;
        };
        let child_path = entry.path();
        match fs::symlink_metadata(&child_path) {
            Ok(metadata) => {
                let is_dir = metadata.is_dir();
                let mut visited = HashSet::new();
                let summary =
                    summarize_path(&child_path, &metadata, root_dev, options, &mut visited);
                stats.entries_traversed += summary.entries_seen;
                stats.io_errors += summary.io_errors;
                children.push(ScannedEntry {
                    name: entry.file_name().to_string_lossy().to_string(),
                    path: child_path,
                    size: summary.size,
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    is_dir,
                    entry_count: is_dir.then_some(summary.entry_count),
                    metadata_error: summary.metadata_error,
                });
            }
            Err(_) => {
                stats.io_errors += 1;
                children.push(ScannedEntry {
                    name: entry.file_name().to_string_lossy().to_string(),
                    path: child_path,
                    size: 0,
                    modified: SystemTime::UNIX_EPOCH,
                    is_dir: false,
                    entry_count: None,
                    metadata_error: true,
                });
            }
        }
    }

    children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    stats.total_bytes = Some(children.iter().map(|child| child.size).sum());
    ScanEvent {
        parent,
        children,
        stats,
    }
}

#[derive(Debug, Default)]
struct Summary {
    size: u128,
    entry_count: u64,
    entries_seen: u64,
    io_errors: u64,
    metadata_error: bool,
}

fn summarize_path(
    path: &Path,
    metadata: &fs::Metadata,
    root_dev: Option<u64>,
    options: ScanOptions,
    visited: &mut HashSet<(u64, u64)>,
) -> Summary {
    let mut summary = Summary {
        size: disk_size(metadata, options.apparent_size),
        entry_count: 1,
        entries_seen: 1,
        ..Default::default()
    };

    if !options.cross_filesystems && root_dev.is_some_and(|root_dev| metadata.dev() != root_dev) {
        return summary;
    }

    if metadata.nlink() > 1 && !visited.insert((metadata.dev(), metadata.ino())) {
        summary.size = 0;
        return summary;
    }

    if !metadata.is_dir() {
        return summary;
    }

    let mut queue = VecDeque::from([path.to_path_buf()]);
    while let Some(dir) = queue.pop_back() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            summary.io_errors += 1;
            summary.metadata_error = true;
            continue;
        };

        for entry in read_dir {
            summary.entries_seen += 1;
            let Ok(entry) = entry else {
                summary.io_errors += 1;
                continue;
            };
            let entry_path = entry.path();
            match fs::symlink_metadata(&entry_path) {
                Ok(metadata) => {
                    if !options.cross_filesystems
                        && root_dev.is_some_and(|root_dev| metadata.dev() != root_dev)
                    {
                        continue;
                    }
                    if metadata.nlink() > 1 && !visited.insert((metadata.dev(), metadata.ino())) {
                        continue;
                    }
                    summary.size += disk_size(&metadata, options.apparent_size);
                    summary.entry_count += 1;
                    if metadata.is_dir() {
                        queue.push_back(entry_path);
                    }
                }
                Err(_) => {
                    summary.io_errors += 1;
                    summary.metadata_error = true;
                }
            }
        }
    }

    summary
}

fn disk_size(metadata: &fs::Metadata, apparent_size: bool) -> u128 {
    if apparent_size {
        metadata.len() as u128
    } else {
        metadata.blocks() as u128 * 512
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_temp_directory_one_level_and_summarizes_children() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("nested")).expect("create nested");
        fs::write(dir.path().join("nested/file.txt"), b"hello").expect("write file");

        let mut scan = ActiveScan::start(
            dir.path().to_path_buf(),
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
        )
        .expect("start scan");

        for _ in 0..500 {
            scan.drain_events(100);
            if scan.finished {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let nested = children(&scan, 0)
            .into_iter()
            .find(|entry| entry.name == "nested")
            .expect("nested directory");
        assert!(nested.is_dir);
        assert!(nested.size >= 5);
        assert!(entry_view(&scan, nested.index).is_some());
    }

    #[test]
    fn prefetch_respects_worker_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["a", "b", "c", "d"] {
            fs::create_dir(dir.path().join(name)).expect("create child dir");
        }

        let mut scan = ActiveScan::start(
            dir.path().to_path_buf(),
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
        )
        .expect("start scan");

        for _ in 0..500 {
            scan.drain_events(100);
            if scan.finished {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        scan.prefetch_children(
            0,
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
        );

        assert!(scan.stats.active_jobs <= MAX_SCAN_JOBS);
        assert!(scan.stats.active_jobs + scan.queued.len() <= MAX_SCAN_JOBS);
    }
}
