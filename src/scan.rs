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
    queued: VecDeque<ScanJob>,
    background_queued: VecDeque<ScanJob>,
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
    boundary_dev: Option<u64>,
    scanned: bool,
    scanning: bool,
    summarized: bool,
    summarizing: bool,
}

#[derive(Debug)]
enum ScanEvent {
    Children {
        parent: TreeIndex,
        children: Vec<ScannedEntry>,
        options: ScanOptions,
        stats: ScanStats,
    },
    Summary {
        index: TreeIndex,
        summary: Summary,
    },
}

#[derive(Debug)]
enum ScanJob {
    Children(TreeIndex, ScanOptions),
    Summary(TreeIndex, ScanOptions),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobPriority {
    Interactive,
    Background,
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
    boundary_dev: Option<u64>,
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
    pub size_pending: bool,
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
            boundary_dev: Some(metadata.dev()),
            scanned: false,
            scanning: false,
            summarized: !metadata.is_dir(),
            summarizing: false,
        };

        let mut scan = Self {
            nodes: vec![root_node],
            event_rx,
            event_tx,
            queued: VecDeque::new(),
            background_queued: VecDeque::new(),
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
        self.finished = self.stats.active_jobs == 0
            && self.queued.is_empty()
            && self.background_queued.is_empty();
        if self.finished {
            self.mark_completed_summary_flags();
        }
        changed
    }

    pub fn ensure_scanned(&mut self, index: TreeIndex, options: ScanOptions) {
        if let Some(node) = self.nodes.get(index)
            && node.is_dir
            && !node.scanned
            && !node.scanning
        {
            self.enqueue_job(ScanJob::Children(index, options), JobPriority::Interactive);
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

    fn enqueue_job(&mut self, job: ScanJob, priority: JobPriority) {
        if self.job_is_redundant(&job) {
            return;
        }
        if self.can_start_job(priority) {
            self.start_job(job);
            return;
        }

        match priority {
            JobPriority::Interactive => self.queued.push_back(job),
            JobPriority::Background => self.background_queued.push_back(job),
        }
    }

    fn start_queued_jobs(&mut self) {
        while self.stats.active_jobs < MAX_SCAN_JOBS {
            let Some(job) = self.queued.pop_front() else {
                break;
            };
            self.start_job(job);
        }

        if self.stats.active_jobs == 0
            && let Some(job) = self.background_queued.pop_front()
        {
            self.start_job(job);
        }
    }

    fn start_job(&mut self, job: ScanJob) {
        match job {
            ScanJob::Children(index, options) => self.scan_children(index, options),
            ScanJob::Summary(index, options) => self.summarize_node(index, options),
        }
    }

    fn job_is_redundant(&self, job: &ScanJob) -> bool {
        match job {
            ScanJob::Children(index, _) => {
                let Some(node) = self.nodes.get(*index) else {
                    return true;
                };
                node.scanned
                    || node.scanning
                    || self.queued.iter().any(|queued| {
                        matches!(queued, ScanJob::Children(queued_index, _) if queued_index == index)
                    })
                    || self.background_queued.iter().any(|queued| {
                        matches!(queued, ScanJob::Children(queued_index, _) if queued_index == index)
                    })
            }
            ScanJob::Summary(index, _) => {
                let Some(node) = self.nodes.get(*index) else {
                    return true;
                };
                node.summarized
                    || node.summarizing
                    || self.queued.iter().any(|queued| {
                        matches!(queued, ScanJob::Summary(queued_index, _) if queued_index == index)
                    })
                    || self.background_queued.iter().any(|queued| {
                        matches!(queued, ScanJob::Summary(queued_index, _) if queued_index == index)
                    })
            }
        }
    }

    fn can_start_job(&self, priority: JobPriority) -> bool {
        match priority {
            JobPriority::Interactive => self.stats.active_jobs < MAX_SCAN_JOBS,
            // Background summaries run only when the scanner is otherwise idle. That keeps one
            // worker slot available for click-driven subtree scans while sizes refine gradually.
            JobPriority::Background => self.stats.active_jobs == 0 && self.queued.is_empty(),
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

    fn summarize_node(&mut self, index: TreeIndex, options: ScanOptions) {
        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        if node.summarized || node.summarizing {
            return;
        }

        node.summarizing = true;
        self.finished = false;
        self.stats.active_jobs += 1;
        let path = node.path.clone();
        let boundary_dev = node.boundary_dev;
        let tx = self.event_tx.clone();

        std::thread::Builder::new()
            .name("spacesniffer-summary-scan".into())
            .spawn(move || {
                let summary = summarize_path_from_fs(&path, boundary_dev, options);
                let _ = tx.send(ScanEvent::Summary { index, summary });
            })
            .expect("spawn summary scanner");
    }

    fn integrate_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Children {
                parent,
                children,
                options,
                stats,
            } => self.integrate_children(parent, children, options, stats),
            ScanEvent::Summary { index, summary } => self.integrate_summary(index, summary),
        }
        self.start_queued_jobs();
    }

    fn integrate_children(
        &mut self,
        parent: TreeIndex,
        children: Vec<ScannedEntry>,
        options: ScanOptions,
        stats: ScanStats,
    ) {
        if self.nodes.get(parent).is_none() {
            self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
            return;
        }

        let parent_size = children.iter().map(|child| child.size).sum();
        let parent_count = children.len() as u64;
        let mut child_indices = Vec::with_capacity(children.len());

        for child in children {
            let index = self.nodes.len();
            let should_summarize = child.is_dir;
            child_indices.push(index);
            self.nodes.push(Node {
                name: child.name,
                path: child.path,
                size: child.size,
                modified: child.modified,
                is_dir: child.is_dir,
                entry_count: child.entry_count,
                metadata_error: child.metadata_error,
                parent: Some(parent),
                children: Vec::new(),
                boundary_dev: child.boundary_dev,
                scanned: false,
                scanning: false,
                summarized: !should_summarize,
                summarizing: false,
            });
            if should_summarize {
                self.enqueue_job(ScanJob::Summary(index, options), JobPriority::Background);
            }
        }

        let update_parent_size = !self.nodes[parent].summarized;
        let old_size = self.nodes[parent].size;
        {
            let parent_node = &mut self.nodes[parent];
            parent_node.children = child_indices;
            parent_node.scanned = true;
            parent_node.scanning = false;
            if update_parent_size {
                parent_node.size = parent_size;
                parent_node.entry_count = Some(parent_count);
            }
        }
        if update_parent_size {
            self.propagate_size_delta(parent, old_size, parent_size);
        }

        self.stats.entries_traversed += stats.entries_traversed;
        self.stats.io_errors += stats.io_errors;
        self.stats.total_bytes = self.nodes.first().map(|node| node.size);
        self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
    }

    fn integrate_summary(&mut self, index: TreeIndex, summary: Summary) {
        if self.nodes.get(index).is_none() {
            self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
            return;
        }

        let old_size = self.nodes[index].size;
        {
            let node = &mut self.nodes[index];
            node.size = summary.size;
            node.entry_count = node.is_dir.then_some(summary.entry_count);
            node.metadata_error |= summary.metadata_error;
            node.summarized = true;
            node.summarizing = false;
        }
        self.propagate_size_delta(index, old_size, summary.size);

        self.stats.entries_traversed += summary.entries_seen;
        self.stats.io_errors += summary.io_errors;
        self.stats.total_bytes = self.nodes.first().map(|node| node.size);
        self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
    }

    fn propagate_size_delta(&mut self, index: TreeIndex, old_size: u128, new_size: u128) {
        let delta = new_size as i128 - old_size as i128;
        if delta == 0 {
            return;
        }

        let mut current = self.nodes.get(index).and_then(|node| node.parent);
        while let Some(parent_index) = current {
            let parent = &mut self.nodes[parent_index];
            parent.size = parent.size.saturating_add_signed(delta);
            current = parent.parent;
        }
    }

    fn mark_completed_summary_flags(&mut self) {
        for index in (0..self.nodes.len()).rev() {
            if !self.nodes[index].is_dir
                || self.nodes[index].summarized
                || self.nodes[index].summarizing
                || !self.nodes[index].scanned
            {
                continue;
            }
            let complete = self.nodes[index]
                .children
                .iter()
                .all(|child| self.nodes.get(*child).is_some_and(|node| node.summarized));
            if complete {
                self.nodes[index].summarized = true;
            }
        }
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
        size_pending: node.is_dir && !node.summarized,
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
        return ScanEvent::Children {
            parent,
            children,
            options,
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
                children.push(ScannedEntry {
                    name: entry.file_name().to_string_lossy().to_string(),
                    path: child_path,
                    size: disk_size(&metadata, options.apparent_size),
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    is_dir,
                    entry_count: None,
                    metadata_error: false,
                    boundary_dev: root_dev,
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
                    boundary_dev: root_dev,
                });
            }
        }
    }

    children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    stats.total_bytes = Some(children.iter().map(|child| child.size).sum());
    ScanEvent::Children {
        parent,
        children,
        options,
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

fn summarize_path_from_fs(path: &Path, root_dev: Option<u64>, options: ScanOptions) -> Summary {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let mut visited = HashSet::new();
            summarize_path(path, &metadata, root_dev, options, &mut visited)
        }
        Err(_) => Summary {
            io_errors: 1,
            metadata_error: true,
            ..Default::default()
        },
    }
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

    fn wait_until<F>(scan: &mut ActiveScan, mut done: F)
    where
        F: FnMut(&ActiveScan) -> bool,
    {
        for _ in 0..500 {
            scan.drain_events(100);
            if done(scan) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn queued_child_jobs(scan: &ActiveScan, index: TreeIndex) -> usize {
        scan.queued
            .iter()
            .chain(scan.background_queued.iter())
            .filter(
                |job| matches!(job, ScanJob::Children(queued_index, _) if *queued_index == index),
            )
            .count()
    }

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

        wait_until(&mut scan, |scan| scan.finished);

        let nested = children(&scan, 0)
            .into_iter()
            .find(|entry| entry.name == "nested")
            .expect("nested directory");
        assert!(nested.is_dir);
        assert!(nested.size >= 5);
        assert!(entry_view(&scan, nested.index).is_some());
    }

    #[test]
    fn lists_children_before_recursive_summary_finishes() {
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

        wait_until(&mut scan, |scan| !children(scan, 0).is_empty());

        let nested = children(&scan, 0)
            .into_iter()
            .find(|entry| entry.name == "nested")
            .expect("nested directory listed");
        assert!(nested.is_dir);
        assert!(nested.size_pending);
        assert_eq!(nested.entry_count, None);

        wait_until(&mut scan, |scan| scan.finished);

        let nested = entry_view(&scan, nested.index).expect("nested directory summarized");
        assert!(!nested.size_pending);
        assert!(nested.size >= 5);
        assert!(nested.entry_count.is_some_and(|count| count >= 2));
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

        wait_until(&mut scan, |scan| !children(scan, 0).is_empty());

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

    #[test]
    fn background_summaries_do_not_fill_interactive_capacity() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["a", "b", "c", "d"] {
            fs::create_dir(dir.path().join(name)).expect("create child dir");
            fs::write(dir.path().join(name).join("file.txt"), b"hello").expect("write file");
        }

        let mut scan = ActiveScan::start(
            dir.path().to_path_buf(),
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
        )
        .expect("start scan");

        wait_until(&mut scan, |scan| !children(scan, 0).is_empty());

        assert!(scan.stats.active_jobs <= 1);
        assert_eq!(scan.queued.len(), 0);
        assert!(scan.background_queued.len() >= 3);

        let child = children(&scan, 0)
            .into_iter()
            .find(|entry| entry.is_dir)
            .expect("child directory");
        scan.ensure_scanned(
            child.index,
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
        );

        assert_eq!(scan.queued.len(), 0);
        assert!(scan.nodes[child.index].scanning || scan.nodes[child.index].scanned);
        assert!(scan.stats.active_jobs <= MAX_SCAN_JOBS);
    }

    #[test]
    fn already_scanned_subtree_is_not_rescanned() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("nested/child")).expect("create nested child");
        fs::write(dir.path().join("nested/child/file.txt"), b"hello").expect("write file");

        let options = ScanOptions {
            apparent_size: true,
            cross_filesystems: false,
        };
        let mut scan = ActiveScan::start(dir.path().to_path_buf(), options).expect("start scan");

        wait_until(&mut scan, |scan| !children(scan, 0).is_empty());
        let nested = children(&scan, 0)
            .into_iter()
            .find(|entry| entry.name == "nested")
            .expect("nested directory");

        scan.ensure_scanned(nested.index, options);
        wait_until(&mut scan, |scan| scan.nodes[nested.index].scanned);

        let active_before = scan.stats.active_jobs;
        let queued_before = scan.queued.len();
        let background_before = scan.background_queued.len();

        scan.ensure_scanned(nested.index, options);
        scan.ensure_scanned(nested.index, options);

        assert_eq!(scan.stats.active_jobs, active_before);
        assert_eq!(scan.queued.len(), queued_before);
        assert_eq!(scan.background_queued.len(), background_before);
        assert_eq!(queued_child_jobs(&scan, nested.index), 0);
    }
}
