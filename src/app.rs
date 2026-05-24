use crate::{
    deletion::{DeleteMode, delete_path, ensure_deletable},
    filters::{FilterEntry, Filters, parse_size_input_bytes},
    mounts::{Mount, discover_mounts},
    scan::{self, ActiveScan, EntryView, ScanOptions, TreeIndex},
    treemap::{self, TreemapItem},
};
use eframe::egui::{
    self, Align2, Color32, FontFamily, FontId, RichText, Sense, Shape, Stroke, StrokeKind, Vec2,
};
use humansize::{BINARY, format_size};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const STORAGE_THEME_KEY: &str = "spacesniffer1000.theme";
const STORAGE_PATH_KEY: &str = "spacesniffer1000.path";
const STORAGE_APPARENT_SIZE_KEY: &str = "spacesniffer1000.apparent_size";
const STORAGE_CROSS_FILESYSTEMS_KEY: &str = "spacesniffer1000.cross_filesystems";
const INSPECTOR_CHILD_LIMIT: usize = 80;

pub struct App {
    path_input: String,
    mounts: Vec<Mount>,
    scan: Option<ActiveScan>,
    focus: Option<TreeIndex>,
    selected: Option<TreeIndex>,
    expanded: HashSet<TreeIndex>,
    expansion_stack: Vec<TreeIndex>,
    expansion_started: HashMap<TreeIndex, f64>,
    click_pulses: Vec<ClickPulse>,
    filters: Filters,
    min_size_input: String,
    apparent_size: bool,
    cross_filesystems: bool,
    active_scan_options: ScanOptions,
    status: String,
    pending_delete: Option<DeleteRequest>,
    permanent_confirmation: String,
    zoom_flash: f64,
    show_filters: bool,
    show_scan_options: bool,
    show_theme_picker: bool,
    ui_theme: UiTheme,
}

#[derive(Debug, Clone)]
struct DeleteRequest {
    path: PathBuf,
    mode: DeleteMode,
}

#[derive(Debug, Clone)]
struct ClickPulse {
    index: TreeIndex,
    position: egui::Pos2,
    started_at: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiTheme {
    Graphite,
    Frost,
    Retrowave,
}

#[derive(Debug, Clone, Copy)]
enum IconKind {
    Up,
    Folder,
    Refresh,
    Settings,
    Sliders,
    Palette,
    Drive,
    File,
}

impl UiTheme {
    const ALL: [Self; 3] = [Self::Graphite, Self::Frost, Self::Retrowave];

    fn label(self) -> &'static str {
        match self {
            Self::Graphite => "Graphite",
            Self::Frost => "Frost",
            Self::Retrowave => "Retrowave",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Graphite => "Dark neutral, low glare",
            Self::Frost => "Bright desktop clarity",
            Self::Retrowave => "Neon arcade contrast",
        }
    }

    fn storage_value(self) -> &'static str {
        match self {
            Self::Graphite => "graphite",
            Self::Frost => "frost",
            Self::Retrowave => "retrowave",
        }
    }

    fn from_storage(value: &str) -> Self {
        match value {
            "frost" => Self::Frost,
            "retrowave" => Self::Retrowave,
            _ => Self::Graphite,
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ui_theme = cc
            .storage
            .and_then(|storage| storage.get_string(STORAGE_THEME_KEY))
            .as_deref()
            .map(UiTheme::from_storage)
            .unwrap_or(UiTheme::Graphite);
        configure_style(&cc.egui_ctx, ui_theme);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let path_input = cc
            .storage
            .and_then(|storage| storage.get_string(STORAGE_PATH_KEY))
            .map(PathBuf::from)
            .filter(|path| path.exists())
            .unwrap_or(cwd)
            .display()
            .to_string();
        let apparent_size = stored_bool(cc.storage, STORAGE_APPARENT_SIZE_KEY).unwrap_or(false);
        let cross_filesystems =
            stored_bool(cc.storage, STORAGE_CROSS_FILESYSTEMS_KEY).unwrap_or(false);
        let mut app = Self {
            path_input,
            mounts: discover_mounts(),
            scan: None,
            focus: None,
            selected: None,
            expanded: HashSet::new(),
            expansion_stack: Vec::new(),
            expansion_started: HashMap::new(),
            click_pulses: Vec::new(),
            filters: Filters::default(),
            min_size_input: String::from("0"),
            apparent_size,
            cross_filesystems,
            active_scan_options: ScanOptions::default(),
            status: "Choose a path and scan.".into(),
            pending_delete: None,
            permanent_confirmation: String::new(),
            zoom_flash: 0.0,
            show_filters: false,
            show_scan_options: false,
            show_theme_picker: false,
            ui_theme,
        };
        app.start_scan();
        app
    }

    fn start_scan(&mut self) {
        let path = PathBuf::from(self.path_input.trim());
        let options = self.scan_options();
        match ActiveScan::start(path.clone(), options) {
            Ok(scan) => {
                self.focus = Some(0);
                self.selected = Some(0);
                self.expanded.clear();
                self.expansion_stack.clear();
                self.expansion_started.clear();
                self.click_pulses.clear();
                self.active_scan_options = options;
                self.scan = Some(scan);
                self.zoom_flash = 1.0;
                self.status = format!("Scanning {}...", path.display());
            }
            Err(err) => {
                self.status = format!("Scan failed: {err}");
            }
        }
    }

    fn scan_options(&self) -> ScanOptions {
        ScanOptions {
            apparent_size: self.apparent_size,
            cross_filesystems: self.cross_filesystems,
        }
    }

    fn scan_options_dirty(&self) -> bool {
        self.scan.is_some() && self.scan_options() != self.active_scan_options
    }

    fn path_dirty(&self) -> bool {
        let Some(scan) = self.scan.as_ref() else {
            return false;
        };
        Path::new(self.path_input.trim()) != scan.root
    }

    fn selected_entry(&self) -> Option<EntryView> {
        let scan = self.scan.as_ref()?;
        scan::entry_view(scan, self.selected?)
    }

    fn focused_entry(&self) -> Option<EntryView> {
        let scan = self.scan.as_ref()?;
        scan::entry_view(scan, self.focus?)
    }

    fn focused_directory_state(&self) -> Option<scan::DirectoryScanState> {
        let scan = self.scan.as_ref()?;
        scan::directory_scan_state(scan, self.focus?)
    }

    fn selected_parent_share(&self, entry: &EntryView) -> Option<f64> {
        let scan = self.scan.as_ref()?;
        let Some(parent) = scan::parent(scan, entry.index) else {
            return Some(1.0);
        };
        let parent = scan::entry_view(scan, parent)?;
        if parent.size == 0 {
            return None;
        }
        Some((entry.size as f64 / parent.size as f64).clamp(0.0, 1.0))
    }

    fn focused_entries(&self) -> Vec<EntryView> {
        let Some(scan) = self.scan.as_ref() else {
            return Vec::new();
        };
        let Some(focus) = self.focus else {
            return Vec::new();
        };
        let now = SystemTime::now();
        scan::children(scan, focus)
            .into_iter()
            .filter(|entry| {
                self.filters.matches(
                    &FilterEntry {
                        name: &entry.name,
                        size: entry.size,
                        modified: entry.modified,
                    },
                    now,
                )
            })
            .collect()
    }

    fn normalize_selection_to_visible_entries(&mut self) {
        let Some(focus) = self.focus else {
            self.selected = None;
            return;
        };

        let entries = self.focused_entries();
        if self.selected == Some(focus)
            || self
                .selected
                .is_some_and(|selected| entries.iter().any(|entry| entry.index == selected))
        {
            return;
        }

        self.selected = entries.first().map(|entry| entry.index).or(Some(focus));
    }

    fn filters_active(&self) -> bool {
        !self.filters.name_substring.trim().is_empty()
            || self.filters.min_size_bytes > 0
            || self.filters.max_age_days > 0
    }

    fn clear_filters(&mut self) {
        self.filters = Filters::default();
        self.min_size_input = String::from("0");
        self.normalize_selection_to_visible_entries();
    }

    fn can_go_up(&self) -> bool {
        if !self.expansion_stack.is_empty() {
            return true;
        }
        let Some(scan) = self.scan.as_ref() else {
            return false;
        };
        self.focus
            .and_then(|focus| scan::parent(scan, focus))
            .is_some()
    }

    fn filtered_children(&self, parent: TreeIndex) -> Vec<EntryView> {
        let Some(scan) = self.scan.as_ref() else {
            return Vec::new();
        };
        let now = SystemTime::now();
        scan::children(scan, parent)
            .into_iter()
            .filter(|entry| {
                self.filters.matches(
                    &FilterEntry {
                        name: &entry.name,
                        size: entry.size,
                        modified: entry.modified,
                    },
                    now,
                )
            })
            .collect()
    }

    fn go_up(&mut self) {
        if let Some(last) = self.expansion_stack.pop() {
            self.expanded.remove(&last);
            self.expansion_started.remove(&last);
            self.selected = Some(last);
            self.zoom_flash = 1.0;
            return;
        }

        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        let Some(focus) = self.focus else {
            return;
        };
        if let Some(parent) = scan::parent(scan, focus) {
            self.focus = Some(parent);
            self.selected = Some(parent);
            self.zoom_flash = 1.0;
        }
    }

    fn expand_directory_tile(&mut self, index: TreeIndex, started_at: f64) {
        let options = self.scan_options();
        if let Some(scan) = self.scan.as_mut() {
            scan.ensure_scanned(index, options);
            scan.prefetch_children(index, options);
        }

        if self.expanded.insert(index) {
            self.expansion_stack.push(index);
            self.expansion_started.insert(index, started_at);
        }
        self.selected = Some(index);
        self.zoom_flash = 1.0;
    }

    fn focus_directory(&mut self, index: TreeIndex) {
        let options = self.scan_options();
        if let Some(scan) = self.scan.as_mut() {
            scan.ensure_scanned(index, options);
            scan.prefetch_children(index, options);
        }
        self.focus = Some(index);
        self.selected = Some(index);
        self.expanded.clear();
        self.expansion_stack.clear();
        self.expansion_started.clear();
        self.click_pulses.clear();
        self.zoom_flash = 1.0;
    }

    fn select_sibling(&mut self, direction: i32) {
        let entries = self.focused_entries();
        if entries.is_empty() {
            return;
        }

        let current = self
            .selected
            .and_then(|selected| entries.iter().position(|entry| entry.index == selected))
            .unwrap_or(0);
        let next = if direction < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(entries.len() - 1)
        };
        self.selected = Some(entries[next].index);
    }

    fn select_boundary_sibling(&mut self, first: bool) {
        let entries = self.focused_entries();
        let Some(entry) = (if first {
            entries.first()
        } else {
            entries.last()
        }) else {
            return;
        };
        self.selected = Some(entry.index);
    }

    fn activate_selected_directory(&mut self, focus_view: bool, started_at: f64) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if !entry.is_dir {
            return;
        }

        if focus_view {
            self.focus_directory(entry.index);
        } else {
            self.expand_directory_tile(entry.index, started_at);
        }
    }

    fn rescan_parent_after_delete(&mut self) {
        if let Some(scan) = self.scan.as_ref()
            && let Some(focus) = self.focus.and_then(|idx| scan::entry_view(scan, idx))
        {
            self.path_input = focus.path.display().to_string();
        }
        self.start_scan();
    }

    fn update_scan(&mut self, ctx: &egui::Context) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        let changed = scan.drain_events(250);
        if scan.finished {
            self.status = format!(
                "{} entries, {}, {} I/O errors",
                scan.stats.entries_traversed,
                scan.stats
                    .total_bytes
                    .map(human_bytes)
                    .unwrap_or_else(|| "0 B".into()),
                scan.stats.io_errors
            );
        } else {
            self.status = format!("Scanning {} entries...", scan.stats.entries_traversed);
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        if changed {
            self.normalize_selection_to_visible_entries();
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn handle_navigation_buttons(&mut self, ctx: &egui::Context) {
        let (
            back_clicked,
            f5_pressed,
            escape_pressed,
            up_pressed,
            down_pressed,
            home_pressed,
            end_pressed,
            enter_pressed,
            backspace_pressed,
            ctrl_pressed,
            time,
        ) = ctx.input(|input| {
            (
                input.pointer.button_clicked(egui::PointerButton::Extra1),
                input.key_pressed(egui::Key::F5),
                input.key_pressed(egui::Key::Escape),
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::Home),
                input.key_pressed(egui::Key::End),
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Backspace),
                input.modifiers.ctrl,
                input.time,
            )
        });
        if back_clicked && self.can_go_up() {
            self.go_up();
        }
        if f5_pressed {
            self.start_scan();
        }
        if escape_pressed {
            self.show_filters = false;
            self.show_scan_options = false;
            self.show_theme_picker = false;
            self.pending_delete = None;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if up_pressed {
            self.select_sibling(-1);
        }
        if down_pressed {
            self.select_sibling(1);
        }
        if home_pressed {
            self.select_boundary_sibling(true);
        }
        if end_pressed {
            self.select_boundary_sibling(false);
        }
        if enter_pressed {
            self.activate_selected_directory(ctrl_pressed, time);
        }
        if backspace_pressed && self.can_go_up() {
            self.go_up();
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let can_go_up = self.can_go_up();
            if toolbar_up_button(ui, can_go_up).clicked() && can_go_up {
                self.go_up();
            }

            let icon_count = 5.0;
            let icon_width = 44.0;
            let icon_spacing = ui.spacing().item_spacing.x;
            let controls_width = icon_count * icon_width + icon_count * icon_spacing;
            let path_width = (ui.available_width() - controls_width).max(160.0);
            let path_response = ui.add_sized(
                [path_width, 40.0],
                egui::TextEdit::singleline(&mut self.path_input)
                    .font(egui::TextStyle::Monospace)
                    .horizontal_align(egui::Align::LEFT)
                    .vertical_align(egui::Align::Center)
                    .margin(egui::Margin::symmetric(10, 8))
                    .hint_text("Path"),
            );
            let focus_path = ui.input(|input| {
                input.key_pressed(egui::Key::L) && (input.modifiers.command || input.modifiers.ctrl)
            });
            if focus_path {
                path_response.request_focus();
            }
            let path_response = path_response.on_hover_text("Scan root path (Ctrl+L)");
            if path_response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                self.start_scan();
            }

            if icon_button(ui, IconKind::Folder, "Choose folder").clicked()
                && let Some(path) = rfd::FileDialog::new().pick_folder()
            {
                self.path_input = path.display().to_string();
                self.start_scan();
            }
            let path_dirty = self.path_dirty();
            let refresh_response = icon_button(ui, IconKind::Refresh, "Scan / rescan (F5)");
            if path_dirty {
                paint_toolbar_badge(ui.painter(), refresh_response.rect, self.ui_theme);
            }
            if refresh_response.clicked() {
                self.start_scan();
            }
            let scan_options_response =
                icon_button(ui, IconKind::Sliders, "Scan options (Esc closes)");
            if self.scan_options_dirty() {
                paint_toolbar_badge(ui.painter(), scan_options_response.rect, self.ui_theme);
            }
            if scan_options_response.clicked() {
                self.show_scan_options = true;
            }
            let filter_response = icon_button(ui, IconKind::Settings, "Filters (Esc closes)");
            if self.filters_active() {
                paint_toolbar_badge(ui.painter(), filter_response.rect, self.ui_theme);
            }
            if filter_response.clicked() {
                self.show_filters = true;
            }
            if icon_button(ui, IconKind::Palette, "Theme (Esc closes)").clicked() {
                self.show_theme_picker = true;
            }
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let scanning = self.scan.as_ref().is_some_and(|scan| !scan.finished);
            let dirty_options = self.scan_options_dirty();
            let path_dirty = self.path_dirty();
            let status_text = if path_dirty {
                "Path changed; scan to update."
            } else {
                &self.status
            };
            let pending_scan = dirty_options || path_dirty;
            status_pill(ui, status_text, scanning, pending_scan, self.ui_theme);
            if pending_scan
                && rescan_pill(
                    ui,
                    if path_dirty {
                        "scan path"
                    } else {
                        "rescan to apply"
                    },
                    self.ui_theme,
                )
                .clicked()
            {
                self.start_scan();
            }
            scan_mode_chip(ui, self.active_scan_options, self.ui_theme);
            self.filter_chips(ui);
        });
    }

    fn filter_chips(&mut self, ui: &mut egui::Ui) {
        if !self.filters_active() {
            return;
        }

        ui.add_space(8.0);
        let name = self.filters.name_substring.trim().to_string();
        if !name.is_empty() {
            filter_chip(ui, &format!("name: {name}"), self.ui_theme);
        }
        if self.filters.min_size_bytes > 0 {
            filter_chip(
                ui,
                &format!("minimum {}", human_bytes(self.filters.min_size_bytes)),
                self.ui_theme,
            );
        }
        if self.filters.max_age_days > 0 {
            filter_chip(
                ui,
                &format!("modified {} days", self.filters.max_age_days),
                self.ui_theme,
            );
        }
        if text_button(ui, "clear", egui::vec2(54.0, 26.0), true)
            .on_hover_text("Clear active filters")
            .clicked()
        {
            self.clear_filters();
        }
    }

    fn filter_window(&mut self, ctx: &egui::Context) {
        if !self.show_filters {
            return;
        }

        let title = if self.filters_active() {
            "Filters - active"
        } else {
            "Filters"
        };
        let mut clear_filters = false;
        let mut normalize_selection = false;
        egui::Window::new(title)
            .open(&mut self.show_filters)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Filter Rules").strong());
                ui.add_space(4.0);

                ui.label(RichText::new("Name contains").small().weak());
                if ui
                    .add_sized(
                        [ui.available_width(), 38.0],
                        egui::TextEdit::singleline(&mut self.filters.name_substring)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("cache, target, log"),
                    )
                    .changed()
                {
                    normalize_selection = true;
                }
                ui.add_space(8.0);
                ui.label(RichText::new("Minimum size").small().weak());
                if ui
                    .add_sized(
                        [ui.available_width(), 38.0],
                        egui::TextEdit::singleline(&mut self.min_size_input)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("512M, 2G, or bytes"),
                    )
                    .changed()
                {
                    let bytes = parse_size_input_bytes(&self.min_size_input).unwrap_or(0);
                    if self.filters.min_size_bytes != bytes {
                        self.filters.min_size_bytes = bytes;
                    }
                    normalize_selection = true;
                }
                let parsed_size = parse_size_input_bytes(&self.min_size_input);
                if self.min_size_input.trim().is_empty() || parsed_size == Some(0) {
                    ui.label(RichText::new("No size floor").small().weak());
                } else if let Some(bytes) = parsed_size {
                    ui.label(
                        RichText::new(format!("Parsed as {}", human_bytes(bytes)))
                            .small()
                            .weak(),
                    );
                } else {
                    ui.colored_label(
                        Color32::from_rgb(230, 92, 104),
                        RichText::new(
                            "Size filter paused; use a number with optional K, M, G, or T suffix",
                        )
                        .small(),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Modified within").small().weak());
                    if ui
                        .add(egui::DragValue::new(&mut self.filters.max_age_days).range(0..=3650))
                        .changed()
                    {
                        normalize_selection = true;
                    }
                    ui.label(RichText::new("days").small().weak());
                });
                let age_summary = if self.filters.max_age_days == 0 {
                    "Any modified date".to_string()
                } else {
                    format!("Modified within {} days", self.filters.max_age_days)
                };
                ui.label(RichText::new(age_summary).small().weak());
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    filter_chip(ui, &active_filter_summary(&self.filters), self.ui_theme);
                    let filters_active = !self.filters.name_substring.trim().is_empty()
                        || self.filters.min_size_bytes > 0
                        || self.filters.max_age_days > 0;
                    if filters_active
                        && text_button(ui, "Clear", egui::vec2(70.0, 32.0), false).clicked()
                    {
                        clear_filters = true;
                    }
                });
            });
        if clear_filters {
            self.clear_filters();
        }
        if normalize_selection {
            self.normalize_selection_to_visible_entries();
        }
    }

    fn scan_options_window(&mut self, ctx: &egui::Context) {
        if !self.show_scan_options {
            return;
        }

        let mut rescan = false;
        let mut open = self.show_scan_options;
        egui::Window::new("Scan Options")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Size Accounting").strong());
                ui.add_space(4.0);

                let apparent_pending = self.scan.is_some()
                    && self.apparent_size != self.active_scan_options.apparent_size;
                option_toggle(
                    ui,
                    &mut self.apparent_size,
                    "Apparent Size",
                    "Use apparent byte size on the next scan.",
                    apparent_pending,
                    self.ui_theme,
                );
                ui.label(
                    RichText::new("Allocated size is closer to real disk usage.")
                        .small()
                        .weak(),
                );

                ui.add_space(12.0);
                ui.label(RichText::new("Traversal").strong());
                ui.add_space(4.0);

                let cross_fs_pending = self.scan.is_some()
                    && self.cross_filesystems != self.active_scan_options.cross_filesystems;
                option_toggle(
                    ui,
                    &mut self.cross_filesystems,
                    "Cross Filesystems",
                    "Cross filesystem boundaries on the next scan.",
                    cross_fs_pending,
                    self.ui_theme,
                );
                ui.label(
                    RichText::new("Same filesystem avoids scanning mounted volumes by surprise.")
                        .small()
                        .weak(),
                );

                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    filter_chip(ui, &scan_mode_text(self.scan_options()), self.ui_theme);
                    if self.scan_options_dirty()
                        && text_button(ui, "Rescan Now", egui::vec2(104.0, 32.0), false).clicked()
                    {
                        rescan = true;
                    }
                });
            });
        self.show_scan_options = open;

        if rescan {
            self.start_scan();
        }
    }

    fn theme_window(&mut self, ctx: &egui::Context) {
        if !self.show_theme_picker {
            return;
        }

        let mut open = self.show_theme_picker;
        let mut close_after_selection = false;
        egui::Window::new("Theme")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Palette").strong());
                ui.add_space(4.0);
                for theme in UiTheme::ALL {
                    let selected = self.ui_theme == theme;
                    if theme_row_button(ui, theme, selected).clicked() {
                        self.ui_theme = theme;
                        configure_style(ctx, self.ui_theme);
                        close_after_selection = true;
                    }
                }
            });
        self.show_theme_picker = open && !close_after_selection;
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Mounts", "volumes");
        egui::ScrollArea::vertical()
            .id_salt("mount-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for mount in self.mounts.clone() {
                    let selected = self.path_input == mount.target.display().to_string();
                    let tooltip = format!(
                        "{}\n{}\n{}",
                        mount.target.display(),
                        mount.source,
                        mount.fs_type
                    );
                    let response = mount_row_button(ui, &mount, selected, self.ui_theme)
                        .on_hover_text(tooltip);
                    if response.clicked() {
                        self.path_input = mount.target.display().to_string();
                        self.start_scan();
                    }
                }
            });
    }

    fn breadcrumbs(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        let Some(mut current) = self.focus else {
            return;
        };
        let mut chain = vec![current];
        while let Some(parent) = scan::parent(scan, current) {
            chain.push(parent);
            current = parent;
        }
        chain.reverse();

        let mut items = Vec::with_capacity(chain.len());
        for node in chain.iter().copied() {
            if let Some(entry) = scan::entry_view(scan, node) {
                let label = if node == 0 {
                    entry.path.display().to_string()
                } else {
                    entry.name.clone()
                };
                items.push((node, label, entry.path.display().to_string()));
            }
        }
        let focus = self.focus;
        let hidden_middle = items.len().saturating_sub(5);

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Path").small().weak());
            for (position, (node, label, path)) in items.iter().enumerate() {
                if hidden_middle > 0 && position == 1 {
                    breadcrumb_overflow(ui, hidden_middle);
                }
                if hidden_middle > 0 && position > 0 && position < hidden_middle + 1 {
                    continue;
                }

                let selected = focus == Some(*node);
                let response = breadcrumb_button(ui, label, selected, self.ui_theme)
                    .on_hover_text(path.as_str());
                if response.clicked() {
                    self.focus_directory(*node);
                }
            }
        });
    }

    fn treemap_header(&mut self, ui: &mut egui::Ui, entries: &[EntryView]) {
        ui.vertical(|ui| {
            self.breadcrumbs(ui);
            ui.horizontal(|ui| {
                focus_summary(
                    ui,
                    self.focused_entry(),
                    entries.len(),
                    self.filters_active(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    size_legend(ui, self.ui_theme);
                    ui.add_space(10.0);
                    type_legend(ui);
                });
            });
        });
    }

    fn treemap(&mut self, ui: &mut egui::Ui, entries: &[EntryView]) {
        let scan_snapshot = self
            .scan
            .as_ref()
            .map(|scan| (scan.finished, scan.stats.clone()));
        let focused_state = self.focused_directory_state();
        let available = ui.available_size();
        let size = Vec2::new(available.x.max(280.0), available.y.max(300.0));
        let (response, painter) = ui.allocate_painter(size, Sense::hover());
        let rect = response.rect.shrink(6.0);
        let time = ui.input(|input| input.time);
        paint_tree_background(&painter, response.rect, time, self.ui_theme);

        if entries.is_empty() {
            let loading_focus = matches!(
                focused_state,
                Some(scan::DirectoryScanState::Pending | scan::DirectoryScanState::Scanning)
            );
            let empty_text = if loading_focus {
                "Loading this folder..."
            } else if self.filters_active() {
                "No matching children"
            } else {
                "No visible children"
            };
            paint_treemap_empty_state(
                &painter,
                rect,
                empty_text,
                loading_focus,
                time,
                self.ui_theme,
            );
            return;
        }

        let max_size = entries.first().map(|entry| entry.size).unwrap_or(1).max(1);
        self.render_entry_tiles(ui, &painter, entries, rect, max_size, 0);

        if let Some((false, stats)) = scan_snapshot {
            paint_scan_activity_badge(&painter, rect, &stats, self.ui_theme);
        }

        if self.zoom_flash > 0.01 {
            let alpha = (self.zoom_flash * 90.0) as u8;
            painter.rect_stroke(
                rect.shrink(4.0 * self.zoom_flash as f32),
                0.0,
                Stroke::new(
                    5.0 * self.zoom_flash as f32,
                    Color32::from_rgba_unmultiplied(80, 240, 255, alpha),
                ),
                StrokeKind::Inside,
            );
        }
    }

    fn render_entry_tiles(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        entries: &[EntryView],
        rect: egui::Rect,
        max_size: u128,
        depth: usize,
    ) {
        if entries.is_empty() || rect.width() < 3.0 || rect.height() < 3.0 || depth > 32 {
            return;
        }
        let time = ui.input(|input| input.time);

        let tiles = treemap::layout(
            &entries
                .iter()
                .map(|entry| TreemapItem {
                    item: entry.index,
                    size: entry.size,
                })
                .collect::<Vec<_>>(),
            rect,
        );
        let selected = self.selected;
        let entries_by_index = entries
            .iter()
            .map(|entry| (entry.index, entry))
            .collect::<HashMap<_, _>>();

        for tile in tiles {
            let Some(entry) = entries_by_index.get(&tile.item).copied() else {
                continue;
            };
            let tile_rect = tile.rect.shrink(2.0);
            if tile_rect.width() < 2.0 || tile_rect.height() < 2.0 {
                continue;
            }

            let id = ui.id().with(("tile", entry.index, depth));
            let tile_response = ui
                .interact(tile_rect, id, Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let hover_t = ui
                .ctx()
                .animate_bool(id.with("hover"), tile_response.hovered());
            let selected_t = ui
                .ctx()
                .animate_bool(id.with("selected"), selected == Some(entry.index));
            let color = size_color(entry.size, max_size, self.ui_theme);
            let glow = (hover_t + selected_t).clamp(0.0, 1.0);
            let is_expanded = self.expanded.contains(&entry.index);
            let expanded_children = if is_expanded {
                self.filtered_children(entry.index)
            } else {
                Vec::new()
            };

            let stroke = if selected == Some(entry.index) {
                Stroke::new(3.0, Color32::WHITE)
            } else if tile_response.hovered() {
                Stroke::new(2.0, themed_effect_color(self.ui_theme, 205))
            } else {
                Stroke::new(1.0, Color32::from_black_alpha(110))
            };

            if is_expanded && !expanded_children.is_empty() {
                let child_rect = expanded_child_rect(tile_rect, depth);
                paint_expanded_directory_frame(painter, tile_rect, entry, self.ui_theme);
                painter.rect_stroke(tile_rect, 2.0, stroke, StrokeKind::Inside);
                if glow > 0.0 {
                    painter.rect_stroke(
                        tile_rect.expand(2.0),
                        4.0,
                        Stroke::new(2.0 * glow, themed_effect_color(self.ui_theme, 150)),
                        StrokeKind::Outside,
                    );
                }
                if selected == Some(entry.index) {
                    paint_selected_breathing(painter, tile_rect, time, self.ui_theme);
                }
                paint_click_pulses(
                    painter,
                    tile_rect,
                    entry.index,
                    &self.click_pulses,
                    time,
                    self.ui_theme,
                );
                self.render_entry_tiles(
                    ui,
                    painter,
                    &expanded_children,
                    child_rect,
                    entry.size.max(1),
                    depth + 1,
                );
                if let Some(started_at) = self.expansion_started.get(&entry.index) {
                    paint_expand_reveal_cover(
                        painter,
                        child_rect,
                        ((time - started_at) / 0.32).clamp(0.0, 1.0) as f32,
                        self.ui_theme,
                    );
                }
                continue;
            }

            paint_tile_body(painter, tile_rect, color, entry.is_dir, self.ui_theme);
            painter.rect_stroke(tile_rect, 2.0, stroke, StrokeKind::Inside);
            if glow > 0.0 {
                painter.rect_stroke(
                    tile_rect.expand(2.0),
                    4.0,
                    Stroke::new(2.0 * glow, themed_effect_color(self.ui_theme, 150)),
                    StrokeKind::Outside,
                );
            }
            if selected == Some(entry.index) {
                paint_selected_breathing(painter, tile_rect, time, self.ui_theme);
            }
            paint_click_pulses(
                painter,
                tile_rect,
                entry.index,
                &self.click_pulses,
                time,
                self.ui_theme,
            );

            paint_tile_label(painter, tile_rect, entry, color);

            tile_response.clone().on_hover_ui(|ui| {
                ui.label(RichText::new(&entry.name).strong());
                ui.label(entry.path.display().to_string());
                ui.label(human_bytes(entry.size));
            });

            if tile_response.clicked() {
                self.selected = Some(entry.index);
                let click_pos = tile_response
                    .hover_pos()
                    .unwrap_or_else(|| tile_rect.center());
                self.click_pulses.push(ClickPulse {
                    index: entry.index,
                    position: click_pos,
                    started_at: time,
                });
                if entry.is_dir {
                    self.expand_directory_tile(entry.index, time);
                }
            }

            if tile_response.hovered()
                && entry.is_dir
                && let Some(scan) = self.scan.as_mut()
            {
                scan.prefetch_children(
                    entry.index,
                    ScanOptions {
                        apparent_size: self.apparent_size,
                        cross_filesystems: self.cross_filesystems,
                    },
                );
            }
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Inspector", "selection");
        let focused_children = self.focused_entries();

        let Some(entry) = self.selected_entry() else {
            inspector_note(ui, "No selection", false, self.ui_theme);
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            self.focused_child_list(ui, &focused_children);
            return;
        };

        selected_summary_card(
            ui,
            &entry,
            self.selected_parent_share(&entry),
            self.ui_theme,
        );
        info_row(ui, "Path", &entry.path.display().to_string());
        info_row(ui, "Size", &human_bytes(entry.size));
        if let Some(share) = self.selected_parent_share(&entry) {
            info_row(ui, "Parent", &percentage_text(share));
        }
        info_row(ui, "Entries", &entry.entry_count.unwrap_or(0).to_string());
        info_row(ui, "Kind", if entry.is_dir { "Folder" } else { "File" });
        if entry.metadata_error {
            ui.colored_label(Color32::YELLOW, "Metadata error");
        }

        if entry.is_dir {
            let time = ui.input(|input| input.time);
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(RichText::new("Navigation").strong());
            if action_button(
                ui,
                "Subdivide",
                "Show children inside the current tile",
                false,
            )
            .on_hover_text("Enter")
            .clicked()
            {
                self.expand_directory_tile(entry.index, time);
            }
            if action_button(
                ui,
                "Focus View",
                "Use this folder as the treemap root",
                false,
            )
            .on_hover_text("Ctrl+Enter")
            .clicked()
            {
                self.focus_directory(entry.index);
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(RichText::new("Delete").strong());
        match ensure_deletable(&entry.path) {
            Ok(()) => {
                if action_button(
                    ui,
                    "Move to Trash",
                    "Recoverable through the desktop trash",
                    false,
                )
                .clicked()
                {
                    self.pending_delete = Some(DeleteRequest {
                        path: entry.path.clone(),
                        mode: DeleteMode::Trash,
                    });
                    self.permanent_confirmation.clear();
                }
                if action_button(
                    ui,
                    "Permanent Delete",
                    "Requires typing the exact path",
                    true,
                )
                .clicked()
                {
                    self.pending_delete = Some(DeleteRequest {
                        path: entry.path,
                        mode: DeleteMode::Permanent,
                    });
                    self.permanent_confirmation.clear();
                }
            }
            Err(err) => {
                ui.label(
                    RichText::new(format!("Delete disabled: {err}"))
                        .small()
                        .weak(),
                );
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        self.focused_child_list(ui, &focused_children);
    }

    fn focused_child_list(&mut self, ui: &mut egui::Ui, children: &[EntryView]) {
        ui.label(RichText::new("Children").strong());
        if children.is_empty() {
            if matches!(
                self.focused_directory_state(),
                Some(scan::DirectoryScanState::Pending | scan::DirectoryScanState::Scanning)
            ) {
                inspector_note(ui, "Loading children", true, self.ui_theme);
            } else {
                inspector_note(ui, "No visible children", false, self.ui_theme);
            }
            return;
        }
        let total_size = children.iter().map(|child| child.size).sum::<u128>().max(1);

        egui::ScrollArea::vertical()
            .id_salt("focused-child-list")
            .max_height(260.0)
            .show(ui, |ui| {
                let time = ui.input(|input| input.time);
                for child in children.iter().take(INSPECTOR_CHILD_LIMIT) {
                    let share = child.size as f64 / total_size as f64;
                    let tooltip = if child.is_dir {
                        format!("{}\nDouble-click to subdivide", child.path.display())
                    } else {
                        child.path.display().to_string()
                    };
                    let response = child_row_button(
                        ui,
                        child,
                        share,
                        self.selected == Some(child.index),
                        self.ui_theme,
                    )
                    .on_hover_text(tooltip);
                    if response.clicked() {
                        self.selected = Some(child.index);
                    }
                    if response.double_clicked() && child.is_dir {
                        self.expand_directory_tile(child.index, time);
                    }
                }
            });
        if children.len() > INSPECTOR_CHILD_LIMIT {
            ui.add_space(4.0);
            child_limit_footer(ui, children.len(), INSPECTOR_CHILD_LIMIT, self.ui_theme);
        }
    }

    fn delete_modal(&mut self, ctx: &egui::Context) {
        let Some(request) = self.pending_delete.clone() else {
            return;
        };
        let title = match request.mode {
            DeleteMode::Trash => "Move to Trash",
            DeleteMode::Permanent => "Permanent Delete",
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                let destructive = request.mode == DeleteMode::Permanent;
                let summary = if destructive {
                    "Permanently delete this path?"
                } else {
                    "Move this path to the trash?"
                };
                ui.label(RichText::new(summary).strong().size(18.0));
                ui.add_space(6.0);
                framed_path(ui, &request.path.display().to_string());
                if request.mode == DeleteMode::Permanent {
                    ui.add_space(10.0);
                    warning_box(ui, "This cannot be undone. Type the exact path to confirm.");
                    ui.add_space(6.0);
                    ui.label(RichText::new("Confirmation path").small().weak());
                    ui.add_sized(
                        [ui.available_width(), 38.0],
                        egui::TextEdit::singleline(&mut self.permanent_confirmation)
                            .font(egui::TextStyle::Monospace)
                            .vertical_align(egui::Align::Center),
                    );
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if big_button(ui, "Cancel").clicked() {
                        self.pending_delete = None;
                    }
                    let confirmed = request.mode == DeleteMode::Trash
                        || self.permanent_confirmation == request.path.display().to_string();
                    let label = if destructive {
                        "Delete Forever"
                    } else {
                        "Move to Trash"
                    };
                    if confirm_button(ui, label, destructive, confirmed).clicked() {
                        match delete_path(&request.path, request.mode) {
                            Ok(()) => {
                                self.status = format!("Deleted {}", request.path.display());
                                self.pending_delete = None;
                                self.rescan_parent_after_delete();
                            }
                            Err(err) => {
                                self.status = format!("Delete failed: {err}");
                            }
                        }
                    }
                });
            });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_navigation_buttons(ctx);
        self.update_scan(ctx);
        let time = ctx.input(|input| input.time);
        self.click_pulses
            .retain(|pulse| time - pulse.started_at < 0.7);
        self.zoom_flash *= 0.86;
        if self.zoom_flash <= 0.01 {
            self.zoom_flash = 0.0;
        }

        let scanning = self.scan.as_ref().is_some_and(|scan| !scan.finished);
        let revealing = self
            .expansion_started
            .values()
            .any(|started_at| time - started_at < 0.32);
        let animating =
            scanning || revealing || !self.click_pulses.is_empty() || self.zoom_flash > 0.0;
        if animating {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::Panel::top("top").show_inside(ui, |ui| {
            self.top_bar(ui);
        });

        egui::Panel::left("mounts")
            .resizable(true)
            .default_size(220.0)
            .show_inside(ui, |ui| self.left_panel(ui));

        egui::Panel::right("inspector")
            .resizable(true)
            .default_size(280.0)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("inspector-panel")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.inspector(ui));
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            let focused_entries = self.focused_entries();
            self.treemap_header(ui, &focused_entries);
            ui.separator();
            self.treemap(ui, &focused_entries);
        });

        self.delete_modal(&ctx);
        self.filter_window(&ctx);
        self.scan_options_window(&ctx);
        self.theme_window(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(STORAGE_THEME_KEY, self.ui_theme.storage_value().to_string());
        storage.set_string(STORAGE_PATH_KEY, self.path_input.clone());
        storage.set_string(STORAGE_APPARENT_SIZE_KEY, self.apparent_size.to_string());
        storage.set_string(
            STORAGE_CROSS_FILESYSTEMS_KEY,
            self.cross_filesystems.to_string(),
        );
    }
}

fn stored_bool(storage: Option<&dyn eframe::Storage>, key: &str) -> Option<bool> {
    storage
        .and_then(|storage| storage.get_string(key))
        .and_then(|value| value.parse().ok())
}

fn human_bytes(bytes: u128) -> String {
    format_size(bytes.min(u64::MAX as u128) as u64, BINARY)
}

fn active_filter_summary(filters: &Filters) -> String {
    let mut count = 0;
    if !filters.name_substring.trim().is_empty() {
        count += 1;
    }
    if filters.min_size_bytes > 0 {
        count += 1;
    }
    if filters.max_age_days > 0 {
        count += 1;
    }

    if count == 0 {
        String::from("no active filters")
    } else if count == 1 {
        String::from("1 active filter")
    } else {
        format!("{count} active filters")
    }
}

fn configure_style(ctx: &egui::Context, theme: UiTheme) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size = egui::vec2(46.0, 40.0);
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(16.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(16.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Heading,
        FontId::new(22.0, FontFamily::Proportional),
    );
    style.visuals = match theme {
        UiTheme::Frost => egui::Visuals::light(),
        UiTheme::Graphite | UiTheme::Retrowave => egui::Visuals::dark(),
    };

    match theme {
        UiTheme::Graphite => {
            style.visuals.window_fill = Color32::from_rgb(24, 25, 27);
            style.visuals.panel_fill = Color32::from_rgb(29, 30, 33);
            style.visuals.extreme_bg_color = Color32::from_rgb(18, 19, 21);
            style.visuals.faint_bg_color = Color32::from_rgb(39, 40, 44);
            style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(46, 48, 52);
            style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(62, 65, 70);
            style.visuals.widgets.active.bg_fill = Color32::from_rgb(82, 86, 93);
            style.visuals.selection.bg_fill = Color32::from_rgb(88, 96, 108);
            style.visuals.hyperlink_color = Color32::from_rgb(170, 184, 205);
        }
        UiTheme::Frost => {
            style.visuals.window_fill = Color32::from_rgb(245, 247, 250);
            style.visuals.panel_fill = Color32::from_rgb(236, 240, 245);
            style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(226, 232, 240);
            style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(210, 220, 232);
            style.visuals.widgets.active.bg_fill = Color32::from_rgb(188, 202, 220);
        }
        UiTheme::Retrowave => {
            style.visuals.window_fill = Color32::from_rgb(9, 6, 24);
            style.visuals.panel_fill = Color32::from_rgb(17, 9, 38);
            style.visuals.extreme_bg_color = Color32::from_rgb(5, 4, 15);
            style.visuals.faint_bg_color = Color32::from_rgb(31, 17, 54);
            style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(34, 34, 58);
            style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(74, 38, 103);
            style.visuals.widgets.active.bg_fill = Color32::from_rgb(122, 35, 139);
            style.visuals.selection.bg_fill = Color32::from_rgb(174, 49, 168);
            style.visuals.hyperlink_color = Color32::from_rgb(74, 225, 255);
        }
    }
    ctx.set_global_style(style);
}

fn section_title(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(20.0).strong());
        ui.label(RichText::new(subtitle).small().weak());
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
}

fn status_pill(ui: &mut egui::Ui, text: &str, active: bool, dirty_options: bool, theme: UiTheme) {
    let accent = if dirty_options {
        match theme {
            UiTheme::Graphite => Color32::from_rgb(215, 176, 92),
            UiTheme::Frost => Color32::from_rgb(38, 104, 158),
            UiTheme::Retrowave => Color32::from_rgb(255, 218, 92),
        }
    } else if active {
        Color32::from_rgb(74, 225, 255)
    } else {
        ui.visuals().weak_text_color()
    };
    let fill = match theme {
        UiTheme::Graphite => Color32::from_rgb(34, 35, 39),
        UiTheme::Frost => Color32::from_rgb(226, 234, 242),
        UiTheme::Retrowave => Color32::from_rgb(31, 17, 54),
    };
    let stroke = if dirty_options || active {
        Stroke::new(1.0, color_with_alpha(accent, 150))
    } else {
        Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color)
    };
    let width = ui.available_width().clamp(170.0, 360.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 28.0), Sense::hover());
    ui.painter().rect_filled(rect, 14.0, fill);
    ui.painter()
        .rect_stroke(rect, 14.0, stroke, StrokeKind::Inside);

    let indicator_center = rect.left_center() + egui::vec2(14.0, 0.0);
    if active {
        let pulse = ui.input(|input| ((input.time * 3.0).sin() * 0.5 + 0.5) as f32);
        ui.painter().circle_filled(
            indicator_center,
            4.0 + pulse * 1.5,
            color_with_alpha(accent, 220),
        );
    } else if dirty_options {
        ui.painter().circle_filled(indicator_center, 4.0, accent);
    } else {
        ui.painter()
            .circle_filled(indicator_center, 3.5, color_with_alpha(accent, 120));
    }

    let text_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(25.0, 0.0),
        rect.right_bottom() - egui::vec2(10.0, 0.0),
    );
    ui.painter().with_clip_rect(text_rect).text(
        text_rect.left_center(),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(12.0),
        accent,
    );
    let _ = response.on_hover_text(text);
}

fn rescan_pill(ui: &mut egui::Ui, label: &str, theme: UiTheme) -> egui::Response {
    let text_color = match theme {
        UiTheme::Frost => Color32::from_rgb(26, 75, 120),
        UiTheme::Graphite | UiTheme::Retrowave => Color32::from_rgb(255, 235, 160),
    };
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), FontId::proportional(12.0), text_color);
    let width = (galley.size().x + 22.0).max(112.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 26.0), Sense::click());
    let visuals = ui.style().interact(&response);
    ui.painter().rect_filled(rect, 13.0, visuals.bg_fill);
    ui.painter()
        .rect_stroke(rect, 13.0, visuals.bg_stroke, StrokeKind::Inside);
    let clip = rect.shrink2(egui::vec2(10.0, 0.0));
    ui.painter().with_clip_rect(clip).galley(
        rect.center() - galley.size() * 0.5,
        galley,
        text_color,
    );
    response
        .on_hover_text("Run a new scan with the current options.")
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn scan_mode_chip(ui: &mut egui::Ui, options: ScanOptions, theme: UiTheme) {
    filter_chip(ui, &scan_mode_text(options), theme);
}

fn scan_mode_text(options: ScanOptions) -> String {
    let size_mode = if options.apparent_size {
        "Apparent size"
    } else {
        "Allocated size"
    };
    let fs_mode = if options.cross_filesystems {
        "All filesystems"
    } else {
        "Same filesystem"
    };
    format!("{size_mode} / {fs_mode}")
}

fn focus_summary(
    ui: &mut egui::Ui,
    entry: Option<EntryView>,
    visible_children: usize,
    filtered: bool,
) {
    let Some(entry) = entry else {
        ui.label(RichText::new("No scan").small().weak());
        return;
    };
    let filter_note = if filtered { " filtered" } else { "" };
    let text = format!(
        "{}  |  {} visible{}  |  {}",
        human_bytes(entry.size),
        visible_children,
        filter_note,
        if entry.is_dir { "folder" } else { "file" }
    );
    ui.label(RichText::new(text).small().weak());
}

fn paint_toolbar_badge(painter: &egui::Painter, rect: egui::Rect, theme: UiTheme) {
    let color = match theme {
        UiTheme::Graphite => Color32::from_rgb(205, 166, 86),
        UiTheme::Frost => Color32::from_rgb(45, 126, 185),
        UiTheme::Retrowave => Color32::from_rgb(255, 214, 86),
    };
    painter.circle_filled(egui::pos2(rect.right() - 8.0, rect.top() + 8.0), 4.0, color);
}

fn paint_pending_badge(painter: &egui::Painter, rect: egui::Rect, theme: UiTheme) {
    let color = match theme {
        UiTheme::Graphite => Color32::from_rgb(215, 176, 92),
        UiTheme::Frost => Color32::from_rgb(38, 104, 158),
        UiTheme::Retrowave => Color32::from_rgb(255, 218, 92),
    };
    painter.circle_filled(egui::pos2(rect.right() - 9.0, rect.top() + 8.0), 3.5, color);
}

fn filter_chip(ui: &mut egui::Ui, text: &str, theme: UiTheme) {
    let fill = match theme {
        UiTheme::Graphite => Color32::from_rgb(48, 47, 42),
        UiTheme::Frost => Color32::from_rgb(218, 233, 246),
        UiTheme::Retrowave => Color32::from_rgb(49, 28, 70),
    };
    let stroke = Stroke::new(
        1.0,
        match theme {
            UiTheme::Graphite => Color32::from_rgb(120, 106, 72),
            UiTheme::Frost => Color32::from_rgb(86, 138, 184),
            UiTheme::Retrowave => Color32::from_rgb(164, 72, 172),
        },
    );
    let text_color = match theme {
        UiTheme::Frost => Color32::from_rgb(35, 68, 96),
        UiTheme::Graphite | UiTheme::Retrowave => Color32::from_gray(230),
    };
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_string(), FontId::proportional(12.0), text_color);
    let size = galley.size() + egui::vec2(16.0, 8.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, 10.0, fill);
    ui.painter()
        .rect_stroke(rect, 10.0, stroke, StrokeKind::Inside);
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, text_color);
}

fn option_toggle(
    ui: &mut egui::Ui,
    value: &mut bool,
    label: &str,
    tooltip: &str,
    pending: bool,
    theme: UiTheme,
) -> egui::Response {
    let width = ((label.chars().count() as f32 * 7.0) + 56.0).clamp(112.0, 176.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 28.0), Sense::click());
    if response.clicked() {
        *value = !*value;
    }

    let visuals = ui.style().interact_selectable(&response, *value);
    let fill = if *value {
        themed_effect_color(theme, 58)
    } else {
        visuals.bg_fill
    };
    let stroke = if *value {
        Stroke::new(1.0, themed_effect_color(theme, 170))
    } else {
        visuals.bg_stroke
    };
    ui.painter().rect_filled(rect, 14.0, fill);
    ui.painter()
        .rect_stroke(rect, 14.0, stroke, StrokeKind::Inside);

    let knob_center = if *value {
        rect.right_center() - egui::vec2(14.0, 0.0)
    } else {
        rect.left_center() + egui::vec2(14.0, 0.0)
    };
    ui.painter().circle_filled(
        knob_center,
        7.0,
        if *value {
            themed_effect_color(theme, 225)
        } else {
            ui.visuals().weak_text_color()
        },
    );

    let text_pos = rect.left_center() + egui::vec2(30.0, 0.0);
    let text_clip = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(29.0, 0.0),
        rect.right_bottom() - egui::vec2(25.0, 0.0),
    );
    ui.painter().with_clip_rect(text_clip).text(
        text_pos,
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        visuals.text_color(),
    );

    if pending {
        paint_pending_badge(ui.painter(), rect, theme);
    }

    let tooltip = if pending {
        format!("{tooltip}\nPending until rescan")
    } else {
        tooltip.to_string()
    };
    response
        .on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn breadcrumb_overflow(ui: &mut egui::Ui, hidden_count: usize) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(38.0, 30.0), Sense::hover());
    let visuals = ui.visuals();
    ui.painter()
        .rect_filled(rect, 4.0, visuals.extreme_bg_color);
    ui.painter().rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, visuals.widgets.inactive.bg_stroke.color),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        "...",
        FontId::monospace(13.0),
        visuals.weak_text_color(),
    );
    response.on_hover_text(format!("{hidden_count} hidden ancestor folders"));
}

fn breadcrumb_button(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    theme: UiTheme,
) -> egui::Response {
    let available = ui.available_width().max(72.0);
    let width = ((label.chars().count() as f32 * 7.4) + 24.0).clamp(58.0, available.min(190.0));
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 30.0), Sense::click());
    let visuals = ui.style().interact_selectable(&response, selected);
    let fill = if selected {
        themed_effect_color(theme, 56)
    } else {
        visuals.bg_fill
    };
    let stroke = if selected {
        Stroke::new(1.0, themed_effect_color(theme, 190))
    } else {
        visuals.bg_stroke
    };

    ui.painter().rect_filled(rect, 4.0, fill);
    ui.painter()
        .rect_stroke(rect, 4.0, stroke, StrokeKind::Inside);
    let clip_rect = rect.shrink2(egui::vec2(9.0, 0.0));
    ui.painter().with_clip_rect(clip_rect).text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        FontId::monospace(13.0),
        visuals.text_color(),
    );

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn mount_row_button(
    ui: &mut egui::Ui,
    mount: &Mount,
    selected: bool,
    theme: UiTheme,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), Sense::click());
    let visuals = ui.style().interact_selectable(&response, selected);
    ui.painter().rect_filled(rect, 5.0, visuals.bg_fill);
    ui.painter()
        .rect_stroke(rect, 5.0, visuals.bg_stroke, StrokeKind::Inside);

    if selected {
        let rail = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + 4.0, rect.bottom()),
        );
        ui.painter()
            .rect_filled(rail, 3.0, themed_effect_color(theme, 210));
    }

    let icon_rect =
        egui::Rect::from_min_size(rect.left_top() + egui::vec2(11.0, 13.0), Vec2::splat(23.0));
    paint_symbolic_icon(
        ui.painter(),
        icon_rect,
        IconKind::Drive,
        if selected {
            themed_effect_color(theme, 235)
        } else {
            visuals.text_color()
        },
    );

    let target = mount.target.display().to_string();
    let badge_width = ((mount.fs_type.chars().count() as f32 * 6.5) + 18.0).clamp(42.0, 78.0);
    let badge_rect = egui::Rect::from_min_max(
        egui::pos2(rect.right() - badge_width - 9.0, rect.top() + 8.0),
        egui::pos2(rect.right() - 9.0, rect.top() + 27.0),
    );
    paint_mount_fs_badge(ui, badge_rect, &mount.fs_type, selected, theme);

    let text_right = (badge_rect.left() - 8.0).max(rect.left() + 54.0);
    let name_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(44.0, 7.0),
        egui::pos2(text_right, rect.top() + 28.0),
    );
    ui.painter().with_clip_rect(name_rect).text(
        name_rect.left_top(),
        Align2::LEFT_TOP,
        &target,
        FontId::monospace(13.0),
        visuals.text_color(),
    );

    let source = if mount.source.is_empty() {
        String::from("unknown source")
    } else {
        mount.source.clone()
    };
    let detail_rect = egui::Rect::from_min_max(
        rect.left_bottom() + egui::vec2(44.0, -19.0),
        egui::pos2(rect.right() - 10.0, rect.bottom() - 4.0),
    );
    ui.painter().with_clip_rect(detail_rect).text(
        detail_rect.left_top(),
        Align2::LEFT_TOP,
        &source,
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn paint_mount_fs_badge(
    ui: &egui::Ui,
    rect: egui::Rect,
    fs_type: &str,
    selected: bool,
    theme: UiTheme,
) {
    let fill = if selected {
        themed_effect_color(theme, 58)
    } else {
        ui.visuals().extreme_bg_color
    };
    let stroke = Stroke::new(
        1.0,
        themed_effect_color(theme, if selected { 145 } else { 72 }),
    );
    ui.painter().rect_filled(rect, 9.0, fill);
    ui.painter()
        .rect_stroke(rect, 9.0, stroke, StrokeKind::Inside);
    let clip = rect.shrink2(egui::vec2(7.0, 0.0));
    ui.painter().with_clip_rect(clip).text(
        rect.center(),
        Align2::CENTER_CENTER,
        fs_type,
        FontId::monospace(10.5),
        ui.visuals().text_color(),
    );
}

fn child_row_button(
    ui: &mut egui::Ui,
    entry: &EntryView,
    share: f64,
    selected: bool,
    theme: UiTheme,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), Sense::click());
    let visuals = ui.style().interact_selectable(&response, selected);
    ui.painter().rect_filled(rect, 5.0, visuals.bg_fill);
    ui.painter()
        .rect_stroke(rect, 5.0, visuals.bg_stroke, StrokeKind::Inside);

    let bar_width = (rect.width() * share as f32).clamp(2.0, rect.width());
    let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(bar_width, rect.height()));
    ui.painter().rect_filled(
        bar,
        5.0,
        themed_effect_color(theme, if selected { 72 } else { 38 }),
    );

    let icon_rect =
        egui::Rect::from_min_size(rect.left_top() + egui::vec2(9.0, 10.0), Vec2::splat(22.0));
    paint_symbolic_icon(
        ui.painter(),
        icon_rect,
        if entry.is_dir {
            IconKind::Folder
        } else {
            IconKind::File
        },
        visuals.text_color(),
    );

    let name_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(40.0, 7.0),
        egui::pos2(rect.right() - 78.0, rect.top() + 27.0),
    );
    ui.painter().with_clip_rect(name_rect).text(
        name_rect.left_top(),
        Align2::LEFT_TOP,
        &entry.name,
        FontId::monospace(13.0),
        visuals.text_color(),
    );

    let detail = format!(
        "{}  |  {}",
        human_bytes(entry.size),
        if entry.is_dir { "folder" } else { "file" }
    );
    let detail_rect = egui::Rect::from_min_max(
        rect.left_bottom() + egui::vec2(40.0, -18.0),
        egui::pos2(rect.right() - 78.0, rect.bottom() - 3.0),
    );
    ui.painter().with_clip_rect(detail_rect).text(
        detail_rect.left_top(),
        Align2::LEFT_TOP,
        &detail,
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );

    ui.painter().text(
        rect.right_center() - egui::vec2(9.0, 0.0),
        Align2::RIGHT_CENTER,
        percentage_text(share),
        FontId::monospace(12.0),
        visuals.text_color(),
    );

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn child_limit_footer(ui: &mut egui::Ui, total: usize, shown: usize, theme: UiTheme) {
    let hidden = total.saturating_sub(shown);
    let text = format!("showing first {shown} of {total} visible");
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), Sense::hover());
    let fill = match theme {
        UiTheme::Graphite => Color32::from_rgb(38, 39, 43),
        UiTheme::Frost => Color32::from_rgb(226, 234, 242),
        UiTheme::Retrowave => Color32::from_rgb(36, 20, 62),
    };
    ui.painter().rect_filled(rect, 5.0, fill);
    ui.painter().rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0, themed_effect_color(theme, 75)),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    response.on_hover_text(format!(
        "{hidden} additional entries are hidden in this inspector list"
    ));
}

fn action_button(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    destructive: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), Sense::click());
    let visuals = ui.style().interact(&response);
    let bg = if destructive {
        Color32::from_rgba_unmultiplied(115, 35, 42, 90)
    } else {
        visuals.bg_fill
    };
    let stroke = if destructive {
        Stroke::new(1.0, Color32::from_rgb(190, 75, 85))
    } else {
        visuals.bg_stroke
    };
    let title_color = if destructive {
        Color32::from_rgb(255, 205, 210)
    } else {
        visuals.text_color()
    };

    ui.painter().rect_filled(rect, 5.0, bg);
    ui.painter()
        .rect_stroke(rect, 5.0, stroke, StrokeKind::Inside);
    ui.painter().text(
        rect.left_top() + egui::vec2(10.0, 6.0),
        Align2::LEFT_TOP,
        title,
        FontId::proportional(15.0),
        title_color,
    );
    ui.painter().text(
        rect.left_bottom() + egui::vec2(10.0, -6.0),
        Align2::LEFT_BOTTOM,
        detail,
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn framed_path(ui: &mut egui::Ui, path: &str) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 42.0), Sense::hover());
    let visuals = ui.visuals();
    ui.painter()
        .rect_filled(rect, 5.0, visuals.extreme_bg_color);
    ui.painter().rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0, visuals.widgets.inactive.bg_stroke.color),
        StrokeKind::Inside,
    );

    let clip_rect = rect.shrink2(egui::vec2(9.0, 0.0));
    ui.painter().with_clip_rect(clip_rect).text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        Align2::LEFT_CENTER,
        path,
        FontId::monospace(13.0),
        visuals.text_color(),
    );
    let _ = response.on_hover_text(path);
}

fn warning_box(ui: &mut egui::Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, 5.0, Color32::from_rgba_unmultiplied(115, 35, 42, 70));
    ui.painter().rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0, Color32::from_rgb(190, 75, 85)),
        StrokeKind::Inside,
    );

    let clip_rect = rect.shrink2(egui::vec2(10.0, 0.0));
    ui.painter().with_clip_rect(clip_rect).text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(13.5),
        Color32::from_rgb(255, 205, 210),
    );
}

fn confirm_button(
    ui: &mut egui::Ui,
    label: &str,
    destructive: bool,
    enabled: bool,
) -> egui::Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(138.0, 36.0), sense);
    let visuals = if enabled {
        ui.style().interact(&response)
    } else {
        &ui.visuals().widgets.noninteractive
    };
    let fill = if destructive && enabled {
        Color32::from_rgb(145, 42, 52)
    } else {
        visuals.bg_fill
    };
    let stroke = if destructive && enabled {
        Stroke::new(1.0, Color32::from_rgb(230, 92, 104))
    } else {
        visuals.bg_stroke
    };
    let text_color = if destructive && enabled {
        Color32::from_rgb(255, 238, 240)
    } else {
        visuals.text_color()
    };

    ui.painter().rect_filled(rect, 5.0, fill);
    ui.painter()
        .rect_stroke(rect, 5.0, stroke, StrokeKind::Inside);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(14.0),
        text_color,
    );

    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

fn fit_text_to_width(text: &str, max_width: f32, approximate_char_width: f32) -> String {
    let max_chars = (max_width / approximate_char_width).floor().max(0.0) as usize;
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let keep = max_chars.saturating_sub(3);
    let prefix = text.chars().take(keep).collect::<String>();
    format!("{prefix}...")
}

fn theme_row_button(ui: &mut egui::Ui, theme: UiTheme, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 58.0), Sense::click());
    let visuals = ui.style().interact_selectable(&response, selected);
    let fill = if selected {
        themed_effect_color(theme, 48)
    } else {
        visuals.bg_fill
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    ui.painter().rect_stroke(
        rect,
        6.0,
        if selected {
            Stroke::new(1.0, themed_effect_color(theme, 180))
        } else {
            visuals.bg_stroke
        },
        StrokeKind::Inside,
    );

    if selected {
        let rail = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.left() + 4.0, rect.bottom()),
        );
        ui.painter()
            .rect_filled(rail, 3.0, themed_effect_color(theme, 230));
    }

    let preview = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(12.0, 12.0),
        egui::vec2(70.0, 34.0),
    );
    let bg = match theme {
        UiTheme::Graphite => Color32::from_rgb(24, 25, 27),
        UiTheme::Frost => Color32::from_rgb(245, 247, 250),
        UiTheme::Retrowave => Color32::from_rgb(9, 6, 24),
    };
    ui.painter().rect_filled(preview, 4.0, bg);
    for i in 0..4 {
        let color = size_color((i + 1) as u128, 4, theme);
        let swatch = egui::Rect::from_min_size(
            preview.left_top() + egui::vec2(7.0 + i as f32 * 14.0, 11.0),
            egui::vec2(11.0, 12.0),
        );
        ui.painter().rect_filled(swatch, 2.0, color);
    }
    ui.painter().text(
        rect.left_top() + egui::vec2(94.0, 9.0),
        Align2::LEFT_TOP,
        theme.label(),
        FontId::proportional(15.0),
        visuals.text_color(),
    );
    ui.painter().text(
        rect.left_bottom() + egui::vec2(94.0, -10.0),
        Align2::LEFT_BOTTOM,
        theme.description(),
        FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    if selected {
        ui.painter().circle_filled(
            rect.right_center() - egui::vec2(17.0, 0.0),
            5.0,
            themed_effect_color(theme, 230),
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), Sense::hover());
    let visuals = ui.visuals();
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.widgets.hovered.bg_fill);
    }

    let label_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(2.0, 0.0),
        egui::pos2(rect.left() + 66.0, rect.bottom()),
    );
    ui.painter().text(
        label_rect.left_center(),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        visuals.weak_text_color(),
    );

    let value_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 72.0, rect.top()),
        rect.right_bottom() - egui::vec2(2.0, 0.0),
    );
    ui.painter().with_clip_rect(value_rect).text(
        value_rect.left_center(),
        Align2::LEFT_CENTER,
        value,
        FontId::monospace(12.5),
        visuals.text_color(),
    );
    let _ = response.on_hover_text(value);
}

fn inspector_note(ui: &mut egui::Ui, text: &str, loading: bool, theme: UiTheme) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 42.0), Sense::hover());
    let fill = match theme {
        UiTheme::Graphite => Color32::from_rgb(34, 35, 39),
        UiTheme::Frost => Color32::from_rgb(229, 237, 246),
        UiTheme::Retrowave => Color32::from_rgb(35, 18, 58),
    };
    let text_color = match theme {
        UiTheme::Frost => Color32::from_rgb(58, 78, 98),
        UiTheme::Graphite | UiTheme::Retrowave => Color32::from_rgb(214, 221, 232),
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    ui.painter().rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0, themed_effect_color(theme, 70)),
        StrokeKind::Inside,
    );

    let marker_center = rect.left_center() + egui::vec2(15.0, 0.0);
    if loading {
        let time = ui.input(|input| input.time);
        let pulse = ((time * 3.0).sin() * 0.5 + 0.5) as f32;
        ui.painter().circle_stroke(
            marker_center,
            5.0 + pulse * 1.5,
            Stroke::new(1.5, themed_effect_color(theme, 150)),
        );
    } else {
        ui.painter()
            .circle_filled(marker_center, 4.0, themed_effect_color(theme, 120));
    }

    let text_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(30.0, 0.0),
        rect.right_bottom() - egui::vec2(8.0, 0.0),
    );
    ui.painter().with_clip_rect(text_rect).text(
        text_rect.left_center(),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(13.0),
        text_color,
    );
}

fn selected_summary_card(
    ui: &mut egui::Ui,
    entry: &EntryView,
    parent_share: Option<f64>,
    theme: UiTheme,
) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 82.0), Sense::hover());
    let visuals = ui.visuals();
    ui.painter()
        .rect_filled(rect, 6.0, visuals.widgets.inactive.bg_fill);
    ui.painter().rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0, themed_effect_color(theme, 76)),
        StrokeKind::Inside,
    );

    let icon_rect =
        egui::Rect::from_min_size(rect.left_top() + egui::vec2(12.0, 13.0), Vec2::splat(30.0));
    paint_symbolic_icon(
        ui.painter(),
        icon_rect,
        if entry.is_dir {
            IconKind::Folder
        } else {
            IconKind::File
        },
        themed_effect_color(theme, 230),
    );

    let title_clip = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(52.0, 10.0),
        egui::pos2(rect.right() - 10.0, rect.top() + 34.0),
    );
    ui.painter().with_clip_rect(title_clip).text(
        title_clip.left_top(),
        Align2::LEFT_TOP,
        &entry.name,
        FontId::monospace(15.0),
        visuals.text_color(),
    );

    let kind = if entry.is_dir { "folder" } else { "file" };
    let share = parent_share
        .map(|share| format!("{} of parent", percentage_text(share)))
        .unwrap_or_else(|| "parent share unavailable".to_string());
    let detail = format!("{}  |  {}  |  {}", human_bytes(entry.size), kind, share);
    let detail_clip = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(52.0, 38.0),
        egui::pos2(rect.right() - 10.0, rect.top() + 60.0),
    );
    ui.painter().with_clip_rect(detail_clip).text(
        detail_clip.left_top(),
        Align2::LEFT_TOP,
        &detail,
        FontId::proportional(13.0),
        visuals.weak_text_color(),
    );

    if let Some(share) = parent_share {
        let track = egui::Rect::from_min_max(
            rect.left_bottom() + egui::vec2(12.0, -13.0),
            rect.right_bottom() - egui::vec2(12.0, 9.0),
        );
        ui.painter()
            .rect_filled(track, 2.0, visuals.extreme_bg_color);
        let fill = egui::Rect::from_min_max(
            track.left_top(),
            egui::pos2(track.left() + track.width() * share as f32, track.bottom()),
        );
        ui.painter()
            .rect_filled(fill, 2.0, themed_effect_color(theme, 180));
    }
}

fn percentage_text(value: f64) -> String {
    let percent = (value * 100.0).clamp(0.0, 100.0);
    if percent >= 10.0 {
        format!("{percent:.0}%")
    } else if percent >= 1.0 {
        format!("{percent:.1}%")
    } else if percent > 0.0 {
        String::from("<1%")
    } else {
        String::from("0%")
    }
}

fn size_legend(ui: &mut egui::Ui, theme: UiTheme) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("small").small().weak());
        for i in 0..5 {
            let t = i as u128 + 1;
            let color = size_color(t, 5, theme);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 10.0), Sense::hover());
            ui.painter().rect_filled(rect, 2.0, color);
        }
        ui.label(RichText::new("large").small().weak());
    });
}

fn type_legend(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let (folder_rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::hover());
        paint_symbolic_icon(
            ui.painter(),
            folder_rect,
            IconKind::Folder,
            ui.visuals().text_color(),
        );
        ui.label(RichText::new("folder").small().weak());
        let (file_rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::hover());
        paint_symbolic_icon(
            ui.painter(),
            file_rect,
            IconKind::File,
            ui.visuals().text_color(),
        );
        ui.label(RichText::new("file").small().weak());
    });
}

fn big_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    text_button(ui, text, egui::vec2(86.0, 36.0), false)
}

fn text_button(ui: &mut egui::Ui, text: &str, min_size: egui::Vec2, small: bool) -> egui::Response {
    let font_size = if small { 12.0 } else { 14.0 };
    let text_color = ui.visuals().text_color();
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        FontId::proportional(font_size),
        text_color,
    );
    let desired_size = egui::vec2(
        min_size.x.max(galley.size().x + 22.0),
        min_size.y.max(galley.size().y + 12.0),
    );
    let width = desired_size.x.min(ui.available_width().max(min_size.x));
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, desired_size.y), Sense::click());
    let visuals = ui.style().interact(&response);
    ui.painter().rect_filled(rect, 5.0, visuals.bg_fill);
    ui.painter()
        .rect_stroke(rect, 5.0, visuals.bg_stroke, StrokeKind::Inside);

    let clip = rect.shrink2(egui::vec2(10.0, 0.0));
    ui.painter().with_clip_rect(clip).galley(
        rect.center() - galley.size() * 0.5,
        galley,
        visuals.text_color(),
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn icon_button(ui: &mut egui::Ui, icon: IconKind, tooltip: &str) -> egui::Response {
    icon_button_enabled(ui, icon, tooltip, true)
}

fn icon_button_enabled(
    ui: &mut egui::Ui,
    icon: IconKind,
    tooltip: &str,
    enabled: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(44.0, 40.0), Sense::click());
    let response = if enabled {
        response
    } else {
        response.on_disabled_hover_text(tooltip)
    };
    let visuals = if enabled {
        ui.style().interact(&response)
    } else {
        &ui.visuals().widgets.noninteractive
    };
    ui.painter().rect_filled(rect, 4.0, visuals.bg_fill);
    ui.painter()
        .rect_stroke(rect, 4.0, visuals.bg_stroke, StrokeKind::Inside);
    paint_symbolic_icon(
        ui.painter(),
        rect.shrink2(egui::vec2(11.0, 9.0)),
        icon,
        visuals.fg_stroke.color,
    );
    if enabled {
        response
            .on_hover_text(tooltip)
            .on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

fn toolbar_up_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    icon_button_enabled(ui, IconKind::Up, "Go up (Backspace)", enabled)
}

fn paint_treemap_empty_state(
    painter: &egui::Painter,
    rect: egui::Rect,
    text: &str,
    loading: bool,
    time: f64,
    theme: UiTheme,
) {
    let foreground = match theme {
        UiTheme::Frost => Color32::from_rgb(64, 82, 100),
        UiTheme::Graphite | UiTheme::Retrowave => Color32::from_rgb(205, 212, 224),
    };
    let center = rect.center();

    if loading {
        let pulse = ((time * 3.0).sin() * 0.5 + 0.5) as f32;
        painter.circle_stroke(
            center - egui::vec2(0.0, 26.0),
            13.0 + pulse * 3.0,
            Stroke::new(2.0, themed_effect_color(theme, 120)),
        );
        painter.circle_stroke(
            center - egui::vec2(0.0, 26.0),
            6.0,
            Stroke::new(2.0, themed_effect_color(theme, 210)),
        );
    }

    painter.text(
        center + egui::vec2(0.0, if loading { 8.0 } else { 0.0 }),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(18.0),
        foreground,
    );
}

fn paint_scan_activity_badge(
    painter: &egui::Painter,
    rect: egui::Rect,
    stats: &scan::ScanStats,
    theme: UiTheme,
) {
    let text = format!(
        "scanning  {} entries  {} jobs",
        stats.entries_traversed, stats.active_jobs
    );
    let width = (text.chars().count() as f32 * 7.2 + 24.0).clamp(168.0, 260.0);
    let badge = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(12.0, 12.0),
        egui::vec2(width, 30.0),
    );
    let bg = match theme {
        UiTheme::Frost => Color32::from_rgba_unmultiplied(245, 248, 252, 224),
        UiTheme::Graphite => Color32::from_rgba_unmultiplied(23, 24, 27, 224),
        UiTheme::Retrowave => Color32::from_rgba_unmultiplied(12, 7, 30, 224),
    };
    let fg = match theme {
        UiTheme::Frost => Color32::from_rgb(36, 78, 112),
        UiTheme::Graphite | UiTheme::Retrowave => Color32::from_rgb(232, 238, 246),
    };
    painter.rect_filled(badge, 5.0, bg);
    painter.rect_stroke(
        badge,
        5.0,
        Stroke::new(1.0, themed_effect_color(theme, 110)),
        StrokeKind::Inside,
    );
    painter.circle_filled(
        badge.left_center() + egui::vec2(12.0, 0.0),
        4.0,
        themed_effect_color(theme, 210),
    );
    painter.text(
        badge.left_center() + egui::vec2(22.0, 0.0),
        Align2::LEFT_CENTER,
        text,
        FontId::monospace(12.0),
        fg,
    );
}

fn paint_tree_background(painter: &egui::Painter, rect: egui::Rect, time: f64, theme: UiTheme) {
    let (fill, line, accent) = match theme {
        UiTheme::Graphite => (
            Color32::from_rgb(20, 21, 23),
            Color32::from_rgba_unmultiplied(145, 150, 158, 18),
            Color32::from_rgba_unmultiplied(150, 156, 166, 58),
        ),
        UiTheme::Frost => (
            Color32::from_rgb(238, 242, 247),
            Color32::from_rgba_unmultiplied(70, 84, 100, 24),
            Color32::from_rgba_unmultiplied(30, 120, 180, 70),
        ),
        UiTheme::Retrowave => (
            Color32::from_rgb(7, 4, 20),
            Color32::from_rgba_unmultiplied(255, 64, 214, 22),
            Color32::from_rgba_unmultiplied(255, 42, 205, 70),
        ),
    };

    painter.rect_filled(rect, 0.0, fill);

    let pulse = ((time * 2.5).sin() * 0.5 + 0.5) as f32;
    let rows = if theme == UiTheme::Retrowave { 18 } else { 12 };
    for i in 0..rows {
        let y = rect.top() + i as f32 * rect.height() / rows as f32 + pulse * 2.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.0, line),
        );
    }

    if theme == UiTheme::Retrowave {
        let horizon = rect.bottom() - rect.height() * 0.18;
        for i in 0..14 {
            let t = i as f32 / 13.0;
            let x = egui::lerp(rect.left()..=rect.right(), t);
            painter.line_segment(
                [
                    egui::pos2(rect.center().x, horizon),
                    egui::pos2(x, rect.bottom()),
                ],
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(70, 220, 255, 42)),
            );
        }
    }

    painter.rect_stroke(
        rect.shrink(2.0),
        6.0,
        Stroke::new(2.0, accent),
        StrokeKind::Inside,
    );
}

fn paint_tile_body(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: Color32,
    is_dir: bool,
    theme: UiTheme,
) {
    let foreground = tile_foreground_color(color);
    if is_dir {
        painter.rect_filled(rect, 2.0, color);
        let header = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(rect.right(), (rect.top() + 20.0).min(rect.bottom())),
        );
        painter.rect_filled(header, 2.0, overlay_color(theme, 54));
        if rect.width() > 42.0 && rect.height() > 28.0 {
            let tab = egui::Rect::from_min_size(
                rect.left_top() + egui::vec2(7.0, 3.0),
                egui::vec2((rect.width() * 0.16).clamp(16.0, 42.0), 4.0),
            );
            painter.rect_filled(tab, 1.0, color_with_alpha(foreground, 105));
            paint_symbolic_icon(
                painter,
                egui::Rect::from_min_size(
                    rect.left_top() + egui::vec2(9.0, 8.0),
                    Vec2::splat(14.0),
                ),
                IconKind::Folder,
                color_with_alpha(foreground, 190),
            );
        }
    } else {
        let file_color = soften_color(color, theme);
        painter.rect_filled(rect, 2.0, file_color);
        painter.rect_stroke(
            rect.shrink(4.0),
            1.0,
            Stroke::new(1.0, color_with_alpha(foreground, 95)),
            StrokeKind::Inside,
        );
        if rect.width() > 36.0 && rect.height() > 36.0 {
            paint_symbolic_icon(
                painter,
                egui::Rect::from_min_size(
                    rect.left_top() + egui::vec2(9.0, 9.0),
                    Vec2::splat(14.0),
                ),
                IconKind::File,
                color_with_alpha(foreground, 190),
            );
        }
    }
}

fn paint_tile_label(painter: &egui::Painter, rect: egui::Rect, entry: &EntryView, color: Color32) {
    let area = rect.width() * rect.height();
    if rect.width() < 42.0 || rect.height() < 28.0 || area < 1_700.0 {
        return;
    }

    let text_color = tile_foreground_color(color);
    let icon_offset = if entry.is_dir { 31.0 } else { 30.0 };
    let can_show_name = rect.width() > 74.0 && rect.height() > 34.0;
    let can_show_size = rect.width() > 92.0 && rect.height() > 50.0;
    let detailed = rect.width() > 168.0 && rect.height() > 78.0;

    if can_show_name {
        let font_size = if detailed {
            15.0
        } else if rect.width() > 120.0 {
            13.5
        } else {
            12.0
        };
        let label_clip = egui::Rect::from_min_max(
            rect.left_top() + egui::vec2(icon_offset, 7.0),
            egui::pos2(rect.right() - 6.0, rect.top() + 25.0),
        );
        painter.with_clip_rect(label_clip).text(
            rect.left_top() + egui::vec2(icon_offset, 7.0),
            Align2::LEFT_TOP,
            &entry.name,
            FontId::monospace(font_size),
            text_color,
        );
    }

    if can_show_size {
        let size_text = human_bytes(entry.size);
        let fitted_size = fit_text_to_width(&size_text, (rect.width() - 16.0).max(12.0), 7.0);
        painter.text(
            rect.left_bottom() + egui::vec2(8.0, -7.0),
            Align2::LEFT_BOTTOM,
            fitted_size,
            FontId::monospace(if detailed { 13.0 } else { 11.5 }),
            color_with_alpha(text_color, 220),
        );
    }

    if detailed && entry.is_dir {
        let count = entry
            .entry_count
            .map(|count| format!("{count} entries"))
            .unwrap_or_else(|| String::from("not scanned"));
        let fitted_count = fit_text_to_width(&count, (rect.width() - 16.0).max(12.0), 6.5);
        painter.text(
            rect.left_top() + egui::vec2(8.0, 31.0),
            Align2::LEFT_TOP,
            fitted_count,
            FontId::proportional(12.0),
            color_with_alpha(text_color, 185),
        );
    }
}

fn expanded_child_rect(rect: egui::Rect, depth: usize) -> egui::Rect {
    let inset = 5.0 + depth as f32;
    let header_height = if rect.height() > 72.0 { 26.0 } else { 0.0 };
    egui::Rect::from_min_max(
        egui::pos2(rect.left() + inset, rect.top() + inset + header_height),
        egui::pos2(rect.right() - inset, rect.bottom() - inset),
    )
}

fn paint_expanded_directory_frame(
    painter: &egui::Painter,
    rect: egui::Rect,
    entry: &EntryView,
    theme: UiTheme,
) {
    painter.rect_filled(rect, 2.0, overlay_color(theme, 125));
    if rect.width() < 46.0 || rect.height() < 40.0 {
        return;
    }

    let header_height = 26.0_f32.min(rect.height() * 0.42);
    let header = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(rect.right(), rect.top() + header_height),
    );
    painter.rect_filled(header, 2.0, overlay_color(theme, 178));
    painter.line_segment(
        [
            egui::pos2(header.left(), header.bottom()),
            egui::pos2(header.right(), header.bottom()),
        ],
        Stroke::new(1.0, themed_effect_color(theme, 78)),
    );

    if rect.width() > 90.0 && header.height() > 20.0 {
        let foreground = match theme {
            UiTheme::Frost => Color32::from_rgb(26, 43, 58),
            UiTheme::Graphite | UiTheme::Retrowave => Color32::from_rgb(240, 244, 248),
        };
        paint_symbolic_icon(
            painter,
            egui::Rect::from_min_size(header.left_top() + egui::vec2(8.0, 6.0), Vec2::splat(14.0)),
            IconKind::Folder,
            color_with_alpha(foreground, 205),
        );

        painter
            .with_clip_rect(header.shrink2(egui::vec2(28.0, 0.0)))
            .text(
                header.left_center() + egui::vec2(29.0, 0.0),
                Align2::LEFT_CENTER,
                &entry.name,
                FontId::monospace(13.0),
                foreground,
            );

        let size = human_bytes(entry.size);
        let fitted_size = fit_text_to_width(&size, 72.0, 7.2);
        painter.text(
            header.right_center() - egui::vec2(8.0, 0.0),
            Align2::RIGHT_CENTER,
            fitted_size,
            FontId::monospace(12.0),
            color_with_alpha(foreground, 190),
        );
    }
}

fn tile_foreground_color(color: Color32) -> Color32 {
    let luminance =
        (0.2126 * color.r() as f32 + 0.7152 * color.g() as f32 + 0.0722 * color.b() as f32) / 255.0;
    if luminance > 0.58 {
        Color32::from_rgb(24, 28, 32)
    } else {
        Color32::from_rgb(248, 250, 252)
    }
}

fn color_with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn paint_selected_breathing(painter: &egui::Painter, rect: egui::Rect, _time: f64, theme: UiTheme) {
    painter.rect_stroke(
        rect.expand(4.0),
        5.0,
        Stroke::new(2.0, themed_effect_color(theme, 96)),
        StrokeKind::Outside,
    );
}

fn paint_click_pulses(
    painter: &egui::Painter,
    rect: egui::Rect,
    index: TreeIndex,
    pulses: &[ClickPulse],
    time: f64,
    theme: UiTheme,
) {
    if rect.width() < 12.0 || rect.height() < 12.0 {
        return;
    }

    let clip_painter = painter.with_clip_rect(rect.shrink(1.0));
    let max_radius = rect.width().hypot(rect.height()) * 0.32;
    for pulse in pulses.iter().filter(|pulse| pulse.index == index) {
        let age = ((time - pulse.started_at) / 0.7).clamp(0.0, 1.0) as f32;
        let radius = egui::lerp(4.0..=max_radius, age);
        let alpha = ((1.0 - age).powf(1.8) * 150.0) as u8;
        let color = themed_effect_color(theme, alpha);
        clip_painter.circle_stroke(
            pulse.position,
            radius,
            Stroke::new(2.0 * (1.0 - age) + 0.5, color),
        );
        clip_painter.circle_filled(
            pulse.position,
            radius * 0.22,
            themed_effect_color(theme, (alpha as f32 * 0.32) as u8),
        );
    }
}

fn paint_expand_reveal_cover(
    painter: &egui::Painter,
    rect: egui::Rect,
    reveal_t: f32,
    theme: UiTheme,
) {
    if reveal_t >= 1.0 {
        return;
    }

    let alpha = ((1.0 - reveal_t).powf(1.6) * 135.0) as u8;
    let cover = match theme {
        UiTheme::Graphite => Color32::from_rgba_unmultiplied(20, 21, 23, alpha),
        UiTheme::Frost => Color32::from_rgba_unmultiplied(238, 242, 247, alpha),
        UiTheme::Retrowave => Color32::from_rgba_unmultiplied(7, 4, 20, alpha),
    };
    painter.rect_filled(rect, 2.0, cover);
    painter.rect_stroke(
        rect.shrink(2.0 + reveal_t * 8.0),
        4.0,
        Stroke::new(
            2.0,
            themed_effect_color(theme, (90.0 * (1.0 - reveal_t)) as u8),
        ),
        StrokeKind::Inside,
    );
}

fn themed_effect_color(theme: UiTheme, alpha: u8) -> Color32 {
    match theme {
        UiTheme::Graphite => Color32::from_rgba_unmultiplied(230, 236, 245, alpha),
        UiTheme::Frost => Color32::from_rgba_unmultiplied(35, 100, 150, alpha),
        UiTheme::Retrowave => Color32::from_rgba_unmultiplied(255, 236, 126, alpha),
    }
}

fn paint_symbolic_icon(painter: &egui::Painter, rect: egui::Rect, icon: IconKind, color: Color32) {
    let stroke = Stroke::new(1.8, color);
    match icon {
        IconKind::Up => {
            let c = rect.center();
            painter.line_segment(
                [
                    egui::pos2(rect.left() + rect.width() * 0.18, c.y),
                    egui::pos2(c.x, rect.top() + rect.height() * 0.18),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.top() + rect.height() * 0.18),
                    egui::pos2(rect.right() - rect.width() * 0.18, c.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.top() + rect.height() * 0.18),
                    egui::pos2(c.x, rect.bottom() - rect.height() * 0.15),
                ],
                stroke,
            );
        }
        IconKind::Folder => {
            let tab = egui::Rect::from_min_max(
                egui::pos2(
                    rect.left() + rect.width() * 0.08,
                    rect.top() + rect.height() * 0.20,
                ),
                egui::pos2(
                    rect.left() + rect.width() * 0.44,
                    rect.top() + rect.height() * 0.38,
                ),
            );
            let body = egui::Rect::from_min_max(
                egui::pos2(
                    rect.left() + rect.width() * 0.08,
                    rect.top() + rect.height() * 0.32,
                ),
                egui::pos2(
                    rect.right() - rect.width() * 0.08,
                    rect.bottom() - rect.height() * 0.16,
                ),
            );
            painter.rect_stroke(tab, 2.0, stroke, StrokeKind::Inside);
            painter.rect_stroke(body, 2.0, stroke, StrokeKind::Inside);
        }
        IconKind::Refresh => {
            painter.circle_stroke(
                rect.center(),
                rect.width().min(rect.height()) * 0.32,
                stroke,
            );
            let p = egui::pos2(
                rect.right() - rect.width() * 0.20,
                rect.top() + rect.height() * 0.34,
            );
            painter.add(Shape::convex_polygon(
                vec![p, p + egui::vec2(-5.0, -1.0), p + egui::vec2(-1.0, 5.0)],
                color,
                Stroke::NONE,
            ));
        }
        IconKind::Settings => {
            painter.circle_stroke(
                rect.center(),
                rect.width().min(rect.height()) * 0.19,
                stroke,
            );
            let c = rect.center();
            for i in 0..8 {
                let angle = i as f32 * std::f32::consts::TAU / 8.0;
                let inner = egui::vec2(angle.cos(), angle.sin()) * rect.width() * 0.29;
                let outer = egui::vec2(angle.cos(), angle.sin()) * rect.width() * 0.40;
                painter.line_segment([c + inner, c + outer], stroke);
            }
        }
        IconKind::Sliders => {
            for (y, knob_x) in [(0.28, 0.68), (0.50, 0.34), (0.72, 0.56)] {
                let left = egui::pos2(
                    rect.left() + rect.width() * 0.16,
                    rect.top() + rect.height() * y,
                );
                let right = egui::pos2(rect.right() - rect.width() * 0.16, left.y);
                painter.line_segment([left, right], stroke);
                painter.circle_filled(
                    egui::pos2(rect.left() + rect.width() * knob_x, left.y),
                    rect.width().min(rect.height()) * 0.055,
                    color,
                );
            }
        }
        IconKind::Palette => {
            painter.circle_stroke(
                rect.center(),
                rect.width().min(rect.height()) * 0.34,
                stroke,
            );
            for (dx, dy) in [(-0.12, -0.12), (0.12, -0.16), (-0.18, 0.12)] {
                painter.circle_filled(
                    rect.center() + egui::vec2(rect.width() * dx, rect.height() * dy),
                    rect.width().min(rect.height()) * 0.045,
                    color,
                );
            }
            painter.circle_stroke(
                rect.center() + egui::vec2(rect.width() * 0.16, rect.height() * 0.13),
                rect.width().min(rect.height()) * 0.06,
                stroke,
            );
        }
        IconKind::Drive => {
            let body = egui::Rect::from_min_max(
                egui::pos2(
                    rect.left() + rect.width() * 0.12,
                    rect.top() + rect.height() * 0.20,
                ),
                egui::pos2(
                    rect.right() - rect.width() * 0.12,
                    rect.bottom() - rect.height() * 0.18,
                ),
            );
            painter.rect_stroke(body, 2.0, stroke, StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(
                        body.left() + rect.width() * 0.12,
                        body.bottom() - rect.height() * 0.20,
                    ),
                    egui::pos2(
                        body.right() - rect.width() * 0.12,
                        body.bottom() - rect.height() * 0.20,
                    ),
                ],
                stroke,
            );
            painter.circle_filled(
                egui::pos2(
                    body.right() - rect.width() * 0.18,
                    body.bottom() - rect.height() * 0.10,
                ),
                rect.width().min(rect.height()) * 0.035,
                color,
            );
        }
        IconKind::File => {
            let body = egui::Rect::from_min_max(
                egui::pos2(
                    rect.left() + rect.width() * 0.20,
                    rect.top() + rect.height() * 0.08,
                ),
                egui::pos2(
                    rect.right() - rect.width() * 0.18,
                    rect.bottom() - rect.height() * 0.08,
                ),
            );
            painter.rect_stroke(body, 1.0, stroke, StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(body.right() - rect.width() * 0.22, body.top()),
                    egui::pos2(body.right(), body.top() + rect.height() * 0.22),
                ],
                stroke,
            );
        }
    }
}

fn overlay_color(theme: UiTheme, alpha: u8) -> Color32 {
    match theme {
        UiTheme::Graphite => Color32::from_rgba_unmultiplied(18, 19, 21, alpha),
        UiTheme::Frost => Color32::from_rgba_unmultiplied(255, 255, 255, alpha),
        UiTheme::Retrowave => Color32::from_rgba_unmultiplied(26, 5, 45, alpha),
    }
}

fn soften_color(color: Color32, theme: UiTheme) -> Color32 {
    let mix = match theme {
        UiTheme::Frost => Color32::from_rgb(246, 248, 251),
        UiTheme::Graphite => Color32::from_rgb(42, 44, 48),
        UiTheme::Retrowave => Color32::from_rgb(24, 12, 44),
    };
    lerp_color(color, mix, 0.24)
}

fn size_color(size: u128, max_size: u128, theme: UiTheme) -> Color32 {
    let ratio = if max_size == 0 {
        0.0
    } else {
        (size as f64 / max_size as f64).clamp(0.0, 1.0) as f32
    };
    let boosted = ratio.sqrt();
    let (small, medium, large) = match theme {
        UiTheme::Graphite => (
            Color32::from_rgb(76, 150, 96),
            Color32::from_rgb(185, 154, 73),
            Color32::from_rgb(190, 83, 74),
        ),
        UiTheme::Frost => (
            Color32::from_rgb(64, 168, 104),
            Color32::from_rgb(224, 174, 67),
            Color32::from_rgb(219, 82, 82),
        ),
        UiTheme::Retrowave => (
            Color32::from_rgb(38, 224, 154),
            Color32::from_rgb(255, 202, 76),
            Color32::from_rgb(255, 58, 118),
        ),
    };

    if boosted < 0.5 {
        lerp_color(small, medium, boosted * 2.0)
    } else {
        lerp_color(medium, large, (boosted - 0.5) * 2.0)
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_active_filters() {
        assert_eq!(
            active_filter_summary(&Filters::default()),
            "no active filters"
        );
        assert_eq!(
            active_filter_summary(&Filters {
                name_substring: "target".into(),
                ..Filters::default()
            }),
            "1 active filter"
        );
        assert_eq!(
            active_filter_summary(&Filters {
                name_substring: "target".into(),
                min_size_bytes: 1024,
                max_age_days: 7,
            }),
            "3 active filters"
        );
    }
}
