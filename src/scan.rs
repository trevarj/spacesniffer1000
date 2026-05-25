use anyhow::{Context, Result};
use crossbeam::channel::{Receiver, Sender};
use jwalk::{Parallelism, WalkDirGeneric};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

pub type TreeIndex = usize;
const MAX_SCAN_JOBS: usize = 2;
const PROVISIONAL_PENDING_LAYOUT_SIZE: u128 = 1024 * 1024;
const CHILD_BATCH_SIZE: usize = 128;
const BACKGROUND_SUMMARIES_PER_LEVEL: usize = 12;
const SUMMARY_PROGRESS_INTERVAL: u64 = 1024;
const SUMMARY_CACHE_FRESH_FOR: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct ActiveScan {
    nodes: Vec<Node>,
    event_rx: Receiver<ScanEvent>,
    event_tx: Sender<ScanEvent>,
    cancel_flag: Arc<AtomicBool>,
    queued: VecDeque<ScanJob>,
    background_queued: VecDeque<ScanJob>,
    summary_cache: SharedSummaryCache,
    pub root: PathBuf,
    pub finished: bool,
    pub canceled: bool,
    pub stats: ScanStats,
}

#[derive(Debug, Clone, Copy, Default, Hash, PartialEq, Eq)]
pub struct ScanOptions {
    pub apparent_size: bool,
    pub cross_filesystems: bool,
}

pub type SharedSummaryCache = Arc<Mutex<SummaryCache>>;

#[derive(Debug, Default)]
pub struct SummaryCache {
    entries: HashMap<SummaryCacheKey, CachedSummary>,
}

impl SummaryCache {
    pub fn invalidate_prefix(&mut self, path: &Path) {
        self.entries.retain(|key, _| !key.path.starts_with(path));
    }

    fn get(
        &self,
        path: &Path,
        options: ScanOptions,
        fingerprint: MetadataFingerprint,
    ) -> Option<CachedSummary> {
        self.entries
            .get(&SummaryCacheKey::new(path, options, fingerprint))
            .cloned()
    }

    fn insert(
        &mut self,
        path: &Path,
        options: ScanOptions,
        fingerprint: MetadataFingerprint,
        summary: &Summary,
    ) {
        self.entries.insert(
            SummaryCacheKey::new(path, options, fingerprint),
            CachedSummary {
                summary: summary.clone(),
                cached_at: SystemTime::now(),
            },
        );
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct SummaryCacheKey {
    path: PathBuf,
    options: ScanOptions,
    fingerprint: MetadataFingerprint,
}

impl SummaryCacheKey {
    fn new(path: &Path, options: ScanOptions, fingerprint: MetadataFingerprint) -> Self {
        Self {
            path: path.to_path_buf(),
            options,
            fingerprint,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct MetadataFingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl MetadataFingerprint {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedSummary {
    summary: Summary,
    cached_at: SystemTime,
}

impl CachedSummary {
    fn is_fresh(&self) -> bool {
        self.cached_at
            .elapsed()
            .is_ok_and(|age| age <= SUMMARY_CACHE_FRESH_FOR)
    }
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
    summary_queued: bool,
    summarizing: bool,
    summary_entries_seen: u64,
}

#[derive(Debug)]
enum ScanEvent {
    ChildrenBatch {
        parent: TreeIndex,
        children: Vec<ScannedEntry>,
        options: ScanOptions,
        stats: ScanStats,
        done: bool,
    },
    Summary {
        index: TreeIndex,
        options: ScanOptions,
        summary: Summary,
    },
    SummaryProgress {
        index: TreeIndex,
        progress: SummaryProgress,
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
    pub estimating: bool,
    pub estimate_entries_seen: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryScanState {
    NotDirectory,
    Pending,
    Scanning,
    Estimating,
    Scanned,
}

impl ActiveScan {
    pub fn start(root: PathBuf, options: ScanOptions) -> Result<Self> {
        Self::start_with_cache(root, options, SharedSummaryCache::default())
    }

    pub fn start_with_cache(
        root: PathBuf,
        options: ScanOptions,
        summary_cache: SharedSummaryCache,
    ) -> Result<Self> {
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
            summary_queued: false,
            summarizing: false,
            summary_entries_seen: 0,
        };

        let mut scan = Self {
            nodes: vec![root_node],
            event_rx,
            event_tx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            queued: VecDeque::new(),
            background_queued: VecDeque::new(),
            summary_cache,
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
            node.summary_queued = false;
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
        self.remove_background_summary(index);
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

    pub fn warm_visible_children(
        &mut self,
        parents: &[TreeIndex],
        options: ScanOptions,
        per_parent_limit: usize,
    ) -> usize {
        if per_parent_limit == 0 {
            return 0;
        }

        let mut scheduled = 0;
        for parent in parents {
            if self.stats.active_jobs + self.queued.len() >= MAX_SCAN_JOBS {
                break;
            }
            let Some(node) = self.nodes.get(*parent) else {
                continue;
            };
            let mut children = node
                .children
                .iter()
                .filter_map(|child| self.nodes.get(*child).map(|node| (*child, node.size)))
                .collect::<Vec<_>>();
            children.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

            let mut scheduled_for_parent = 0;
            for (child, _) in children {
                if scheduled_for_parent >= per_parent_limit {
                    break;
                }
                if self.stats.active_jobs + self.queued.len() >= MAX_SCAN_JOBS {
                    break;
                }
                if self.child_scan_is_known_or_pending(child) {
                    continue;
                }
                let before = self.stats.active_jobs + self.queued.len();
                self.ensure_scanned(child, options);
                let after = self.stats.active_jobs + self.queued.len();
                if after > before {
                    scheduled += 1;
                    scheduled_for_parent += 1;
                }
            }
        }
        scheduled
    }

    fn child_scan_is_known_or_pending(&self, index: TreeIndex) -> bool {
        let Some(node) = self.nodes.get(index) else {
            return true;
        };
        node.scanned
            || node.scanning
            || self.queued.iter().any(|queued| {
                matches!(queued, ScanJob::Children(queued_index, _) if *queued_index == index)
            })
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
            JobPriority::Background => {
                if let ScanJob::Summary(index, _) = job
                    && let Some(node) = self.nodes.get_mut(index)
                {
                    node.summary_queued = true;
                }
                self.background_queued.push_back(job);
            }
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
            ScanJob::Summary(index, options) => {
                if let Some(node) = self.nodes.get_mut(index) {
                    node.summary_queued = false;
                }
                self.summarize_node(index, options);
            }
        }
    }

    fn remove_background_summary(&mut self, index: TreeIndex) {
        let before = self.background_queued.len();
        self.background_queued
            .retain(|job| !matches!(job, ScanJob::Summary(queued, _) if *queued == index));
        if before != self.background_queued.len()
            && let Some(node) = self.nodes.get_mut(index)
        {
            node.summary_queued = false;
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
                    || node.summary_queued
                    || self.queued.iter().any(|queued| {
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
                scan_directory_level(index, path, options, &cancel_flag, &tx);
            })
            .expect("spawn directory scanner");
    }

    fn summarize_node(&mut self, index: TreeIndex, options: ScanOptions) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        if node.summarized || node.summarizing {
            return;
        }

        let path = node.path.clone();
        let boundary_dev = node.boundary_dev;
        if let Some(cached) = self.cached_summary_for_path(&path, options) {
            let fresh = cached.is_fresh();
            self.apply_cached_summary(index, cached.summary, fresh);
            if fresh {
                return;
            }
        }

        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        if node.summarized || node.summarizing {
            return;
        }
        node.summarizing = true;
        node.summary_entries_seen = 0;
        self.finished = false;
        self.stats.active_jobs += 1;
        let tx = self.event_tx.clone();
        let cancel_flag = self.cancel_flag.clone();

        std::thread::Builder::new()
            .name("spacesniffer-summary-scan".into())
            .spawn(move || {
                let summary =
                    summarize_path_from_fs(index, &path, boundary_dev, options, cancel_flag, &tx);
                let _ = tx.send(ScanEvent::Summary {
                    index,
                    options,
                    summary,
                });
            })
            .expect("spawn summary scanner");
    }

    fn cached_summary_for_path(&self, path: &Path, options: ScanOptions) -> Option<CachedSummary> {
        let metadata = fs::symlink_metadata(path).ok()?;
        let fingerprint = MetadataFingerprint::from_metadata(&metadata);
        self.summary_cache
            .lock()
            .ok()?
            .get(path, options, fingerprint)
    }

    fn integrate_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::ChildrenBatch {
                parent,
                children,
                options,
                stats,
                done,
            } => self.integrate_children_batch(parent, children, options, stats, done),
            ScanEvent::Summary {
                index,
                options,
                summary,
            } => self.integrate_summary(index, options, summary),
            ScanEvent::SummaryProgress { index, progress } => {
                self.integrate_summary_progress(index, progress)
            }
        }
        self.start_queued_jobs();
    }

    fn integrate_children_batch(
        &mut self,
        parent: TreeIndex,
        children: Vec<ScannedEntry>,
        options: ScanOptions,
        stats: ScanStats,
        done: bool,
    ) {
        if self.nodes.get(parent).is_none() {
            if done {
                self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
            }
            return;
        }

        let batch_count = children.len() as u64;
        let mut batch_size = 0;
        let mut child_indices = Vec::with_capacity(children.len());
        let mut background_summaries = self.background_summaries_for_parent(parent);

        for child in children {
            let index = self.nodes.len();
            let should_summarize = child.is_dir;
            let cached_summary = should_summarize
                .then(|| self.cached_summary_for_path(&child.path, options))
                .flatten();
            let cached_summary_is_fresh =
                cached_summary.as_ref().is_some_and(CachedSummary::is_fresh);
            let cached_summary = cached_summary.map(|cached| cached.summary);
            let child_size = cached_summary
                .as_ref()
                .map_or(child.size, |summary| summary.size);
            batch_size += child_size;
            child_indices.push(index);
            self.nodes.push(Node {
                name: child.name,
                path: child.path,
                size: child_size,
                modified: child.modified,
                is_dir: child.is_dir,
                entry_count: cached_summary
                    .as_ref()
                    .map_or(child.entry_count, |summary| Some(summary.entry_count)),
                metadata_error: child.metadata_error
                    || cached_summary
                        .as_ref()
                        .is_some_and(|summary| summary.metadata_error),
                parent: Some(parent),
                children: Vec::new(),
                boundary_dev: child.boundary_dev,
                scanned: false,
                scanning: false,
                summarized: !should_summarize || cached_summary_is_fresh,
                summary_queued: false,
                summarizing: false,
                summary_entries_seen: cached_summary
                    .as_ref()
                    .map_or(0, |summary| summary.entries_seen),
            });
            if should_summarize
                && !cached_summary_is_fresh
                && background_summaries < BACKGROUND_SUMMARIES_PER_LEVEL
            {
                self.enqueue_job(ScanJob::Summary(index, options), JobPriority::Background);
                background_summaries += 1;
            }
        }

        let update_parent_size = !self.nodes[parent].summarized;
        let old_size = self.nodes[parent].size;
        {
            let parent_node = &mut self.nodes[parent];
            let first_batch = parent_node.children.is_empty();
            parent_node.children.extend(child_indices.iter().copied());
            parent_node.scanned = done;
            parent_node.scanning = !done;
            if update_parent_size {
                parent_node.size = if first_batch {
                    batch_size
                } else {
                    parent_node.size.saturating_add(batch_size)
                };
                parent_node.entry_count = Some(
                    parent_node
                        .entry_count
                        .unwrap_or(0)
                        .saturating_add(batch_count),
                );
            }
        }
        if update_parent_size {
            let new_size = self.nodes[parent].size;
            self.propagate_size_delta(parent, old_size, new_size);
        }

        self.stats.entries_traversed += stats.entries_traversed;
        self.stats.io_errors += stats.io_errors;
        append_error_samples(&mut self.stats.error_samples, stats.error_samples);
        self.stats.total_bytes = self.nodes.first().map(|node| node.size);
        if done {
            self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
        }
    }

    fn integrate_summary(&mut self, index: TreeIndex, options: ScanOptions, summary: Summary) {
        if self.nodes.get(index).is_none() {
            self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
            return;
        }

        self.store_summary_cache(index, options, &summary);
        let old_size = self.nodes[index].size;
        {
            let node = &mut self.nodes[index];
            node.size = summary.size;
            node.entry_count = node.is_dir.then_some(summary.entry_count);
            node.metadata_error |= summary.metadata_error;
            node.summarized = true;
            node.summarizing = false;
            node.summary_entries_seen = summary.entries_seen;
        }
        self.propagate_size_delta(index, old_size, summary.size);

        self.stats.entries_traversed += summary.entries_seen;
        self.stats.io_errors += summary.io_errors;
        append_error_samples(&mut self.stats.error_samples, summary.error_samples);
        self.stats.total_bytes = self.nodes.first().map(|node| node.size);
        self.stats.active_jobs = self.stats.active_jobs.saturating_sub(1);
    }

    fn apply_cached_summary(&mut self, index: TreeIndex, summary: Summary, complete: bool) {
        if self.nodes.get(index).is_none() {
            return;
        }

        let old_size = self.nodes[index].size;
        {
            let node = &mut self.nodes[index];
            node.size = summary.size;
            node.entry_count = node.is_dir.then_some(summary.entry_count);
            node.metadata_error |= summary.metadata_error;
            node.summary_entries_seen = summary.entries_seen;
            if complete {
                node.summarized = true;
                node.summary_queued = false;
                node.summarizing = false;
            }
        }
        self.propagate_size_delta(index, old_size, summary.size);
        self.stats.total_bytes = self.nodes.first().map(|node| node.size);
    }

    fn store_summary_cache(&mut self, index: TreeIndex, options: ScanOptions, summary: &Summary) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        let Ok(metadata) = fs::symlink_metadata(&node.path) else {
            return;
        };
        let fingerprint = MetadataFingerprint::from_metadata(&metadata);
        if let Ok(mut cache) = self.summary_cache.lock() {
            cache.insert(&node.path, options, fingerprint, summary);
        }
    }

    fn integrate_summary_progress(&mut self, index: TreeIndex, progress: SummaryProgress) {
        if self.nodes.get(index).is_none_or(|node| node.summarized) {
            return;
        }

        let old_size = self.nodes[index].size;
        {
            let node = &mut self.nodes[index];
            node.size = progress.size.max(node.size);
            node.entry_count = node.is_dir.then_some(progress.entry_count);
            node.summary_entries_seen = progress.entries_seen;
        }
        let new_size = self.nodes[index].size;
        self.propagate_size_delta(index, old_size, new_size);
        self.stats.total_bytes = self.nodes.first().map(|node| node.size);
    }

    fn background_summaries_for_parent(&self, parent: TreeIndex) -> usize {
        self.nodes
            .get(parent)
            .into_iter()
            .flat_map(|node| node.children.iter())
            .filter(|child| {
                self.nodes
                    .get(**child)
                    .is_some_and(|node| node.is_dir && (node.summarizing || node.summary_queued))
            })
            .count()
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
        estimating: node.summarizing || node.summary_queued,
        estimate_entries_seen: node.summary_entries_seen,
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
    } else if node.summarizing || node.summary_queued {
        Some(DirectoryScanState::Estimating)
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
    tx: &Sender<ScanEvent>,
) {
    let root_dev = fs::symlink_metadata(&path).map(|meta| meta.dev()).ok();
    let mut children = Vec::with_capacity(CHILD_BATCH_SIZE);
    let mut stats = child_batch_stats();

    let read_dir = match fs::read_dir(&path) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            record_io_error(&mut stats.io_errors, &mut stats.error_samples, &path, err);
            send_child_batch(tx, parent, &mut children, options, stats, true);
            return;
        }
    };

    if cancel_flag.load(Ordering::Relaxed) {
        send_child_batch(tx, parent, &mut children, options, stats, true);
        return;
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
        if children.len() >= CHILD_BATCH_SIZE {
            send_child_batch(tx, parent, &mut children, options, stats, false);
            stats = child_batch_stats();
        }
    }

    send_child_batch(tx, parent, &mut children, options, stats, true);
}

fn child_batch_stats() -> ScanStats {
    ScanStats {
        active_jobs: 1,
        ..Default::default()
    }
}

fn send_child_batch(
    tx: &Sender<ScanEvent>,
    parent: TreeIndex,
    children: &mut Vec<ScannedEntry>,
    options: ScanOptions,
    stats: ScanStats,
    done: bool,
) {
    let event = ScanEvent::ChildrenBatch {
        parent,
        children: std::mem::take(children),
        options,
        stats,
        done,
    };
    let _ = tx.send(event);
}

#[derive(Debug, Clone, Default)]
struct Summary {
    size: u128,
    entry_count: u64,
    entries_seen: u64,
    io_errors: u64,
    error_samples: Vec<IoErrorSample>,
    metadata_error: bool,
}

#[derive(Debug, Clone, Copy)]
struct SummaryProgress {
    size: u128,
    entry_count: u64,
    entries_seen: u64,
}

type WalkClientState = ((), Option<std::result::Result<fs::Metadata, jwalk::Error>>);

fn summarize_path_with_jwalk(
    index: TreeIndex,
    path: &Path,
    metadata: &fs::Metadata,
    root_dev: Option<u64>,
    options: ScanOptions,
    cancel_flag: Arc<AtomicBool>,
    tx: &Sender<ScanEvent>,
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

    let mut visited = HashSet::new();
    if metadata.nlink() > 1 && !visited.insert((metadata.dev(), metadata.ino())) {
        summary.size = 0;
        return summary;
    }

    if !metadata.is_dir() {
        return summary;
    }

    let cancel_for_walk = cancel_flag.clone();
    let walker = WalkDirGeneric::<WalkClientState>::new(path)
        .min_depth(1)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(Parallelism::RayonDefaultPool {
            busy_timeout: Duration::from_secs(1),
        })
        .process_read_dir(move |depth, _, _, entries| {
            if cancel_for_walk.load(Ordering::Relaxed) {
                for entry in entries.iter_mut().filter_map(|entry| entry.as_mut().ok()) {
                    entry.read_children_path = None;
                }
                return;
            }

            // jwalk calls this once for the synthetic root entry. We already have
            // that metadata, and min_depth(1) keeps the root out of the iterator.
            if depth.is_none() {
                return;
            }

            for entry in entries {
                let Ok(entry) = entry else {
                    continue;
                };
                let metadata = entry.metadata();
                if let Ok(metadata) = &metadata
                    && entry.file_type.is_dir()
                    && !options.cross_filesystems
                    && root_dev.is_some_and(|root_dev| metadata.dev() != root_dev)
                {
                    entry.read_children_path = None;
                }
                entry.client_state = Some(metadata);
            }
        });

    let iter = match walker.try_into_iter() {
        Ok(iter) => iter,
        Err(err) => {
            record_error_message(
                &mut summary.io_errors,
                &mut summary.error_samples,
                path,
                err.to_string(),
            );
            summary.metadata_error = true;
            return summary;
        }
    };

    for entry in iter {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }
        summary.entries_seen += 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                record_error_message(
                    &mut summary.io_errors,
                    &mut summary.error_samples,
                    path,
                    err.to_string(),
                );
                summary.metadata_error = true;
                continue;
            }
        };
        let mut entry_path = None;
        if let Some(err) = entry.read_children_error.as_ref() {
            let entry_path = entry_path.get_or_insert_with(|| entry.path());
            record_error_message(
                &mut summary.io_errors,
                &mut summary.error_samples,
                entry_path,
                err.to_string(),
            );
            summary.metadata_error = true;
        }
        let Some(metadata_result) = entry.client_state.as_ref() else {
            continue;
        };
        let metadata = match metadata_result {
            Ok(metadata) => metadata,
            Err(err) => {
                let entry_path = entry_path.get_or_insert_with(|| entry.path());
                record_error_message(
                    &mut summary.io_errors,
                    &mut summary.error_samples,
                    entry_path,
                    err.to_string(),
                );
                summary.metadata_error = true;
                continue;
            }
        };
        if !options.cross_filesystems && root_dev.is_some_and(|root_dev| metadata.dev() != root_dev)
        {
            continue;
        }
        if metadata.nlink() > 1 && !visited.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        summary.size += disk_size(metadata, options.apparent_size);
        summary.entry_count += 1;
        send_summary_progress_if_due(index, &summary, tx);
    }

    summary
}

fn summarize_path_from_fs(
    index: TreeIndex,
    path: &Path,
    root_dev: Option<u64>,
    options: ScanOptions,
    cancel_flag: Arc<AtomicBool>,
    tx: &Sender<ScanEvent>,
) -> Summary {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            summarize_path_with_jwalk(index, path, &metadata, root_dev, options, cancel_flag, tx)
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

fn send_summary_progress_if_due(index: TreeIndex, summary: &Summary, tx: &Sender<ScanEvent>) {
    if !summary
        .entries_seen
        .is_multiple_of(SUMMARY_PROGRESS_INTERVAL)
    {
        return;
    }

    let _ = tx.send(ScanEvent::SummaryProgress {
        index,
        progress: SummaryProgress {
            size: summary.size,
            entry_count: summary.entry_count,
            entries_seen: summary.entries_seen,
        },
    });
}

fn record_io_error(
    count: &mut u64,
    samples: &mut Vec<IoErrorSample>,
    path: &Path,
    err: std::io::Error,
) {
    record_error_message(count, samples, path, err.to_string());
}

fn record_error_message(
    count: &mut u64,
    samples: &mut Vec<IoErrorSample>,
    path: &Path,
    message: String,
) {
    *count += 1;
    if samples.len() < 8 {
        samples.push(IoErrorSample {
            path: path.to_path_buf(),
            message,
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

    fn queued_summary_jobs(scan: &ActiveScan, index: TreeIndex) -> usize {
        scan.queued
            .iter()
            .chain(scan.background_queued.iter())
            .filter(
                |job| matches!(job, ScanJob::Summary(queued_index, _) if *queued_index == index),
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
            estimating: false,
            estimate_entries_seen: 0,
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
    fn fresh_summary_cache_seeds_rescanned_children_immediately() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("nested")).expect("create nested");
        fs::write(dir.path().join("nested/file.txt"), b"hello").expect("write file");
        let options = ScanOptions {
            apparent_size: true,
            cross_filesystems: false,
        };
        let cache = SharedSummaryCache::default();
        let mut first =
            ActiveScan::start_with_cache(dir.path().to_path_buf(), options, cache.clone())
                .expect("start first scan");

        wait_until(&mut first, |scan| scan.finished);

        let mut second = ActiveScan::start_with_cache(dir.path().to_path_buf(), options, cache)
            .expect("start second scan");
        wait_until(&mut second, |scan| !children(scan, 0).is_empty());

        let nested = children(&second, 0)
            .into_iter()
            .find(|entry| entry.name == "nested")
            .expect("nested directory");
        assert!(!nested.size_pending);
        assert!(nested.size >= 5);
        assert!(nested.entry_count.is_some_and(|count| count >= 2));
    }

    #[test]
    fn directory_level_scan_streams_large_directories_in_batches() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..(CHILD_BATCH_SIZE + 5) {
            fs::write(dir.path().join(format!("file-{index:03}.txt")), b"x").expect("write file");
        }
        let (tx, rx) = crossbeam::channel::unbounded();

        scan_directory_level(
            0,
            dir.path().to_path_buf(),
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
            &AtomicBool::new(false),
            &tx,
        );

        let first = rx.try_recv().expect("first child batch");
        match first {
            ScanEvent::ChildrenBatch { children, done, .. } => {
                assert_eq!(children.len(), CHILD_BATCH_SIZE);
                assert!(!done);
            }
            ScanEvent::Summary { .. } => panic!("expected child batch"),
            ScanEvent::SummaryProgress { .. } => panic!("expected child batch"),
        }

        let final_batch = rx.try_recv().expect("final child batch");
        match final_batch {
            ScanEvent::ChildrenBatch { children, done, .. } => {
                assert_eq!(children.len(), 5);
                assert!(done);
            }
            ScanEvent::Summary { .. } => panic!("expected child batch"),
            ScanEvent::SummaryProgress { .. } => panic!("expected child batch"),
        }
    }

    #[test]
    fn summary_scan_reports_progress_before_completion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).expect("create nested");
        for index in 0..(SUMMARY_PROGRESS_INTERVAL as usize + 8) {
            fs::write(nested.join(format!("file-{index:03}.txt")), b"x").expect("write file");
        }
        let (tx, rx) = crossbeam::channel::unbounded();
        let summary = summarize_path_from_fs(
            7,
            &nested,
            None,
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
            Arc::new(AtomicBool::new(false)),
            &tx,
        );

        assert!(summary.entries_seen > SUMMARY_PROGRESS_INTERVAL);
        let progress = rx
            .try_iter()
            .find_map(|event| match event {
                ScanEvent::SummaryProgress { index, progress } => Some((index, progress)),
                _ => None,
            })
            .expect("summary progress event");
        assert_eq!(progress.0, 7);
        assert!(progress.1.entries_seen >= SUMMARY_PROGRESS_INTERVAL);
        assert!(progress.1.size > 0);
    }

    #[test]
    fn summary_scan_counts_hardlinked_files_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("original.bin");
        let linked = dir.path().join("linked.bin");
        fs::write(&original, b"hardlinked bytes").expect("write file");
        if fs::hard_link(&original, &linked).is_err() {
            return;
        }
        let (tx, _rx) = crossbeam::channel::unbounded();

        let summary = summarize_path_from_fs(
            0,
            dir.path(),
            None,
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
            Arc::new(AtomicBool::new(false)),
            &tx,
        );

        assert_eq!(summary.entry_count, 2);
    }

    #[test]
    fn background_summary_queue_is_bounded_per_level() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..(BACKGROUND_SUMMARIES_PER_LEVEL + 8) {
            fs::create_dir(dir.path().join(format!("dir-{index:02}"))).expect("create child dir");
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

        assert!(scan.background_summaries_for_parent(0) <= BACKGROUND_SUMMARIES_PER_LEVEL);
    }

    #[test]
    fn queued_summary_is_reported_as_estimating() {
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
        let queued = children(&scan, 0)
            .into_iter()
            .find(|entry| queued_summary_jobs(&scan, entry.index) > 0)
            .expect("queued summary");

        assert_eq!(
            directory_scan_state(&scan, queued.index),
            Some(DirectoryScanState::Estimating)
        );
        assert!(entry_view(&scan, queued.index).is_some_and(|entry| entry.estimating));
    }

    #[test]
    fn interactive_scan_removes_redundant_background_summary() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["a", "b", "c", "d"] {
            fs::create_dir(dir.path().join(name)).expect("create nested");
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
        let nested = children(&scan, 0)
            .into_iter()
            .find(|entry| queued_summary_jobs(&scan, entry.index) > 0)
            .expect("directory with queued summary");
        assert!(queued_summary_jobs(&scan, nested.index) > 0);

        scan.ensure_scanned(
            nested.index,
            ScanOptions {
                apparent_size: true,
                cross_filesystems: false,
            },
        );

        assert_eq!(queued_summary_jobs(&scan, nested.index), 0);
        assert!(scan.nodes[nested.index].scanning || queued_child_jobs(&scan, nested.index) > 0);
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
    fn warm_visible_children_prefetches_largest_visible_dirs_within_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["small", "medium", "large"] {
            fs::create_dir(dir.path().join(name)).expect("create child dir");
        }

        let options = ScanOptions {
            apparent_size: true,
            cross_filesystems: false,
        };
        let mut scan = ActiveScan::start(dir.path().to_path_buf(), options).expect("start scan");
        wait_until(&mut scan, |scan| scan.finished);

        let entries = children(&scan, 0);
        let small = entries
            .iter()
            .find(|entry| entry.name == "small")
            .expect("small")
            .index;
        let medium = entries
            .iter()
            .find(|entry| entry.name == "medium")
            .expect("medium")
            .index;
        let large = entries
            .iter()
            .find(|entry| entry.name == "large")
            .expect("large")
            .index;
        scan.nodes[small].size = 100;
        scan.nodes[medium].size = 200;
        scan.nodes[large].size = 300;

        let scheduled = scan.warm_visible_children(&[0], options, 2);

        assert_eq!(scheduled, 2);
        assert!(scan.nodes[large].scanning || queued_child_jobs(&scan, large) > 0);
        assert!(scan.nodes[medium].scanning || queued_child_jobs(&scan, medium) > 0);
        assert!(!scan.nodes[small].scanning);
        assert_eq!(queued_child_jobs(&scan, small), 0);
        assert!(scan.stats.active_jobs + scan.queued.len() <= MAX_SCAN_JOBS);
    }

    #[test]
    fn warm_visible_children_advances_past_already_known_large_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["small", "medium", "large"] {
            fs::create_dir(dir.path().join(name)).expect("create child dir");
        }

        let options = ScanOptions {
            apparent_size: true,
            cross_filesystems: false,
        };
        let mut scan = ActiveScan::start(dir.path().to_path_buf(), options).expect("start scan");
        wait_until(&mut scan, |scan| scan.finished);

        let entries = children(&scan, 0);
        let small = entries
            .iter()
            .find(|entry| entry.name == "small")
            .expect("small")
            .index;
        let medium = entries
            .iter()
            .find(|entry| entry.name == "medium")
            .expect("medium")
            .index;
        let large = entries
            .iter()
            .find(|entry| entry.name == "large")
            .expect("large")
            .index;
        scan.nodes[small].size = 100;
        scan.nodes[medium].size = 200;
        scan.nodes[large].size = 300;
        scan.nodes[large].scanned = true;

        let scheduled = scan.warm_visible_children(&[0], options, 2);

        assert_eq!(scheduled, 2);
        assert!(scan.nodes[medium].scanning || queued_child_jobs(&scan, medium) > 0);
        assert!(scan.nodes[small].scanning || queued_child_jobs(&scan, small) > 0);
        assert_eq!(queued_child_jobs(&scan, large), 0);
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
