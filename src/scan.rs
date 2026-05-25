use anyhow::{Context, Result};
use crossbeam::channel::{Receiver, Sender};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

pub type TreeIndex = usize;
const MAX_SCAN_JOBS: usize = 2;
const PROVISIONAL_PENDING_LAYOUT_SIZE: u128 = 1024 * 1024;

#[derive(Debug)]
pub struct ActiveScan {
    nodes: Vec<Node>,
    event_rx: Receiver<ScanEvent>,
    event_tx: Sender<ScanEvent>,
    cancel_flag: Arc<AtomicBool>,
    queued: VecDeque<ScanJob>,
    background_queued: VecDeque<ScanJob>,
    pub root: PathBuf,
    pub finished: bool,
    pub canceled: bool,
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
    pub error_samples: Vec<IoErrorSample>,
}

#[derive(Debug, Clone)]
pub struct IoErrorSample {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgress {
    pub active_jobs: usize,
    pub queued_jobs: usize,
    pub background_jobs: usize,
    pub entries_traversed: u64,
    pub io_errors: u64,
    pub finished: bool,
    pub canceled: bool,
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
    pub layout_size: u128,
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
            cancel_flag: Arc::new(AtomicBool::new(false)),
            queued: VecDeque::new(),
            background_queued: VecDeque::new(),
            root,
            finished: false,
            canceled: false,
            stats: ScanStats::default(),
        };
        scan.scan_children(0, options);
        Ok(scan)
    }

    pub fn drain_events(&mut self, limit: usize) -> bool {
        if self.canceled {
            while self.event_rx.try_recv().is_ok() {}
            self.finished = true;
            return false;
        }

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

    pub fn cancel(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.queued.clear();
        self.background_queued.clear();
        for node in &mut self.nodes {
            node.scanning = false;
            node.summarizing = false;
        }
        self.stats.active_jobs = 0;
        self.finished = true;
        self.canceled = true;
    }

    pub fn progress(&self) -> ScanProgress {
        ScanProgress {
            active_jobs: self.stats.active_jobs,
            queued_jobs: self.queued.len(),
            background_jobs: self.background_queued.len(),
            entries_traversed: self.stats.entries_traversed,
            io_errors: self.stats.io_errors,
            finished: self.finished,
            canceled: self.canceled,
        }
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
        let cancel_flag = self.cancel_flag.clone();

        std::thread::Builder::new()
            .name("spacesniffer-level-scan".into())
            .spawn(move || {
                let event = scan_directory_level(index, path, options, &cancel_flag);
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
        let cancel_flag = self.cancel_flag.clone();

        std::thread::Builder::new()
            .name("spacesniffer-summary-scan".into())
            .spawn(move || {
                let summary = summarize_path_from_fs(&path, boundary_dev, options, &cancel_flag);
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
        append_error_samples(&mut self.stats.error_samples, stats.error_samples);
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
        append_error_samples(&mut self.stats.error_samples, summary.error_samples);
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

fn append_error_samples(samples: &mut Vec<IoErrorSample>, incoming: Vec<IoErrorSample>) {
    for sample in incoming {
        if samples.len() >= 8 {
            break;
        }
        samples.push(sample);
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
    apply_pending_layout_weights(&mut entries);
    entries.sort_by(|a, b| {
        b.layout_size
            .cmp(&a.layout_size)
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}

pub fn entry_view(scan: &ActiveScan, index: TreeIndex) -> Option<EntryView> {
    let node = scan.nodes.get(index)?;
    Some(EntryView {
        index,
        name: node.name.clone(),
        path: node.path.clone(),
        size: node.size,
        layout_size: node.size,
        modified: node.modified,
        is_dir: node.is_dir,
        entry_count: node.entry_count,
        metadata_error: node.metadata_error,
        size_pending: node.is_dir && !node.summarized,
    })
}

fn apply_pending_layout_weights(entries: &mut [EntryView]) {
    let pending_dirs = entries
        .iter()
        .filter(|entry| entry.is_dir && entry.size_pending)
        .count();
    if pending_dirs == 0 {
        return;
    }

    let provisional = provisional_pending_layout_size(entries);
    for entry in entries {
        if entry.is_dir && entry.size_pending {
            entry.layout_size = entry.layout_size.max(provisional);
        }
    }
}

fn provisional_pending_layout_size(entries: &[EntryView]) -> u128 {
    let mut known_sizes = entries
        .iter()
        .filter(|entry| !entry.size_pending && entry.size > 0)
        .map(|entry| entry.size)
        .collect::<Vec<_>>();

    if known_sizes.is_empty() {
        return entries
            .iter()
            .map(|entry| entry.size)
            .max()
            .unwrap_or(1)
            .max(PROVISIONAL_PENDING_LAYOUT_SIZE);
    }

    known_sizes.sort_unstable();
    let median = known_sizes[known_sizes.len() / 2];
    let average = known_sizes.iter().sum::<u128>() / known_sizes.len() as u128;
    let max_known = *known_sizes.last().unwrap_or(&median);

    // Pending directories should stay visible next to already-measured siblings,
    // but not dominate the layout as if they were known to be the largest entry.
    median
        .max(average / 2)
        .max(max_known / 6)
        .max(PROVISIONAL_PENDING_LAYOUT_SIZE)
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

fn scan_directory_level(
    parent: TreeIndex,
    path: PathBuf,
    options: ScanOptions,
    cancel_flag: &AtomicBool,
) -> ScanEvent {
    let root_dev = fs::symlink_metadata(&path).map(|meta| meta.dev()).ok();
    let mut children = Vec::new();
    let mut stats = ScanStats {
        active_jobs: 1,
        ..Default::default()
    };

    let read_dir = match fs::read_dir(&path) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            record_io_error(&mut stats.io_errors, &mut stats.error_samples, &path, err);
            return ScanEvent::Children {
                parent,
                children,
                options,
                stats,
            };
        }
    };

    if cancel_flag.load(Ordering::Relaxed) {
        return ScanEvent::Children {
            parent,
            children,
            options,
            stats,
        };
    }

    for entry in read_dir {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }
        stats.entries_traversed += 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                record_io_error(&mut stats.io_errors, &mut stats.error_samples, &path, err);
                continue;
            }
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
            Err(err) => {
                record_io_error(
                    &mut stats.io_errors,
                    &mut stats.error_samples,
                    &child_path,
                    err,
                );
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
    error_samples: Vec<IoErrorSample>,
    metadata_error: bool,
}

fn summarize_path(
    path: &Path,
    metadata: &fs::Metadata,
    root_dev: Option<u64>,
    options: ScanOptions,
    visited: &mut HashSet<(u64, u64)>,
    cancel_flag: &AtomicBool,
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
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }
        let read_dir = match fs::read_dir(&dir) {
            Ok(read_dir) => read_dir,
            Err(err) => {
                record_io_error(
                    &mut summary.io_errors,
                    &mut summary.error_samples,
                    &dir,
                    err,
                );
                summary.metadata_error = true;
                continue;
            }
        };

        for entry in read_dir {
            if cancel_flag.load(Ordering::Relaxed) {
                break;
            }
            summary.entries_seen += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    record_io_error(
                        &mut summary.io_errors,
                        &mut summary.error_samples,
                        &dir,
                        err,
                    );
                    continue;
                }
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
                Err(err) => {
                    record_io_error(
                        &mut summary.io_errors,
                        &mut summary.error_samples,
                        &entry_path,
                        err,
                    );
                    summary.metadata_error = true;
                }
            }
        }
    }

    summary
}

fn summarize_path_from_fs(
    path: &Path,
    root_dev: Option<u64>,
    options: ScanOptions,
    cancel_flag: &AtomicBool,
) -> Summary {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let mut visited = HashSet::new();
            summarize_path(
                path,
                &metadata,
                root_dev,
                options,
                &mut visited,
                cancel_flag,
            )
        }
        Err(err) => {
            let mut summary = Summary {
                metadata_error: true,
                ..Default::default()
            };
            record_io_error(
                &mut summary.io_errors,
                &mut summary.error_samples,
                path,
                err,
            );
            summary
        }
    }
}

fn record_io_error(
    count: &mut u64,
    samples: &mut Vec<IoErrorSample>,
    path: &Path,
    err: std::io::Error,
) {
    *count += 1;
    if samples.len() < 8 {
        samples.push(IoErrorSample {
            path: path.to_path_buf(),
            message: err.to_string(),
        });
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

    fn entry_for_layout(name: &str, size: u128, is_dir: bool, size_pending: bool) -> EntryView {
        EntryView {
            index: 0,
            name: name.to_string(),
            path: PathBuf::from(name),
            size,
            layout_size: size,
            modified: SystemTime::UNIX_EPOCH,
            is_dir,
            entry_count: None,
            metadata_error: false,
            size_pending,
        }
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
    fn pending_directories_get_provisional_layout_weight() {
        let mut entries = vec![
            entry_for_layout("known-large", 12 * 1024 * 1024 * 1024, true, false),
            entry_for_layout("workspace", 4096, true, true),
            entry_for_layout("tiny-file", 512, false, false),
        ];

        apply_pending_layout_weights(&mut entries);

        let workspace = entries
            .iter()
            .find(|entry| entry.name == "workspace")
            .expect("workspace entry");
        assert_eq!(workspace.size, 4096);
        assert!(workspace.layout_size >= PROVISIONAL_PENDING_LAYOUT_SIZE);
        assert!(workspace.layout_size > workspace.size);
    }

    #[test]
    fn all_pending_directories_get_visible_floor() {
        let mut entries = vec![
            entry_for_layout("workspace", 4096, true, true),
            entry_for_layout("src", 4096, true, true),
            entry_for_layout("small-file", 512, false, false),
        ];

        apply_pending_layout_weights(&mut entries);

        assert!(
            entries
                .iter()
                .filter(|entry| entry.is_dir)
                .all(|entry| entry.layout_size >= PROVISIONAL_PENDING_LAYOUT_SIZE)
        );
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

    #[test]
    fn cancel_stops_accepting_scan_results() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..8 {
            fs::create_dir_all(dir.path().join(format!("dir-{index}/child")))
                .expect("create child dir");
            fs::write(
                dir.path().join(format!("dir-{index}/child/file.txt")),
                b"hello",
            )
            .expect("write file");
        }

        let options = ScanOptions {
            apparent_size: true,
            cross_filesystems: false,
        };
        let mut scan = ActiveScan::start(dir.path().to_path_buf(), options).expect("start scan");
        wait_until(&mut scan, |scan| !children(scan, 0).is_empty());
        scan.prefetch_children(0, options);

        scan.cancel();
        let entries_before = scan.stats.entries_traversed;
        std::thread::sleep(std::time::Duration::from_millis(20));
        scan.drain_events(100);

        assert!(scan.canceled);
        assert!(scan.finished);
        assert_eq!(scan.stats.active_jobs, 0);
        assert_eq!(scan.progress().queued_jobs, 0);
        assert_eq!(scan.progress().background_jobs, 0);
        assert_eq!(scan.stats.entries_traversed, entries_before);
        assert!(scan.nodes.iter().all(|node| !node.scanning));
        assert!(scan.nodes.iter().all(|node| !node.summarizing));
    }

    #[test]
    fn progress_reports_interactive_and_background_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["a", "b", "c"] {
            fs::create_dir(dir.path().join(name)).expect("create child dir");
            fs::write(dir.path().join(name).join("file.txt"), b"hello").expect("write file");
        }

        let options = ScanOptions {
            apparent_size: true,
            cross_filesystems: false,
        };
        let mut scan = ActiveScan::start(dir.path().to_path_buf(), options).expect("start scan");
        wait_until(&mut scan, |scan| !children(scan, 0).is_empty());

        let progress = scan.progress();

        assert!(progress.active_jobs <= MAX_SCAN_JOBS);
        assert!(progress.background_jobs > 0 || progress.active_jobs > 0);
        assert!(!progress.canceled);
    }

    #[test]
    fn io_errors_keep_user_visible_samples() {
        let mut count = 0;
        let mut samples = Vec::new();

        record_io_error(
            &mut count,
            &mut samples,
            Path::new("/tmp/not-readable"),
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
        );

        assert_eq!(count, 1);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].path, Path::new("/tmp/not-readable"));
        assert!(samples[0].message.contains("permission denied"));
    }
}
