//! Application state and input handling. Queries run on background threads
//! and report back through a channel. Mutating commands run in an embedded
//! terminal pane (`job`), or are handed to the main loop via `run_request`
//! so it can suspend the TUI.
//!
//! No code path here may panic: lookups use `get`, arithmetic saturates.

use crate::backend::{Backend, Cmd, DiskUsage, Multi, Package, Repo, Section, VersionChoice, is_unsupported};
use crate::config::{Config, RunMode};
use crate::history::{Draft, Op};
use crate::term::{Job, JobState};
use crate::theme::Colors;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Search,
    Installed,
    Orphans,
    Updates,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Search, Tab::Installed, Tab::Orphans, Tab::Updates];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Search => "Search",
            Tab::Installed => "Installed",
            Tab::Orphans => "Orphans",
            Tab::Updates => "Updates",
        }
    }
    fn position(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
    /// The tab `delta` steps away, wrapping around.
    fn step(self, delta: isize) -> Tab {
        let n = Tab::ALL.len() as isize;
        let i = (self.position() as isize).saturating_add(delta).rem_euclid(n);
        Tab::ALL.get(i as usize).copied().unwrap_or(Tab::Search)
    }
}

/// What confirming a popup does.
pub enum PendingAction {
    /// Run `Pending::cmds`.
    Run,
    /// Write an export file (replacing any existing one).
    WriteExport { path: PathBuf, text: String },
}

pub struct Pending {
    pub title: String,
    pub cmds: Vec<Cmd>,
    /// Extra lines shown above the commands (skipped packages, plans).
    pub notes: Vec<String>,
    pub action: PendingAction,
    /// Always ask, even with `confirm = false` (file writes, imports).
    pub force_confirm: bool,
    /// What the history records once the commands have run.
    pub record: Option<Draft>,
}

impl Pending {
    /// Commands to run, and what the history records once they ran (a
    /// required argument: no run can be left out by accident; use
    /// `Draft::other()` for runs that cannot be undone).
    pub fn run(title: String, cmds: Vec<Cmd>, record: Draft) -> Self {
        Self {
            title,
            cmds,
            notes: Vec::new(),
            action: PendingAction::Run,
            force_confirm: false,
            record: Some(record),
        }
    }
}

pub enum Mode {
    Normal,
    Input,
    Confirm(Pending),
    Help,
    /// Choosing a version of a package to install.
    Versions(VersionPick),
    /// Repositories (Homebrew taps) of the active package managers.
    Repos(RepoView),
    /// A package's dependency tree.
    Tree(DepTree),
    /// Installed size per package manager and the largest packages.
    Disk(DiskView),
    /// The theme picker: moving the selection previews a theme.
    Themes(ThemePick),
    /// What pakdeck ran, newest first; Enter undoes the selected entry.
    History(HistoryView),
}

pub struct HistoryView {
    /// Newest first.
    pub entries: Vec<crate::history::Entry>,
    /// Why each entry cannot be undone here (None: it can), decided when
    /// the popup opened.
    pub why_not: Vec<Option<String>>,
    pub selected: usize,
    /// Why the last undo could not be planned (shown in the popup).
    pub error: Option<String>,
}

pub struct ThemePick {
    /// Every theme (name, kind), built-ins first.
    pub themes: Vec<(String, crate::theme::Kind)>,
    pub selected: usize,
    /// The theme in use when the picker opened (Esc goes back to it).
    pub original: String,
}

pub struct RepoView {
    /// What they are called ("Taps").
    pub kind: &'static str,
    pub repos: Vec<Repo>,
    /// Installed packages from each repository (same order as `repos`).
    pub installed: Vec<usize>,
    pub selected: usize,
    /// The repository being typed after `a` (None: not adding).
    pub adding: Option<String>,
    /// Why the last add or remove was refused (shown in the popup).
    pub error: Option<String>,
    /// How to write a repository to add.
    pub add_hint: &'static str,
}

impl RepoView {
    /// The package manager a new repository goes to: the selected
    /// repository's (empty: the first one that has repositories).
    pub fn add_target(&self) -> &str {
        self.repos
            .get(self.selected)
            .or_else(|| self.repos.first())
            .map_or("", |r| r.source.as_str())
    }

    /// One of them, in lower case: "tap", "repository".
    pub fn noun(&self) -> String {
        singular(self.kind)
    }
}

/// "Taps" -> "tap", "Repositories" -> "repository".
pub fn singular(kind: &str) -> String {
    let kind = kind.to_lowercase();
    match kind.strip_suffix("ies") {
        Some(stem) => format!("{stem}y"),
        None => kind.strip_suffix('s').map_or(kind.clone(), str::to_string),
    }
}

/// One node of a dependency tree. Children load when first expanded.
pub struct TreeNode {
    pub name: String,
    pub depth: usize,
    pub parent: Option<usize>,
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    pub loading: bool,
    /// Already an ancestor: shown, never expanded.
    pub cycle: bool,
    pub error: Option<String>,
}

pub struct DepTree {
    /// Distinguishes answers for this tree from an earlier one.
    pub id: u64,
    pub root: Package,
    /// `Depends` or `RequiredBy`.
    pub direction: Section,
    pub nodes: Vec<TreeNode>,
    /// The selected node (an index into `nodes`, not a row): rows move
    /// when children load, the selection must not.
    pub selected: usize,
}

impl DepTree {
    fn new(id: u64, root: Package, direction: Section) -> Self {
        let node = TreeNode {
            name: root.name.clone(),
            depth: 0,
            parent: None,
            children: None,
            expanded: true,
            loading: false,
            cycle: false,
            error: None,
        };
        Self {
            id,
            root,
            direction,
            nodes: vec![node],
            selected: 0,
        }
    }

    /// Node indices in display order (expanded nodes show their children).
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![0usize];
        while let Some(i) = stack.pop() {
            let Some(n) = self.nodes.get(i) else { continue };
            out.push(i);
            if n.expanded
                && let Some(children) = &n.children
            {
                stack.extend(children.iter().rev());
            }
        }
        out
    }

    /// The selected node's row in `visible()`.
    pub fn selected_row(&self) -> usize {
        self.visible().iter().position(|i| *i == self.selected).unwrap_or(0)
    }

    /// Select the node shown in `row` (clamped to the last row).
    fn select_row(&mut self, row: usize) {
        let visible = self.visible();
        if let Some(i) = visible.get(row).or_else(|| visible.last()) {
            self.selected = *i;
        }
    }

    fn is_ancestor(&self, mut at: Option<usize>, name: &str) -> bool {
        while let Some(i) = at {
            let Some(n) = self.nodes.get(i) else { return false };
            if n.name == name {
                return true;
            }
            at = n.parent;
        }
        false
    }

    /// Attach loaded children to `node`, marking cycles.
    fn set_children(&mut self, node: usize, result: Result<Vec<String>, String>) {
        let depth = self.nodes.get(node).map_or(0, |n| n.depth.saturating_add(1));
        let mut kids = Vec::new();
        let names = match result {
            Ok(v) => v,
            Err(e) => {
                if let Some(n) = self.nodes.get_mut(node) {
                    n.loading = false;
                    n.error = Some(e);
                    n.children = Some(Vec::new());
                }
                return;
            }
        };
        for name in names {
            let cycle = self.is_ancestor(Some(node), &name);
            kids.push(self.nodes.len());
            self.nodes.push(TreeNode {
                name,
                depth,
                parent: Some(node),
                children: None,
                expanded: false,
                loading: false,
                cycle,
                error: None,
            });
        }
        if let Some(n) = self.nodes.get_mut(node) {
            n.loading = false;
            n.children = Some(kids);
        }
    }
}

/// A background size measurement of one package manager's packages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Measurement {
    Running,
    /// Finished (or failed: not retried before the next reload).
    Done,
}

pub struct DiskView {
    pub totals: Vec<DiskUsage>,
    /// The largest installed packages, biggest first.
    pub largest: Vec<Package>,
    pub selected: usize,
    /// Package managers whose sizes are being measured.
    pub measuring: Vec<String>,
    /// Package managers whose missing sizes are not measured now because
    /// the list is reloading (measured on the next open).
    pub postponed: Vec<String>,
}

impl DiskView {
    /// The view of `items`, keeping the selection on the package with key
    /// `keep` when it is still among the largest.
    fn build(items: &[Package], measuring: Vec<String>, postponed: Vec<String>, keep: Option<&str>) -> Self {
        let (totals, largest) = crate::backend::disk_summary(items, 15);
        let selected = keep
            .and_then(|k| largest.iter().position(|p| p.key() == k))
            .unwrap_or(0);
        Self {
            totals,
            largest,
            selected,
            measuring,
            postponed,
        }
    }
}

/// The version picker: available versions (newest first) of one package.
pub struct VersionPick {
    /// The package; `installed` / `version` describe the installed one.
    pub pkg: Package,
    pub versions: Vec<VersionChoice>,
    pub selected: usize,
    /// Hold the package at the chosen version afterwards (on by default:
    /// otherwise the next update undoes a downgrade).
    pub hold: bool,
    /// Whether this package manager can hold at all.
    pub can_hold: bool,
    /// The package is held now: the plan releases the hold first.
    pub held_now: bool,
}

/// List order. `Default` is relevance on the Search tab and name elsewhere;
/// size and date are offered only where the package manager provides them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Default,
    Name,
    Size,
    Date,
    Source,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Default => "default",
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Date => "install date",
            SortKey::Source => "source",
        }
    }
}

/// User-triggerable operations, shared by keys and clickable footer buttons.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Search,
    Mark,
    Install,
    Delete,
    Update,
    UpdateAll,
    Cleanup,
    Sync,
    Reload,
    DryRun,
    Explicit,
    Expand,
    RunMode,
    Hold,
    Source,
    Export,
    Import,
    Sort,
    Versions,
    Theme,
    Tree,
    Disk,
    Repos,
    History,
    ClearLog,
    Help,
    Quit,
}

/// A scrollbar being dragged with the mouse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bar {
    List,
    Details,
    Log,
}

/// Screen regions recorded during drawing, used to hit-test mouse events.
#[derive(Default)]
pub struct Hits {
    pub tabs: Vec<(Rect, Tab)>,
    pub input: Rect,
    /// Inner area of the package list (rows start at its top).
    pub list: Rect,
    pub details: Rect,
    /// Clickable section headers in the details pane.
    pub sections: Vec<(Rect, Section)>,
    pub log: Rect,
    /// Scrollbar columns (zero-sized when no scrollbar is shown).
    pub list_bar: Rect,
    pub details_bar: Rect,
    pub log_bar: Rect,
    /// The "clear" label in the Log pane's title.
    pub log_clear: Rect,
    pub footer: Vec<(Rect, Action)>,
    pub popup: Rect,
    pub confirm_yes: Rect,
    pub confirm_no: Rect,
    /// The embedded terminal pane (outer and terminal area).
    pub job: Rect,
    pub job_term: Rect,
    /// Clickable status-bar labels: package managers (source filter) and
    /// run mode.
    pub status_source: Rect,
    pub status_mode: Rect,
    /// Rows of the version picker and the other list popups (index into
    /// their items).
    pub popup_rows: Vec<(Rect, usize)>,
    pub hold_toggle: Rect,
}

/// Why a query produced no data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadErr {
    /// The package manager cannot do this (shown as information).
    Unsupported(String),
    Failed(String),
}

impl LoadErr {
    fn from(e: anyhow::Error) -> Self {
        if is_unsupported(&e) {
            LoadErr::Unsupported(e.to_string())
        } else {
            LoadErr::Failed(format!("{e:#}"))
        }
    }
}

/// State of one detail section for one package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SectionState {
    Loading,
    Ready(Vec<String>),
    Error(LoadErr),
}

enum Msg {
    List {
        tab: Tab,
        generation: u64,
        result: Result<Vec<Package>, LoadErr>,
    },
    // Messages below carry the reload epoch they were requested in;
    // results from before a reload are dropped.
    Explicit {
        epoch: u64,
        result: Result<ExplicitSet, LoadErr>,
    },
    Held {
        epoch: u64,
        result: Result<Vec<(String, String)>, LoadErr>,
    },
    Info {
        epoch: u64,
        key: String,
        result: Result<String, String>,
    },
    Section {
        epoch: u64,
        key: String,
        section: Section,
        result: Result<Vec<String>, LoadErr>,
    },
    Export(Result<(String, Vec<String>), String>),
    ImportPlan {
        path: PathBuf,
        result: Result<(Vec<Cmd>, Vec<String>), String>,
    },
    Cleanup(Result<Vec<Cmd>, String>),
    Repos(Result<Vec<Repo>, String>),
    /// Measured (`Package::key()`, bytes) for one package manager.
    Sizes {
        epoch: u64,
        source: String,
        result: Result<Vec<(String, u64)>, String>,
    },
    TreeChildren {
        tree: u64,
        node: usize,
        result: Result<Vec<String>, String>,
    },
    Versions {
        pkg: Package,
        result: Result<(Vec<VersionChoice>, bool), String>,
    },
    Plan {
        title: String,
        result: Result<(Vec<Cmd>, Vec<String>), String>,
        draft: Draft,
    },
}

/// Explicitly installed (source, name) pairs, and the sources that could
/// answer: packages of other sources are never filtered out.
#[derive(Default)]
pub struct ExplicitSet {
    names: HashSet<(String, String)>,
    covered: HashSet<String>,
}

impl ExplicitSet {
    fn keeps(&self, p: &Package) -> bool {
        !self.covered.contains(&p.source) || self.names.contains(&p.id())
    }
}

#[derive(Default)]
pub struct Pane {
    pub items: Vec<Package>,
    /// Indices into `items` that pass the filter.
    pub view: Vec<usize>,
    pub state: ListState,
    pub marked: HashSet<String>,
    pub filter: String,
    pub loading: bool,
    /// Set when the backend cannot provide this list at all.
    pub unsupported: Option<String>,
    /// Matched character positions in each visible name, aligned with
    /// `view` (computed with the view, not on every frame).
    pub highlights: Vec<Vec<usize>>,
    /// The sort actually applied (the global one, if this list has the data).
    pub sort: SortKey,
    /// The filter the view was last built with (a new filter re-ranks the
    /// rows, so the view starts at the top again).
    last_filter: String,
    generation: u64,
}

impl Pane {
    pub fn item(&self, view_row: usize) -> Option<&Package> {
        self.view.get(view_row).and_then(|&i| self.items.get(i))
    }
}

#[derive(Default)]
pub struct Panes {
    pub search: Pane,
    pub installed: Pane,
    pub orphans: Pane,
    pub updates: Pane,
}

impl Panes {
    pub fn get(&self, tab: Tab) -> &Pane {
        match tab {
            Tab::Search => &self.search,
            Tab::Installed => &self.installed,
            Tab::Orphans => &self.orphans,
            Tab::Updates => &self.updates,
        }
    }
    pub fn get_mut(&mut self, tab: Tab) -> &mut Pane {
        match tab {
            Tab::Search => &mut self.search,
            Tab::Installed => &mut self.installed,
            Tab::Orphans => &mut self.orphans,
            Tab::Updates => &mut self.updates,
        }
    }
    fn all_mut(&mut self) -> [&mut Pane; 4] {
        [
            &mut self.search,
            &mut self.installed,
            &mut self.orphans,
            &mut self.updates,
        ]
    }
}

/// The warning when commands need root but nothing can give it: two short
/// parts, shown as two log lines in the TUI (the log pane does not wrap)
/// and joined on one line by the command line.
pub const NO_ELEVATION: [&str; 2] = [
    "no sudo or doas found: commands that need root will fail",
    "install one, or set sudo = \"...\" in the config",
];

impl App {
    /// Say so up front when commands will need root but nothing can give
    /// it (not root, no sudo or doas, and no `sudo` set in the config):
    /// otherwise they would only fail once run.
    pub fn warn_no_elevation(&mut self) {
        if self.backend.needs_root() && self.sudo.is_none() {
            for part in NO_ELEVATION {
                self.log(format!("warning: {part}"));
            }
        }
    }
}

/// The startup log line naming the package managers, and the privilege
/// prefix only when one of them has root commands at all (Homebrew never
/// does, so naming sudo there would only confuse).
fn backend_line(backend: &Multi, sudo: Option<&str>) -> String {
    let elevate = sudo
        .filter(|_| backend.needs_root())
        .map(|s| format!(" (commands that need root use {s})"))
        .unwrap_or_default();
    format!("backend: {}{elevate}", backend.names().join(", "))
}

/// Move a selection one row in the direction of `delta` (the wheel),
/// staying within `len` rows.
fn step(selected: usize, delta: i16, len: usize) -> usize {
    let last = len.saturating_sub(1);
    if delta < 0 {
        selected.saturating_sub(1)
    } else {
        selected.saturating_add(1).min(last)
    }
}

/// Order search results by relevance: exact name, prefix, name contains, rest.
pub fn rank(items: &mut [Package], query: &str) {
    let q = query.trim().to_lowercase();
    items.sort_by_cached_key(|p| {
        let full = p.name.to_lowercase();
        // Rank tap packages ("user/tap/name") by their short name.
        let n = full.rsplit('/').next().unwrap_or(&full).to_string();
        let score = if n == q {
            0
        } else if n.starts_with(&q) {
            1
        } else if n.contains(&q) {
            2
        } else {
            3
        };
        (score, n.len(), full)
    });
}

const LOG_CAP: usize = 200;
const INFO_DELAY: Duration = Duration::from_millis(150);
/// Sections shown expanded until the user collapses them.
const DEFAULT_EXPANDED: [Section; 2] = [Section::Depends, Section::RequiredBy];

pub struct App {
    pub backend: Arc<Multi>,
    pub cfg: Config,
    pub colors: Colors,
    pub sudo: Option<String>,
    pub dry_run: bool,
    pub run_mode: RunMode,
    pub tab: Tab,
    pub panes: Panes,
    pub query: String,
    pub mode: Mode,
    /// Installed tab shows only explicitly installed packages.
    pub explicit_only: bool,
    explicit: Option<ExplicitSet>,
    explicit_loading: bool,
    /// Incremented by every reload; stale results are dropped.
    epoch: u64,
    /// Scroll offset of the help or confirm popup, and its largest value
    /// (computed when drawing).
    pub popup_scroll: u16,
    pub popup_max: u16,
    /// The active theme's name ("auto" included).
    pub theme: String,
    /// What "auto" means here (decided once at startup: NO_COLOR, then the
    /// terminal's background).
    pub auto_theme: String,
    /// Whether `auto_theme` was really decided (the terminal is asked only
    /// when the preset is "auto"); otherwise it is a stand-in until the
    /// next start.
    pub auto_known: bool,
    /// The terminal shows 24-bit colour (otherwise colours are fitted to 256).
    pub truecolor: bool,
    /// Where the config file lives: the theme picker saves the choice there.
    pub config_path: Option<PathBuf>,
    /// The history file (None: not recorded, e.g. no HOME).
    pub history_path: Option<PathBuf>,
    /// The run in progress and what to record when it finishes.
    running: Option<(Draft, String, Vec<Cmd>)>,
    pub sort: SortKey,
    /// Searches run this session, oldest first, and the position while
    /// stepping through them with Up/Down.
    pub search_history: Vec<String>,
    history_pos: Option<usize>,
    /// A version list that arrived while another prompt was open.
    queued_pick: Option<VersionPick>,
    /// Show only Installed packages from this (source, repository).
    pub repo_filter: Option<Repo>,
    /// Size measurements started since the last reload, per source.
    sizes: Vec<(String, Measurement)>,
    /// Sizes measured since the last reload, by `Package::key()`; applied
    /// again whenever the Installed list is reloaded.
    measured: HashMap<String, u64>,
    next_tree: u64,
    /// The last search actually run (highlights use it, not the draft).
    pub last_query: String,
    /// The text typed before stepping into the search history.
    history_draft: String,
    /// A confirmation that arrived while another prompt was open.
    queued: Option<Pending>,
    /// Held (pinned / locked) packages as (source, name).
    pub held: HashSet<(String, String)>,
    /// Show only packages from this package manager (several active).
    pub source_filter: Option<String>,
    pub details: HashMap<String, String>,
    pub sections: HashMap<(String, Section), SectionState>,
    pub expanded: HashSet<Section>,
    pub detail_scroll: u16,
    /// Largest useful `detail_scroll`, computed when drawing.
    pub detail_max: u16,
    /// Log lines scrolled up from the newest entry (0 = follow).
    pub log_scroll: usize,
    pub log_max: usize,
    pub log: VecDeque<String>,
    pub quit: bool,
    /// Commands the main loop should run with the terminal suspended.
    pub run_request: Option<Vec<Cmd>>,
    /// Commands running in the embedded terminal pane.
    pub job: Option<Job>,
    pub hits: Hits,
    /// Never start processes (fuzzing): runs go to `run_request` instead.
    pub sandbox: bool,
    dragging: Option<Bar>,
    inline: bool,
    installed: HashSet<(String, String)>,
    pending_info: HashSet<String>,
    sel_changed: Instant,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl App {
    pub fn new(backend: Arc<Multi>, cfg: Config, sudo: Option<String>) -> Self {
        Self::with_options(backend, cfg, sudo, false)
    }

    /// `inline`: run backend queries synchronously instead of on threads,
    /// and fetch details without the selection delay (tests and fuzzing).
    pub fn with_options(backend: Arc<Multi>, cfg: Config, sudo: Option<String>, inline: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            inline,
            theme: "dark".into(),
            auto_theme: "dark".into(),
            auto_known: true,
            truecolor: true,
            config_path: None,
            history_path: None,
            running: None,
            sort: SortKey::Default,
            search_history: Vec::new(),
            history_pos: None,
            queued_pick: None,
            repo_filter: None,
            sizes: Vec::new(),
            measured: HashMap::new(),
            next_tree: 0,
            last_query: String::new(),
            history_draft: String::new(),
            colors: crate::theme::BUILTIN
                .first()
                .map_or(crate::theme::fallback(), |b| b.colors),
            dry_run: cfg.dry_run,
            run_mode: cfg.run_mode,
            backend,
            cfg,
            sudo,
            tab: Tab::Search,
            panes: Panes::default(),
            query: String::new(),
            mode: Mode::Normal,
            explicit_only: false,
            explicit: None,
            explicit_loading: false,
            epoch: 0,
            popup_scroll: 0,
            popup_max: 0,
            held: HashSet::new(),
            queued: None,
            source_filter: None,
            details: HashMap::new(),
            sections: HashMap::new(),
            expanded: DEFAULT_EXPANDED.into_iter().collect(),
            detail_scroll: 0,
            detail_max: 0,
            log_scroll: 0,
            log_max: 0,
            log: VecDeque::new(),
            quit: false,
            run_request: None,
            job: None,
            hits: Hits::default(),
            sandbox: false,
            dragging: None,
            installed: HashSet::new(),
            pending_info: HashSet::new(),
            sel_changed: Instant::now(),
            tx,
            rx,
        };
        let unbound = app.cfg.keys.unbound();
        if !unbound.is_empty() {
            app.log(format!(
                "no key for {} (their default keys are bound to other actions in your config)",
                unbound.join(", ")
            ));
        }
        let line = backend_line(&app.backend, app.sudo.as_deref());
        app.log(line);
        app.load_held();
        app.load(Tab::Installed);
        app.load(Tab::Orphans);
        app.load(Tab::Updates);
        app
    }

    /// The colours of theme `name`, "auto" included.
    pub fn theme_colors(&self, name: &str) -> Option<Colors> {
        let name = if name == "auto" { self.auto_theme.as_str() } else { name };
        crate::theme::resolve(&self.cfg.theme, &self.cfg.themes, name, self.truecolor)
    }

    /// Switch to theme `name` (startup, or a preview in the picker).
    /// Unknown names are refused and leave the theme as it is.
    pub fn set_theme(&mut self, name: &str) -> bool {
        match self.theme_colors(name) {
            Some(colors) => {
                self.colors = colors;
                self.theme = name.to_string();
                true
            }
            None => false,
        }
    }

    /// Open the theme picker on the active theme.
    fn act_theme(&mut self) {
        // "auto" first: picking it again brings back NO_COLOR and the
        // terminal's background as the deciders.
        let auto_kind = crate::theme::builtin(&self.auto_theme).map_or(crate::theme::Kind::Dark, |b| b.kind);
        let themes: Vec<(String, crate::theme::Kind)> = std::iter::once(("auto".to_string(), auto_kind))
            .chain(crate::theme::all(&self.cfg.themes))
            .collect();
        let selected = themes.iter().position(|(n, _)| *n == self.theme).unwrap_or(0);
        self.popup_scroll = 0;
        self.mode = Mode::Themes(ThemePick {
            themes,
            selected,
            original: self.theme.clone(),
        });
    }

    /// Keys in the theme picker: moving previews, Enter keeps and saves,
    /// Esc (or quit) goes back to the theme it started with.
    fn themes_key(&mut self, mut pick: ThemePick, key: KeyEvent) {
        let last = pick.themes.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => pick.selected = pick.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => pick.selected = pick.selected.saturating_add(1).min(last),
            KeyCode::Home => pick.selected = 0,
            KeyCode::End => pick.selected = last,
            KeyCode::Enter => return self.keep_theme(&pick),
            KeyCode::Esc => return self.revert_theme(&pick),
            KeyCode::Char(c) if c == self.cfg.keys.quit => return self.revert_theme(&pick),
            _ => {}
        }
        self.preview_theme(&pick);
        self.mode = Mode::Themes(pick);
    }

    fn preview_theme(&mut self, pick: &ThemePick) {
        if let Some((name, _)) = pick.themes.get(pick.selected) {
            let name = name.clone();
            self.set_theme(&name);
        }
    }

    fn revert_theme(&mut self, pick: &ThemePick) {
        let original = pick.original.clone();
        self.set_theme(&original);
    }

    /// Keep the selected theme and write it to the config file, so it is
    /// used next time too.
    fn keep_theme(&mut self, pick: &ThemePick) {
        let Some((name, _)) = pick.themes.get(pick.selected) else {
            return;
        };
        let name = name.clone();
        self.set_theme(&name);
        self.cfg.theme.preset = name.clone();
        let saved = match (&self.config_path, self.sandbox) {
            (_, true) => Err("not saved in this mode".to_string()),
            (None, _) => Err("no config file location (HOME is not set)".to_string()),
            (Some(path), false) => crate::config::save_theme(path, &name)
                .map(|()| crate::config::tilde(path))
                .map_err(|e| format!("{e:#}")),
        };
        match saved {
            Ok(path) => self.log(format!("theme: {name} (saved to {path})")),
            Err(why) => self.log(format!("theme: {name} for this session; {why}")),
        }
    }

    /// Run `job` on a background thread (or inline). If the OS cannot start
    /// a thread, run it inline rather than failing.
    fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        if self.inline {
            return job();
        }
        let job = std::sync::Mutex::new(Some(job));
        let job = Arc::new(job);
        let for_thread = job.clone();
        let started = std::thread::Builder::new().spawn(move || {
            if let Some(f) = for_thread.lock().ok().and_then(|mut j| j.take()) {
                f();
            }
        });
        if started.is_err()
            && let Some(f) = job.lock().ok().and_then(|mut j| j.take())
        {
            f();
        }
    }

    pub fn log(&mut self, s: impl Into<String>) {
        for line in s.into().lines() {
            if self.log.len() >= LOG_CAP {
                self.log.pop_front();
            }
            self.log.push_back(line.to_string());
            // Keep the view still while the user is reading older lines.
            if self.log_scroll > 0 {
                self.log_scroll = self.log_scroll.saturating_add(1);
            }
        }
    }

    pub fn pane(&self) -> &Pane {
        self.panes.get(self.tab)
    }

    fn pane_mut(&mut self) -> &mut Pane {
        self.panes.get_mut(self.tab)
    }

    pub fn selected(&self) -> Option<&Package> {
        let p = self.pane();
        p.state.selected().and_then(|i| p.item(i))
    }

    pub fn is_installed(&self, p: &Package) -> bool {
        p.installed || matches!(self.tab, Tab::Installed | Tab::Orphans | Tab::Updates)
    }

    // ---------------------------------------------------------------- loading

    fn load(&mut self, tab: Tab) {
        let query = self.query.trim().to_string();
        let pane = self.panes.get_mut(tab);
        pane.generation = pane.generation.wrapping_add(1);
        if tab == Tab::Search && query.is_empty() {
            pane.items.clear();
            pane.loading = false;
            self.rebuild_view(tab);
            return;
        }
        pane.loading = true;
        let (generation, backend, tx) = (pane.generation, self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let result = match tab {
                Tab::Search => backend.search(&query),
                Tab::Installed => backend.installed(),
                Tab::Orphans => backend.orphans(),
                Tab::Updates => backend.updates(),
            };
            let _ = tx.send(Msg::List {
                tab,
                generation,
                result: result.map_err(LoadErr::from),
            });
        });
    }

    fn load_explicit(&mut self) {
        if self.explicit_loading {
            return;
        }
        self.explicit_loading = true;
        let (backend, tx, epoch) = (self.backend.clone(), self.tx.clone(), self.epoch);
        self.spawn(move || {
            let result = backend
                .explicit_covered()
                .map(|(names, covered)| ExplicitSet {
                    names: names.into_iter().collect(),
                    covered: covered.into_iter().collect(),
                })
                .map_err(LoadErr::from);
            let _ = tx.send(Msg::Explicit { epoch, result });
        });
    }

    fn load_held(&mut self) {
        let (backend, tx, epoch) = (self.backend.clone(), self.tx.clone(), self.epoch);
        self.spawn(move || {
            let result = backend.held_tagged().map_err(LoadErr::from);
            let _ = tx.send(Msg::Held { epoch, result });
        });
    }

    pub fn is_held(&self, p: &Package) -> bool {
        self.held.contains(&p.id())
    }

    fn reload_all(&mut self) {
        // Everything requested before this point is stale, including a
        // waiting command plan (it was made against the old state).
        if let Some(p) = self.queued.take_if(|p| matches!(p.action, PendingAction::Run)) {
            self.log(format!("{}: dropped, the package lists changed; run it again", p.title));
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.sizes.clear();
        self.measured.clear();
        self.details.clear();
        self.sections.clear();
        self.pending_info.clear();
        self.explicit_loading = false;
        // Keep the old explicit set until the new one arrives, so the
        // filtered view does not flash the full list.
        if self.explicit_only {
            self.load_explicit();
        } else {
            self.explicit = None;
        }
        self.load_held();
        for tab in Tab::ALL {
            self.load(tab);
        }
    }

    fn rebuild_view(&mut self, tab: Tab) {
        let explicit = (tab == Tab::Installed && self.explicit_only)
            .then_some(self.explicit.as_ref())
            .flatten();
        let source = self.source_filter.as_deref();
        let repo = (tab == Tab::Installed).then_some(self.repo_filter.as_ref()).flatten();
        let backend = self.backend.clone();
        let pane = self.panes.get_mut(tab);
        // Keep the cursor on the same package when it is still visible.
        let selected_key = pane.state.selected().and_then(|i| pane.item(i)).map(Package::key);
        let filter = pane.filter.trim().to_string();
        let filter_lower = filter.to_lowercase();
        // Names match fuzzily; summaries only as a substring (a loose
        // subsequence matches almost any long summary).
        let score = |p: &Package| -> Option<i64> {
            if filter.is_empty() {
                return Some(0);
            }
            let by_name = crate::fuzzy::fuzzy_match(&filter, &p.name).map(|m| m.score);
            let by_desc = p.description.to_lowercase().contains(&filter_lower).then_some(-40);
            by_name.or(by_desc)
        };
        let mut scored: Vec<(usize, i64)> = pane
            .items
            .iter()
            .enumerate()
            .filter(|(_, p)| explicit.is_none_or(|e| e.keeps(p)))
            .filter(|(_, p)| source.is_none_or(|s| p.source == s))
            .filter(|(_, p)| repo.is_none_or(|r| backend.repositories().is_some_and(|repos| repos.contains(r, p))))
            .filter_map(|(i, p)| score(p).map(|sc| (i, sc)))
            .collect();
        let items = &pane.items;
        let short = |i: usize| {
            items
                .get(i)
                .map(|p| p.name.rsplit('/').next().unwrap_or(&p.name).to_lowercase())
                .unwrap_or_default()
        };
        // Sort by size/date/source only where this list has that data.
        let sort = match self.sort {
            SortKey::Size if !items.iter().any(|p| p.size.is_some()) => SortKey::Default,
            SortKey::Date if !items.iter().any(|p| p.installed_at.is_some()) => SortKey::Default,
            SortKey::Source if !self.backend.is_multi() => SortKey::Default,
            other => other,
        };
        // Stable sorts: ties keep the backend's (or relevance) order.
        match sort {
            SortKey::Default if !filter.is_empty() => scored.sort_by_key(|x| std::cmp::Reverse(x.1)),
            SortKey::Default => {}
            SortKey::Name => scored.sort_by_cached_key(|(i, _)| short(*i)),
            // Biggest / newest first; unknown values last.
            SortKey::Size => scored.sort_by_key(|(i, _)| std::cmp::Reverse(items.get(*i).and_then(|p| p.size))),
            SortKey::Date => scored.sort_by_key(|(i, _)| std::cmp::Reverse(items.get(*i).and_then(|p| p.installed_at))),
            SortKey::Source => scored
                .sort_by_cached_key(|(i, _)| (items.get(*i).map(|p| p.source.clone()).unwrap_or_default(), short(*i))),
        }
        pane.view = scored.into_iter().map(|(i, _)| i).collect();
        pane.sort = sort;
        // Highlights: the filter, or on the Search tab the last search run.
        let pattern = if tab == Tab::Search {
            self.last_query.as_str()
        } else {
            filter.as_str()
        };
        pane.highlights = pane
            .view
            .iter()
            .map(|&i| {
                pane.items
                    .get(i)
                    .and_then(|p| crate::fuzzy::fuzzy_match(pattern, &p.name))
                    .map(|m| m.positions)
                    .unwrap_or_default()
            })
            .collect();
        let last = pane.view.len().checked_sub(1);
        let same = selected_key.and_then(|k| {
            pane.view
                .iter()
                .position(|&i| pane.items.get(i).is_some_and(|p| p.key() == k))
        });
        let sel = match (same, pane.state.selected(), last) {
            (_, _, None) => None,
            (Some(i), _, _) => Some(i),
            (None, Some(i), Some(last)) => Some(i.min(last)),
            (None, None, Some(_)) => Some(0),
        };
        pane.state.select(sel);
        // Keep the scroll position while it still fits the new rows (a
        // reload or re-sort should not make the list jump). Reset it when
        // the filter changed (the rows were re-ranked, best first) or when
        // it points past the end or below the selection (a stale offset
        // from a longer list would show only the tail). The list widget then
        // scrolls only as far as needed to show the selection.
        let offset = pane.state.offset();
        let refiltered = pane.last_filter != filter;
        pane.last_filter = filter.clone();
        if refiltered || offset >= pane.view.len() || sel.is_some_and(|s| offset > s) {
            *pane.state.offset_mut() = 0;
        }
        self.sel_changed = Instant::now();
    }

    fn flag_installed(&mut self) {
        let installed = &self.installed;
        for pane in self.panes.all_mut() {
            for p in &mut pane.items {
                p.installed = installed.contains(&p.id());
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::List {
                tab,
                generation,
                result,
            } => {
                if self.panes.get(tab).generation != generation {
                    return; // stale
                }
                self.panes.get_mut(tab).loading = false;
                self.panes.get_mut(tab).unsupported = None;
                match result {
                    Ok(mut items) => {
                        if tab == Tab::Search {
                            rank(&mut items, &self.query);
                            self.log(format!("search '{}': {} results", self.query.trim(), items.len()));
                        } else {
                            // Sort tap packages ("user/tap/name") among the others by short name.
                            items.sort_by_cached_key(|p| {
                                let short = p.name.rsplit('/').next().unwrap_or(&p.name).to_lowercase();
                                (short, p.name.clone())
                            });
                        }
                        if tab == Tab::Installed {
                            crate::backend::apply_sizes(&mut items, &self.measured);
                            self.installed = items.iter().map(Package::id).collect();
                        }
                        self.panes.get_mut(tab).items = items;
                        self.flag_installed();
                    }
                    Err(LoadErr::Unsupported(msg)) => {
                        let pane = self.panes.get_mut(tab);
                        pane.items.clear();
                        pane.unsupported = Some(msg);
                    }
                    Err(LoadErr::Failed(e)) => self.log(format!("error loading {}: {e}", tab.title())),
                }
                self.rebuild_view(tab);
            }
            Msg::Explicit { epoch, .. }
            | Msg::Held { epoch, .. }
            | Msg::Info { epoch, .. }
            | Msg::Section { epoch, .. }
                if epoch != self.epoch => {} // requested before a reload
            Msg::Explicit { result, .. } => {
                self.explicit_loading = false;
                match result {
                    Ok(set) => self.explicit = Some(set),
                    Err(LoadErr::Unsupported(m) | LoadErr::Failed(m)) => {
                        self.log(m);
                        self.explicit_only = false;
                        self.explicit = None;
                    }
                }
                self.rebuild_view(Tab::Installed);
            }
            Msg::Held { result, .. } => match result {
                Ok(v) => self.held = v.into_iter().collect(),
                Err(LoadErr::Unsupported(_)) => self.held.clear(),
                Err(LoadErr::Failed(e)) => self.log(format!("error loading held packages: {e}")),
            },
            Msg::Info { key, result, .. } => {
                self.pending_info.remove(&key);
                self.details
                    .insert(key, result.unwrap_or_else(|e| format!("error: {e}")));
            }
            Msg::Section {
                key, section, result, ..
            } => {
                let state = match result {
                    Ok(v) => SectionState::Ready(v),
                    Err(e) => SectionState::Error(e),
                };
                self.sections.insert((key, section), state);
            }
            Msg::Export(result) => self.export_ready(result),
            Msg::ImportPlan { path, result } => match result {
                Ok((cmds, notes)) => {
                    // The file may come from another machine: always show it.
                    let mut p = Pending::run(format!("Import from {}", path.display()), cmds, Draft::other());
                    p.notes = notes;
                    p.force_confirm = true;
                    self.confirm(p);
                }
                Err(e) => self.log(format!("error: import from {}: {e}", path.display())),
            },
            Msg::Plan { title, result, draft } => match result {
                Ok((cmds, notes)) => {
                    let mut p = Pending::run(title, cmds, draft);
                    p.notes = notes;
                    self.confirm(p);
                }
                Err(e) => self.log(e),
            },
            Msg::Versions { pkg, result } => match result {
                Ok((versions, _)) if versions.is_empty() => self.log(format!("{}: no versions found", pkg.name)),
                Ok((versions, can_hold)) => {
                    let selected = versions
                        .iter()
                        .position(|v| pkg.installed && v.version == pkg.version)
                        .unwrap_or(0);
                    let held_now = self.is_held(&pkg);
                    let pick = VersionPick {
                        pkg,
                        versions,
                        selected,
                        hold: can_hold,
                        can_hold,
                        held_now,
                    };
                    if matches!(self.mode, Mode::Normal) && self.job.is_none() {
                        self.popup_scroll = 0;
                        self.mode = Mode::Versions(pick);
                    } else {
                        self.log(format!(
                            "{}: versions ready; shown when the current prompt closes",
                            pick.pkg.name
                        ));
                        self.queued_pick = Some(pick);
                    }
                }
                Err(e) => self.log(e),
            },
            Msg::Repos(result) => match result {
                Ok(repos) if matches!(self.mode, Mode::Normal) && self.job.is_none() => {
                    let Some(r) = self.backend.repositories() else { return };
                    let items = &self.panes.installed.items;
                    let installed = repos
                        .iter()
                        .map(|repo| items.iter().filter(|p| r.contains(repo, p)).count())
                        .collect();
                    self.mode = Mode::Repos(RepoView {
                        kind: r.kind(),
                        repos,
                        installed,
                        selected: 0,
                        adding: None,
                        error: None,
                        add_hint: r.add_hint(),
                    });
                }
                Ok(_) => self.log("repositories ready; press the key again when the current prompt is closed"),
                Err(e) => self.log(format!("error: {e}")),
            },
            // From before a reload: the list it measured is gone, and the
            // reload may already have started a new measurement.
            Msg::Sizes { epoch, .. } if epoch != self.epoch => {}
            Msg::Sizes { source, result, .. } => {
                match result {
                    Ok(sizes) => {
                        // Kept, so a reload of the Installed list keeps them.
                        self.measured.extend(sizes);
                        crate::backend::apply_sizes(&mut self.panes.installed.items, &self.measured);
                        // Size sort and the size column see them now.
                        self.rebuild_view(Tab::Installed);
                    }
                    Err(e) => self.log(format!("error: measuring sizes for {source}: {e}")),
                }
                if let Some(entry) = self.sizes.iter_mut().find(|(src, _)| *src == source) {
                    entry.1 = Measurement::Done;
                }
                if let Mode::Disk(d) = &self.mode {
                    let keep = d.largest.get(d.selected).map(Package::key);
                    let measuring = d.measuring.iter().filter(|m| **m != source).cloned().collect();
                    let view = DiskView::build(
                        &self.panes.installed.items,
                        measuring,
                        d.postponed.clone(),
                        keep.as_deref(),
                    );
                    self.mode = Mode::Disk(view);
                }
            }
            Msg::TreeChildren { tree, node, result } => {
                if let Mode::Tree(t) = &mut self.mode
                    && t.id == tree
                {
                    t.set_children(node, result);
                }
            }
            Msg::Cleanup(result) => match result {
                Ok(cmds) => self.confirm(Pending::run(
                    "Clean up unused packages and cache".into(),
                    cmds,
                    Draft::other(),
                )),
                Err(e) => self.log(format!("error: cleanup: {e}")),
            },
        }
    }

    /// Process background results, advance the running job, and fetch
    /// details for the selection once it has been stable for a moment.
    pub fn tick(&mut self) {
        self.drain();
        if let Some(job) = self.job.as_mut() {
            job.poll();
            // Recorded as soon as the commands finish, not when the pane is
            // closed: quitting with the pane open must not lose the record.
            if let JobState::Finished { ok } = job.state
                && let Some((draft, title, cmds)) = self.running.take()
            {
                self.record(crate::history::Entry::new(
                    draft,
                    &title,
                    &cmds,
                    self.sudo.as_deref(),
                    ok,
                ));
            }
        }
        self.request_details();
        if matches!(self.mode, Mode::Normal)
            && self.job.is_none()
            && let Some(p) = self.queued.take()
        {
            self.confirm(p);
        }
        if matches!(self.mode, Mode::Normal)
            && self.job.is_none()
            && let Some(pick) = self.queued_pick.take()
        {
            self.popup_scroll = 0;
            self.mode = Mode::Versions(pick);
        }
        // Inline loads finished during this tick; apply them now so tests
        // and fuzzing see a settled state after a single tick.
        if self.inline {
            self.drain();
        }
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
        // Package managers that failed while others answered.
        for w in self.backend.take_warnings() {
            self.log(format!("error: {w}"));
        }
    }

    fn request_details(&mut self) {
        // Package managers lock their database while a command runs.
        if self.job.is_some() || (!self.inline && self.sel_changed.elapsed() < INFO_DELAY) {
            return;
        }
        let Some(pkg) = self.selected().cloned() else {
            return;
        };
        let key = pkg.key();
        if !self.details.contains_key(&key) && self.pending_info.insert(key.clone()) {
            let (backend, tx, pkg, key, epoch) = (
                self.backend.clone(),
                self.tx.clone(),
                pkg.clone(),
                key.clone(),
                self.epoch,
            );
            self.spawn(move || {
                let result = backend.info(&pkg).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Info { epoch, key, result });
            });
        }
        for section in Section::ALL {
            // Files can be long and slow: only load them once expanded.
            if section == Section::Files && !self.expanded.contains(&Section::Files) {
                continue;
            }
            let id = (key.clone(), section);
            if self.sections.contains_key(&id) {
                continue;
            }
            self.sections.insert(id, SectionState::Loading);
            let (backend, tx, pkg, key, epoch) = (
                self.backend.clone(),
                self.tx.clone(),
                pkg.clone(),
                key.clone(),
                self.epoch,
            );
            self.spawn(move || {
                let result = backend.section(&pkg, section).map_err(LoadErr::from);
                let _ = tx.send(Msg::Section {
                    epoch,
                    key,
                    section,
                    result,
                });
            });
        }
    }

    // ---------------------------------------------------------------- actions

    /// Marked packages that are visible in the current tab (filters hide
    /// marks rather than acting on them), or the selected one.
    fn targets(&self) -> Vec<Package> {
        let p = self.pane();
        let marked: Vec<Package> = p
            .view
            .iter()
            .filter_map(|&i| p.items.get(i))
            .filter(|x| p.marked.contains(&x.key()))
            .cloned()
            .collect();
        if marked.is_empty() {
            self.selected().cloned().into_iter().collect()
        } else {
            marked
        }
    }

    fn installed_targets(&mut self) -> Option<Vec<Package>> {
        let all = self.targets();
        if all.is_empty() {
            self.log("nothing selected");
            return None;
        }
        let t: Vec<Package> = all.into_iter().filter(|p| self.is_installed(p)).collect();
        if t.is_empty() {
            self.log("selected packages are not installed");
            return None;
        }
        Some(t)
    }

    fn names(pkgs: &[Package]) -> String {
        let n: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        match n.split_at_checked(6) {
            Some((first, rest)) if !rest.is_empty() => format!("{} and {} more", first.join(", "), rest.len()),
            _ => n.join(", "),
        }
    }

    fn act_install(&mut self) {
        let t = self.targets();
        if t.is_empty() {
            return self.log("nothing selected");
        }
        let cmds = self.backend.install(&t);
        // Only what was missing: undo must not remove what was there before.
        let fresh = crate::history::changed(Op::Install, &t, |p| self.is_installed(p), |p| self.is_held(p));
        self.confirm(Pending::run(
            format!("Install {}", Self::names(&t)),
            cmds,
            Draft::new(Op::Install, &fresh),
        ));
    }

    fn act_delete(&mut self) {
        if let Some(t) = self.installed_targets() {
            let cmds = self.backend.remove(&t);
            self.confirm(Pending::run(
                format!("Remove {}", Self::names(&t)),
                cmds,
                Draft::new(Op::Remove, &t),
            ));
        }
    }

    fn act_update(&mut self) {
        if let Some(t) = self.installed_targets() {
            let cmds = self.backend.upgrade(&t);
            let draft = Draft::new(Op::Upgrade, &self.with_new_versions(&t));
            self.confirm(Pending::run(format!("Update {}", Self::names(&t)), cmds, draft));
        }
    }

    fn act_update_all(&mut self) {
        let cmds = self.backend.upgrade(&[]);
        // What the Updates list shows is what this upgrades (and what undo
        // can take back).
        let draft = Draft::new(Op::Upgrade, &self.panes.updates.items);
        self.confirm(Pending::run("Update all packages".into(), cmds, draft));
    }

    /// Some cleanups query the package manager first (orphans, old
    /// revisions), so they are planned in the background.
    fn act_cleanup(&mut self) {
        self.log("planning cleanup...");
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let _ = tx.send(Msg::Cleanup(backend.cleanup().map_err(|e| format!("{e:#}"))));
        });
    }

    fn act_sync(&mut self) {
        let cmds = self.backend.sync();
        self.confirm(Pending::run("Sync repository metadata".into(), cmds, Draft::other()));
    }

    fn act_explicit(&mut self) {
        self.explicit_only = !self.explicit_only;
        if self.explicit_only && self.explicit.is_none() {
            self.load_explicit();
        }
        self.set_tab(Tab::Installed);
        self.rebuild_view(Tab::Installed);
    }

    /// Hold the targets, or release them if they are all held already.
    fn act_hold(&mut self) {
        let Some(t) = self.installed_targets() else { return };
        let on = !t.iter().all(|p| self.is_held(p));
        let verb = if on { "Hold" } else { "Release" };
        let title = format!("{verb} {}", Self::names(&t));
        // Only what changes: undo must not release a hold the user had set.
        let op = if on { Op::Hold } else { Op::Unhold };
        let changed = crate::history::changed(op, &t, |p| self.is_installed(p), |p| self.is_held(p));
        let draft = Draft::new(op, &changed);
        // Some backends query first (dnf versionlock, apk versions).
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let result = backend.hold_with_notes(&t, on).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Plan { title, result, draft });
        });
    }

    /// Cycle the list order through the keys this data supports.
    fn act_sort(&mut self) {
        let items = &self.pane().items;
        let has_size = items.iter().any(|p| p.size.is_some());
        let has_date = items.iter().any(|p| p.installed_at.is_some());
        let multi = self.backend.is_multi();
        let order = [
            SortKey::Default,
            SortKey::Name,
            SortKey::Size,
            SortKey::Date,
            SortKey::Source,
        ];
        let available = |k: &SortKey| match k {
            SortKey::Size => has_size,
            SortKey::Date => has_date,
            SortKey::Source => multi,
            _ => true,
        };
        let pos = order.iter().position(|k| *k == self.sort).unwrap_or(0);
        self.sort = order
            .iter()
            .cycle()
            .skip(pos.saturating_add(1))
            .take(order.len())
            .find(|k| available(k))
            .copied()
            .unwrap_or(SortKey::Default);
        self.log(format!("sorted by {}", self.sort.label()));
        for tab in Tab::ALL {
            self.rebuild_view(tab);
        }
    }

    /// Load the versions of the selected package for the version picker.
    /// On the Search tab a package's version is the repository's, so the
    /// installed version is looked up first.
    fn act_versions(&mut self) {
        let Some(mut pkg) = self.selected().cloned() else {
            return self.log("nothing selected");
        };
        if self.tab == Tab::Search {
            // Search results carry the repository version: take the
            // installed one from the Installed list, which must be loaded.
            if self.panes.installed.loading || self.panes.installed.items.is_empty() && pkg.installed {
                return self.log("the installed list is still loading; try again in a moment");
            }
            let installed = self.panes.installed.items.iter().find(|p| p.id() == pkg.id());
            pkg.installed = installed.is_some();
            pkg.version = installed.map(|p| p.version.clone()).unwrap_or_default();
        } else {
            // Installed, Orphans, Updates: the row is the installed package.
            pkg.installed = true;
        }
        self.log(format!("{}: loading versions...", pkg.name));
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let result = backend
                .versions(&pkg)
                .map(|v| {
                    // Can this manager hold at all? (pacman, nix cannot.)
                    let probe = backend.hold(std::slice::from_ref(&pkg), true);
                    let can_hold = !matches!(&probe, Err(e) if is_unsupported(e));
                    (v, can_hold)
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Versions { pkg, result });
        });
    }

    /// Plan installing the picked version (and holding it) in the background.
    fn pick_version(&mut self, pick: VersionPick) {
        let Some(choice) = pick.versions.get(pick.selected).cloned() else {
            return;
        };
        let hold = pick.hold && pick.can_hold;
        let title = format!(
            "Install {} {}{}",
            pick.pkg.name,
            choice.label(),
            if hold { " and hold it" } else { "" }
        );
        // Recorded like an upgrade: undo goes back to the installed version.
        let mut target = pick.pkg.clone();
        target.new_version = Some(choice.version.clone());
        if !pick.pkg.installed {
            target.version.clear();
        }
        let draft = Draft::new(Op::Upgrade, &[target]);
        let (backend, tx, pkg, held_now) = (self.backend.clone(), self.tx.clone(), pick.pkg, pick.held_now);
        self.spawn(move || {
            let result = crate::backend::version_plan(backend.as_ref(), &pkg, &choice, held_now, hold)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Plan { title, result, draft });
        });
    }

    /// Open the repositories popup (Homebrew taps).
    fn act_repos(&mut self) {
        if self.backend.repositories().is_none() {
            return self.log("no active package manager has repositories pakdeck can manage");
        }
        self.log("loading repositories...");
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let result = match backend.repositories() {
                Some(r) => r.list().map_err(|e| format!("{e:#}")),
                None => Err("no active package manager has repositories".into()),
            };
            let _ = tx.send(Msg::Repos(result));
        });
    }

    /// Keys in the repositories popup.
    fn repos_key(&mut self, mut view: RepoView, key: KeyEvent) {
        if let Some(mut text) = view.adding.take() {
            match key.code {
                KeyCode::Enter => {
                    let result = match self.backend.repositories() {
                        Some(r) => r.add(view.add_target(), text.trim()),
                        None => crate::backend::unsupported("no active package manager has repositories"),
                    };
                    match result {
                        Ok(cmds) => {
                            return self.confirm(Pending::run(
                                format!("Add {} {}", view.noun(), text.trim()),
                                cmds,
                                Draft::other(),
                            ));
                        }
                        // Stay in the input line with the text, and say why.
                        Err(e) => {
                            view.error = Some(format!("{e:#}"));
                            view.adding = Some(text);
                        }
                    }
                }
                KeyCode::Esc => view.error = None,
                KeyCode::Backspace => {
                    text.pop();
                    view.adding = Some(text);
                    view.error = None;
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    view.adding = Some(text);
                    view.error = None;
                }
                _ => view.adding = Some(text),
            }
            self.mode = Mode::Repos(view);
            return;
        }
        let last = view.repos.len().saturating_sub(1);
        let keys = self.cfg.keys;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                view.selected = view.selected.saturating_sub(1);
                view.error = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.selected = view.selected.saturating_add(1).min(last);
                view.error = None;
            }
            KeyCode::Char(c) if c == keys.add => {
                view.adding = Some(String::new());
                view.error = None;
            }
            KeyCode::Char(c) if c == keys.delete => {
                let result = match (view.repos.get(view.selected), self.backend.repositories()) {
                    (Some(repo), Some(r)) => r.remove(repo).map(|cmds| (repo.name.clone(), cmds)),
                    _ => Err(anyhow::anyhow!("no {} to remove", view.noun())),
                };
                match result {
                    Ok((name, cmds)) => {
                        return self.confirm(Pending::run(
                            format!("Remove {} {name}", view.noun()),
                            cmds,
                            Draft::other(),
                        ));
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
            }
            // Enter or the source-filter key: show its installed packages.
            KeyCode::Char(c) if c == keys.source => return self.filter_by_repo(&view),
            KeyCode::Enter => return self.filter_by_repo(&view),
            KeyCode::Esc => return,
            KeyCode::Char(c) if c == self.cfg.keys.quit => return,
            _ => {}
        }
        self.mode = Mode::Repos(view);
    }

    /// Close the popup and show only the Installed packages from the
    /// selected repository.
    fn filter_by_repo(&mut self, view: &RepoView) {
        if let Some(r) = view.repos.get(view.selected) {
            self.repo_filter = Some(r.clone());
            self.log(format!("showing installed packages from {} (Esc clears)", r.name));
            self.set_tab(Tab::Installed);
            self.rebuild_view(Tab::Installed);
        }
    }

    /// Open the dependency tree of the selected package.
    fn act_tree(&mut self) {
        let Some(pkg) = self.selected().cloned() else {
            return self.log("nothing selected");
        };
        self.open_tree(pkg, Section::Depends);
    }

    fn open_tree(&mut self, pkg: Package, direction: Section) {
        self.next_tree = self.next_tree.wrapping_add(1);
        let tree = DepTree::new(self.next_tree, pkg, direction);
        self.mode = Mode::Tree(tree);
        self.load_tree_children(0);
    }

    /// Load the children of `node` (if not loaded yet) in the background.
    fn load_tree_children(&mut self, node: usize) {
        let Mode::Tree(t) = &mut self.mode else { return };
        let Some(n) = t.nodes.get_mut(node) else { return };
        if n.children.is_some() || n.loading || n.cycle {
            return;
        }
        n.loading = true;
        let mut pkg = Package::new(n.name.as_str(), "").source(t.root.source.as_str());
        if node == 0 {
            pkg = t.root.clone();
        } else {
            pkg.installed = self.installed.contains(&pkg.id());
        }
        let (direction, tree) = (t.direction, t.id);
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let result = backend.section(&pkg, direction).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::TreeChildren { tree, node, result });
        });
    }

    /// Keys in the dependency tree popup.
    fn tree_key(&mut self, mut t: DepTree, key: KeyEvent) {
        let row = t.selected_row();
        let current = Some(t.selected);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => t.select_row(row.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => t.select_row(row.saturating_add(1)),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(i) = current
                    && let Some(n) = t.nodes.get_mut(i)
                    && !n.cycle
                {
                    n.expanded = !n.expanded || key.code != KeyCode::Enter;
                    let load = n.expanded;
                    self.mode = Mode::Tree(t);
                    if load {
                        self.load_tree_children(i);
                    }
                    return;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(i) = current {
                    let expanded = t.nodes.get(i).is_some_and(|n| n.expanded);
                    if expanded {
                        if let Some(n) = t.nodes.get_mut(i) {
                            n.expanded = false;
                        }
                    } else if let Some(parent) = t.nodes.get(i).and_then(|n| n.parent) {
                        t.selected = parent;
                    }
                }
            }
            // The tree key again: the other direction.
            KeyCode::Char(c) if c == self.cfg.keys.tree => {
                let direction = if t.direction == Section::Depends {
                    Section::RequiredBy
                } else {
                    Section::Depends
                };
                return self.open_tree(t.root, direction);
            }
            KeyCode::Char('g') => {
                if let Some(name) = current.and_then(|i| t.nodes.get(i)).map(|n| n.name.clone()) {
                    self.go_to_package(&t.root.source, &name);
                }
                return;
            }
            KeyCode::Esc => return,
            KeyCode::Char(c) if c == self.cfg.keys.quit => return,
            _ => {}
        }
        self.mode = Mode::Tree(t);
    }

    /// Add a finished run to the history file (never in sandbox mode or
    /// when `history = false`); a failure to write is only logged.
    fn record(&mut self, entry: crate::history::Entry) {
        if self.sandbox || !self.cfg.history {
            return;
        }
        let Some(path) = self.history_path.clone() else { return };
        if let Err(e) = crate::history::append(&path, &entry) {
            self.log(format!("error: history not saved: {e:#}"));
        }
    }

    /// Targets with the version the Updates list offers for them, so an
    /// update from the Installed tab records where it went.
    fn with_new_versions(&self, targets: &[Package]) -> Vec<Package> {
        targets
            .iter()
            .map(|p| {
                let mut p = p.clone();
                if p.new_version.is_none() {
                    p.new_version = self
                        .panes
                        .updates
                        .items
                        .iter()
                        .find(|u| u.id() == p.id())
                        .and_then(|u| u.new_version.clone());
                }
                p
            })
            .collect()
    }

    /// Open the history, newest first.
    fn act_history(&mut self) {
        let Some(path) = self.history_path.clone() else {
            return self.log("no history file location (HOME is not set)");
        };
        match crate::history::load(&path) {
            Ok(mut entries) if !entries.is_empty() => {
                entries.reverse();
                let why_not = entries
                    .iter()
                    .map(|e| crate::history::why_not(&self.backend, e, &entries))
                    .collect();
                self.popup_scroll = 0;
                self.mode = Mode::History(HistoryView {
                    entries,
                    why_not,
                    selected: 0,
                    error: None,
                });
            }
            Ok(_) if !self.cfg.history => self.log("history is turned off (history = false in the config)"),
            Ok(_) => self.log("no history yet: installs, removals, updates and holds are recorded here"),
            Err(e) => self.log(format!("error: {e:#}")),
        }
    }

    /// Keys in the history: Enter plans undoing the selected entry (the
    /// confirmation shows the commands first).
    fn history_key(&mut self, mut view: HistoryView, key: KeyEvent) {
        let last = view.entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => view.selected = view.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.selected = view.selected.saturating_add(1).min(last),
            KeyCode::Home => view.selected = 0,
            KeyCode::End => view.selected = last,
            KeyCode::Enter => {
                let Some(entry) = view.entries.get(view.selected) else {
                    return;
                };
                match crate::history::undo_plan(&self.backend, entry, &view.entries) {
                    Ok((title, cmds, notes)) => {
                        let mut p = Pending::run(title, cmds, crate::history::undo_draft(entry));
                        p.notes = notes;
                        p.force_confirm = true;
                        return self.confirm(p);
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
            }
            KeyCode::Esc => return,
            KeyCode::Char(c) if c == self.cfg.keys.quit => return,
            _ => {}
        }
        if !matches!(key.code, KeyCode::Enter) {
            view.error = None;
        }
        self.mode = Mode::History(view);
    }

    /// Show a package in the Installed list, or search for it.
    fn go_to_package(&mut self, source: &str, name: &str) {
        // The given source first, then any: a tree rooted at an AUR package
        // lists repository dependencies installed under pacman.
        let matches = |p: &Package, any: bool| p.name == name && (any || source.is_empty() || p.source == source);
        let items = &self.panes.installed.items;
        let any = !items.iter().any(|p| matches(p, false));
        if !items.iter().any(|p| matches(p, any)) {
            self.set_tab(Tab::Search);
            self.query = name.to_string();
            self.last_query = name.to_string();
            self.panes.search.state.select(Some(0));
            self.load(Tab::Search);
            return;
        }
        self.set_tab(Tab::Installed);
        let row_of = |pane: &Pane| {
            pane.view
                .iter()
                .position(|&i| pane.items.get(i).is_some_and(|p| matches(p, any)))
        };
        if row_of(&self.panes.installed).is_none() {
            // Hidden: clear only what hides it, and say so.
            let mut cleared = Vec::new();
            if !self.panes.installed.filter.is_empty() {
                self.panes.installed.filter.clear();
                cleared.push("the filter");
            }
            if self.source_filter.take().is_some() {
                cleared.push("the source filter");
            }
            if self.repo_filter.take().is_some() {
                cleared.push("the repository filter");
            }
            if std::mem::take(&mut self.explicit_only) {
                cleared.push("explicit only");
            }
            self.rebuild_view(Tab::Installed);
            if !cleared.is_empty() {
                self.log(format!("cleared {} to show {name}", cleared.join(", ")));
            }
        }
        let row = row_of(&self.panes.installed);
        self.panes.installed.state.select(row);
        self.sel_changed = Instant::now();
    }

    /// Disk usage from the sizes already loaded with the Installed list.
    fn act_disk(&mut self) {
        if self.panes.installed.items.is_empty() {
            return self.log("the installed list is not loaded yet");
        }
        let (totals, _) = crate::backend::disk_summary(&self.panes.installed.items, 0);
        let measurable: Vec<String> = totals
            .iter()
            .filter(|t| t.incomplete())
            .filter(|t| self.backend.size_meter().is_some_and(|m| m.measures(&t.source)))
            .map(|t| t.source.clone())
            .collect();
        // Managers without sizes in their list (Homebrew) measure them in
        // the background, once per reload. Not while a reload is replacing
        // the list: it would measure packages that are about to go away.
        let reloading = self.panes.installed.loading;
        let mut measuring = Vec::new();
        let mut postponed = Vec::new();
        for source in measurable {
            match self.sizes.iter().find(|(src, _)| *src == source) {
                Some((_, Measurement::Done)) => continue,
                Some((_, Measurement::Running)) => {}
                None if reloading => {
                    postponed.push(source);
                    continue;
                }
                None => {
                    self.sizes.push((source.clone(), Measurement::Running));
                    let pkgs: Vec<Package> = self
                        .panes
                        .installed
                        .items
                        .iter()
                        .filter(|p| p.source == source && p.size.is_none())
                        .cloned()
                        .collect();
                    let (backend, tx, epoch, src) = (self.backend.clone(), self.tx.clone(), self.epoch, source.clone());
                    self.spawn(move || {
                        let result = match backend.size_meter() {
                            Some(m) => m.measure(&pkgs).map_err(|e| format!("{e:#}")),
                            None => Err("cannot measure package sizes".into()),
                        };
                        let _ = tx.send(Msg::Sizes {
                            epoch,
                            source: src,
                            result,
                        });
                    });
                }
            }
            measuring.push(source);
        }
        self.popup_scroll = 0;
        self.mode = Mode::Disk(DiskView::build(&self.panes.installed.items, measuring, postponed, None));
    }

    fn disk_key(&mut self, mut d: DiskView, key: KeyEvent) {
        let last = d.largest.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => d.selected = d.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => d.selected = d.selected.saturating_add(1).min(last),
            KeyCode::Enter | KeyCode::Char('g') => {
                if let Some(p) = d.largest.get(d.selected) {
                    let (source, name) = (p.source.clone(), p.name.clone());
                    self.go_to_package(&source, &name);
                }
                return;
            }
            KeyCode::Esc => return,
            KeyCode::Char(c) if c == self.cfg.keys.quit => return,
            _ => {}
        }
        self.mode = Mode::Disk(d);
    }

    /// Cycle the source filter: all, then each active package manager.
    fn act_source(&mut self) {
        let names = self.backend.names();
        if names.len() < 2 {
            return self.log("only one package manager is active (see `backends` in the config)");
        }
        let next = match &self.source_filter {
            None => names.first(),
            Some(cur) => names.iter().skip_while(|n| *n != cur).nth(1),
        };
        self.source_filter = next.map(|n| n.to_string());
        let shown = self.source_filter.clone().unwrap_or_else(|| "all".into());
        self.log(format!("showing packages from: {shown}"));
        for tab in Tab::ALL {
            self.rebuild_view(tab);
        }
    }

    /// Build the export in the background; writing it is confirmed first.
    fn act_export(&mut self) {
        self.log("exporting...");
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let _ = tx.send(Msg::Export(
                crate::export::export(&backend).map_err(|e| format!("{e:#}")),
            ));
        });
    }

    fn export_ready(&mut self, result: Result<(String, Vec<String>), String>) {
        let path = self.cfg.export_file();
        let (text, notes) = match result {
            Ok(v) => v,
            Err(e) => return self.log(format!("error: export: {e}")),
        };
        if self.dry_run || self.sandbox {
            notes.into_iter().for_each(|n| self.log(n));
            return self.log(format!("[dry-run] package list not written to {}", path.display()));
        }
        let replaces = if path.exists() {
            " (replaces the existing file)"
        } else {
            ""
        };

        self.confirm(Pending {
            title: format!("Write package list to {}{replaces}", path.display()),
            cmds: Vec::new(),
            notes,
            action: PendingAction::WriteExport { path, text },
            force_confirm: true,
            record: None,
        });
    }

    /// Read the export file and plan the import in the background.
    fn act_import(&mut self) {
        let path = self.cfg.export_file();
        let sandbox = self.sandbox;
        self.log(format!("planning import from {}...", path.display()));
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        self.spawn(move || {
            let text = if sandbox {
                // Tests and fuzzing: import this machine's own export, no file.
                crate::export::export(&backend).map(|(t, _)| t)
            } else {
                std::fs::read_to_string(&path).map_err(anyhow::Error::from)
            };
            let result = text
                .and_then(|t| crate::export::parse(&t))
                .map(|list| crate::export::plan(&backend, &list))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::ImportPlan { path, result });
        });
    }

    /// Expand every section, or return to the default layout.
    fn act_expand(&mut self) {
        if self.expanded.len() == Section::ALL.len().saturating_sub(1) {
            self.expanded = DEFAULT_EXPANDED.into_iter().collect();
        } else {
            self.expanded = Section::ALL.into_iter().filter(|s| *s != Section::Size).collect();
        }
    }

    pub fn toggle_section(&mut self, s: Section) {
        if !self.expanded.remove(&s) {
            self.expanded.insert(s);
        }
    }

    fn confirm(&mut self, p: Pending) {
        if p.cmds.is_empty() && matches!(p.action, PendingAction::Run) {
            p.notes.iter().for_each(|n| self.log(n.clone()));
            return self.log(format!("{}: nothing to do", p.title));
        }
        // A result arriving while the user types, reads a popup or watches
        // a command must not take over the screen: it waits its turn.
        if !matches!(self.mode, Mode::Normal) || self.job.is_some() {
            if let Some(old) = self.queued.take() {
                self.log(format!("{}: replaced by a newer request", old.title));
            }
            self.log(format!("{}: ready; shown when the current prompt closes", p.title));
            // Whatever waited is always shown before it runs.
            let mut p = p;
            p.force_confirm = true;
            self.queued = Some(p);
            return;
        }
        if self.cfg.confirm || self.dry_run || p.force_confirm {
            self.popup_scroll = 0;
            self.mode = Mode::Confirm(p);
        } else {
            self.execute(p);
        }
    }

    fn execute(&mut self, p: Pending) {
        let Pending {
            title,
            cmds,
            notes,
            action,
            record,
            ..
        } = p;
        // Recorded when the run finishes (`after_run`); dry runs never.
        if matches!(action, PendingAction::Run) && !self.dry_run {
            self.running = record.map(|d| (d, title.clone(), cmds.clone()));
        }
        notes.into_iter().for_each(|n| self.log(n));
        match action {
            // Dry-run may have been switched on while the popup was open.
            PendingAction::WriteExport { path, .. } if self.dry_run => {
                self.log(format!("[dry-run] package list not written to {}", path.display()))
            }
            PendingAction::WriteExport { path, text } => match crate::util::write_atomic(&path, &text) {
                Ok(()) => self.log(format!("exported to {}", path.display())),
                Err(e) => self.log(format!("error: {e:#}")),
            },
            PendingAction::Run if self.dry_run => {
                self.log(format!("[dry-run] {title}"));
                for c in &cmds {
                    let line = format!("[dry-run] $ {}", c.display(self.sudo.as_deref()));
                    self.log(line);
                }
            }
            PendingAction::Run if self.run_mode == RunMode::Embedded && !self.sandbox => self.start_job(title, cmds),
            PendingAction::Run => {
                self.log(title);
                self.run_request = Some(cmds);
            }
        }
    }

    /// Run commands in the embedded terminal pane.
    pub fn start_job(&mut self, title: String, cmds: Vec<Cmd>) {
        self.log(title.clone());
        // Size of the last drawn pane, or a sensible default before the first draw.
        let r = self.hits.job_term;
        let (rows, cols) = if r.area() > 0 { (r.height, r.width) } else { (24, 100) };
        self.job = Some(Job::start(title, cmds, self.sudo.clone(), rows, cols));
    }

    /// Close a finished job pane and refresh everything.
    fn close_job(&mut self) {
        if let Some(job) = self.job.take() {
            let ok = matches!(job.state, JobState::Finished { ok: true });
            self.after_run(ok);
        }
    }

    /// Called after commands finished (embedded or suspended).
    pub fn after_run(&mut self, ok: bool) {
        self.log(if ok { "done" } else { "command failed" });
        if let Some((draft, title, cmds)) = self.running.take() {
            self.record(crate::history::Entry::new(
                draft,
                &title,
                &cmds,
                self.sudo.as_deref(),
                ok,
            ));
        }
        if ok {
            for p in self.panes.all_mut() {
                p.marked.clear();
            }
        }
        self.reload_all();
    }

    // ------------------------------------------------------------- navigation

    fn move_sel(&mut self, delta: isize) {
        let pane = self.pane_mut();
        let Some(max) = pane.view.len().checked_sub(1) else {
            return;
        };
        let cur = pane.state.selected().unwrap_or(0);
        let next = cur.saturating_add_signed(delta).min(max);
        pane.state.select(Some(next));
        self.sel_changed = Instant::now();
        self.detail_scroll = 0;
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.sel_changed = Instant::now();
        self.detail_scroll = 0;
    }

    fn toggle_mark(&mut self) {
        let Some(key) = self.selected().map(|p| p.key()) else {
            return;
        };
        let pane = self.pane_mut();
        if !pane.marked.remove(&key) {
            pane.marked.insert(key);
        }
        self.move_sel(1);
    }

    // ------------------------------------------------------------------ input

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let quit = self.cfg.keys.quit;
        if let Some(job) = self.job.as_mut() {
            let page = isize::try_from(self.hits.job_term.height.max(1)).unwrap_or(10);
            match (job.state, key.code) {
                // Ctrl-\ interrupts the command; a second press kills it.
                (JobState::Running, KeyCode::Char('\\')) if ctrl => job.stop(),
                // Shift+PgUp/PgDn scroll while running; other keys go to it.
                (JobState::Running, KeyCode::PageUp) if shift => job.scroll(page),
                (JobState::Running, KeyCode::PageDown) if shift => job.scroll(page.saturating_neg()),
                (JobState::Running, _) => job.send_key(key),
                (JobState::Finished { .. }, KeyCode::Enter | KeyCode::Esc) => self.close_job(),
                (JobState::Finished { .. }, KeyCode::Char(c)) if c == quit => self.close_job(),
                (JobState::Finished { .. }, KeyCode::PageUp) => job.scroll(page),
                (JobState::Finished { .. }, KeyCode::PageDown) => job.scroll(page.saturating_neg()),
                (JobState::Finished { .. }, KeyCode::Up | KeyCode::Char('k')) => job.scroll(1),
                (JobState::Finished { .. }, KeyCode::Down | KeyCode::Char('j')) => job.scroll(-1),
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('c') && ctrl {
            self.quit = true;
            return;
        }
        let scroll = match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(-1),
            KeyCode::Down | KeyCode::Char('j') => Some(1),
            KeyCode::PageUp => Some(-10),
            KeyCode::PageDown => Some(10),
            _ => None,
        };
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            // Scroll keys scroll the help; any other key closes it.
            Mode::Help => {
                if let Some(d) = scroll {
                    self.scroll_popup(d);
                    self.mode = Mode::Help;
                }
            }
            Mode::Confirm(p) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.execute(p),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.log("cancelled"),
                KeyCode::Char(c) if c == self.cfg.keys.quit => self.log("cancelled"),
                _ => {
                    if let Some(d) = scroll {
                        self.scroll_popup(d);
                    }
                    self.mode = Mode::Confirm(p);
                }
            },
            Mode::Repos(view) => self.repos_key(view, key),
            Mode::Tree(t) => self.tree_key(t, key),
            Mode::Disk(d) => self.disk_key(d, key),
            Mode::Themes(pick) => self.themes_key(pick, key),
            Mode::History(view) => self.history_key(view, key),
            Mode::Versions(mut pick) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    pick.selected = pick.selected.saturating_sub(1);
                    self.mode = Mode::Versions(pick);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    pick.selected = pick
                        .selected
                        .saturating_add(1)
                        .min(pick.versions.len().saturating_sub(1));
                    self.mode = Mode::Versions(pick);
                }
                // Space, h or the hold key toggle "hold afterwards".
                KeyCode::Char(c) if pick.can_hold && (c == ' ' || c == 'h' || c == self.cfg.keys.hold) => {
                    pick.hold = !pick.hold;
                    self.mode = Mode::Versions(pick);
                }
                KeyCode::Enter => self.pick_version(pick),
                KeyCode::Esc => self.log("cancelled"),
                KeyCode::Char(c) if c == self.cfg.keys.quit => self.log("cancelled"),
                _ => self.mode = Mode::Versions(pick),
            },
            Mode::Input => self.on_input_key(key),
            Mode::Normal => self.on_normal_key(key),
        }
    }

    fn scroll_popup(&mut self, delta: i16) {
        let next = if delta < 0 {
            self.popup_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.popup_scroll.saturating_add(delta.unsigned_abs())
        };
        self.popup_scroll = next.min(self.popup_max);
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        self.mode = Mode::Input;
        let search = self.tab == Tab::Search;
        let text = if search {
            &mut self.query
        } else {
            &mut self.pane_mut().filter
        };
        match key.code {
            // Search history (this session): Up/Down step through past searches.
            KeyCode::Up | KeyCode::Down if search => {
                if self.history_pos.is_none() {
                    self.history_draft = self.query.clone();
                }
                let n = self.search_history.len();
                let pos = match (key.code, self.history_pos) {
                    (KeyCode::Up, None) => n.checked_sub(1),
                    (KeyCode::Up, Some(p)) => Some(p.saturating_sub(1)),
                    (_, Some(p)) if p.saturating_add(1) < n => Some(p.saturating_add(1)),
                    _ => None,
                };
                self.history_pos = pos;
                // Past the newest entry: back to what was being typed.
                self.query = pos
                    .and_then(|p| self.search_history.get(p).cloned())
                    .unwrap_or_else(|| self.history_draft.clone());
                return;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if search {
                    if self.query.trim_start().starts_with('-') {
                        return self.log("search terms may not start with '-'");
                    }
                    let q = self.query.trim().to_string();
                    if !q.is_empty() && self.search_history.last() != Some(&q) {
                        self.search_history.push(q);
                        if self.search_history.len() > 100 {
                            self.search_history.remove(0);
                        }
                    }
                    self.history_pos = None;
                    self.last_query = self.query.trim().to_string();
                    self.panes.search.marked.clear();
                    self.panes.search.state.select(Some(0));
                    self.load(Tab::Search);
                }
                return;
            }
            KeyCode::Esc | KeyCode::Down | KeyCode::Tab => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Char(c) => text.push(c),
            _ => {}
        }
        if !search {
            self.rebuild_view(self.tab);
        }
    }

    fn key_action(&self, c: char) -> Option<Action> {
        let k = self.cfg.keys;
        Some(match c {
            _ if c == k.quit => Action::Quit,
            _ if c == k.search => Action::Search,
            _ if c == k.install => Action::Install,
            _ if c == k.delete => Action::Delete,
            _ if c == k.update => Action::Update,
            _ if c == k.update_all => Action::UpdateAll,
            _ if c == k.cleanup => Action::Cleanup,
            _ if c == k.sync => Action::Sync,
            _ if c == k.mark => Action::Mark,
            _ if c == k.help => Action::Help,
            _ if c == k.reload => Action::Reload,
            _ if c == k.dry_run => Action::DryRun,
            _ if c == k.explicit => Action::Explicit,
            _ if c == k.expand => Action::Expand,
            _ if c == k.run_mode => Action::RunMode,
            _ if c == k.hold => Action::Hold,
            _ if c == k.source => Action::Source,
            _ if c == k.export => Action::Export,
            _ if c == k.import => Action::Import,
            _ if c == k.sort => Action::Sort,
            _ if c == k.versions => Action::Versions,
            _ if c == k.theme => Action::Theme,
            _ if c == k.tree => Action::Tree,
            _ if c == k.disk => Action::Disk,
            _ if c == k.repos => Action::Repos,
            _ if c == k.history => Action::History,
            _ if c == k.clear_log => Action::ClearLog,
            _ => return None,
        })
    }

    pub fn perform(&mut self, action: Action) {
        match action {
            Action::Quit => self.quit = true,
            Action::Search => self.mode = Mode::Input,
            Action::Install => self.act_install(),
            Action::Delete => self.act_delete(),
            Action::Update => self.act_update(),
            Action::UpdateAll => self.act_update_all(),
            Action::Cleanup => self.act_cleanup(),
            Action::Sync => self.act_sync(),
            Action::Mark => self.toggle_mark(),
            Action::Help => {
                self.popup_scroll = 0;
                self.mode = Mode::Help;
            }
            Action::Explicit => self.act_explicit(),
            Action::Expand => self.act_expand(),
            Action::Reload => {
                self.details.clear();
                self.sections.clear();
                self.load(self.tab);
            }
            Action::DryRun => {
                self.dry_run = !self.dry_run;
                let s = if self.dry_run {
                    "dry-run enabled"
                } else {
                    "dry-run disabled"
                };
                self.log(s);
            }
            Action::Hold => self.act_hold(),
            Action::Source => self.act_source(),
            Action::Export => self.act_export(),
            Action::Import => self.act_import(),
            Action::Sort => self.act_sort(),
            Action::Versions => self.act_versions(),
            Action::Tree => self.act_tree(),
            Action::Disk => self.act_disk(),
            Action::Repos => self.act_repos(),
            Action::History => self.act_history(),
            Action::ClearLog => {
                self.log.clear();
                self.log_scroll = 0;
            }
            Action::Theme => self.act_theme(),
            Action::RunMode => {
                self.run_mode = match self.run_mode {
                    RunMode::Embedded => RunMode::Suspend,
                    RunMode::Suspend => RunMode::Embedded,
                };
                let s = match self.run_mode {
                    RunMode::Embedded => "commands run in the embedded terminal",
                    RunMode::Suspend => "commands run with the TUI suspended",
                };
                self.log(s);
            }
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        if let KeyCode::Char(c) = key.code
            && c != crate::config::UNBOUND
            && let Some(action) = self.key_action(c)
        {
            return self.perform(action);
        }
        let page = 10;
        match key.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.set_tab(self.tab.step(1)),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.set_tab(self.tab.step(-1)),
            KeyCode::Char(c @ '1'..='4') => {
                let i = c.to_digit(10).and_then(|d| d.checked_sub(1)).unwrap_or(0) as usize;
                self.set_tab(Tab::ALL.get(i).copied().unwrap_or(Tab::Search));
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::PageUp => self.move_sel(page.saturating_neg()),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX / 2),
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::Esc => {
                let tab = self.tab;
                let pane = self.pane_mut();
                if !pane.marked.is_empty() {
                    pane.marked.clear();
                } else if !pane.filter.is_empty() {
                    pane.filter.clear();
                    self.rebuild_view(tab);
                } else if tab == Tab::Installed && self.repo_filter.take().is_some() {
                    self.rebuild_view(tab);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ mouse

    /// Index into the current view for a screen row inside the list, if any.
    fn list_row_at(&self, pos: Position) -> Option<usize> {
        let area = self.hits.list;
        if !area.contains(pos) {
            return None;
        }
        let pane = self.pane();
        let row = pane
            .state
            .offset()
            .saturating_add(usize::from(pos.y.saturating_sub(area.y)));
        (row < pane.view.len()).then_some(row)
    }

    fn select_row(&mut self, row: usize) {
        if self.pane().state.selected() != Some(row) {
            self.pane_mut().state.select(Some(row));
            self.sel_changed = Instant::now();
            self.detail_scroll = 0;
        }
    }

    fn toggle_mark_row(&mut self, row: usize) {
        let pane = self.pane_mut();
        let Some(key) = pane.item(row).map(Package::key) else {
            return;
        };
        if !pane.marked.remove(&key) {
            pane.marked.insert(key);
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        let left_click = m.kind == MouseEventKind::Down(MouseButton::Left);
        if let Some(job) = self.job.as_mut() {
            match m.kind {
                MouseEventKind::ScrollUp => job.scroll(3),
                MouseEventKind::ScrollDown => job.scroll(-3),
                _ if left_click && matches!(job.state, JobState::Finished { .. }) && !self.hits.job.contains(pos) => {
                    self.close_job()
                }
                _ => {}
            }
            return;
        }
        let wheel = match m.kind {
            MouseEventKind::ScrollUp => Some(-3),
            MouseEventKind::ScrollDown => Some(3),
            _ => None,
        };
        let row = self
            .hits
            .popup_rows
            .iter()
            .find(|(r, _)| r.contains(pos))
            .map(|(_, i)| *i);
        let outside = left_click && !self.hits.popup.contains(pos);
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            // Click selects a row, a second click on it activates it (Enter),
            // the wheel moves, a click outside closes.
            Mode::Repos(mut v) if v.adding.is_none() => {
                match (left_click, row, wheel) {
                    (true, Some(i), _) if i == v.selected => return self.repos_key(v, KeyEvent::from(KeyCode::Enter)),
                    (true, Some(i), _) => v.selected = i,
                    (_, _, Some(d)) => v.selected = step(v.selected, d, v.repos.len()),
                    _ if outside => return,
                    _ => {}
                }
                self.mode = Mode::Repos(v);
            }
            // Typing a name: only a click outside closes; other events
            // (moves, clicks on rows) must not drop the text.
            Mode::Repos(v) => {
                if !outside {
                    self.mode = Mode::Repos(v);
                }
            }
            Mode::Tree(mut t) => {
                match (left_click, row, wheel) {
                    // Rows are positions in `visible()`; the selection is a node.
                    (true, Some(i), _) if i == t.selected_row() => {
                        return self.tree_key(t, KeyEvent::from(KeyCode::Enter));
                    }
                    (true, Some(i), _) => t.select_row(i),
                    (_, _, Some(d)) => t.select_row(step(t.selected_row(), d, t.visible().len())),
                    _ if outside => return,
                    _ => {}
                }
                self.mode = Mode::Tree(t);
            }
            // Click selects, a second click undoes (asks first).
            Mode::History(mut v) => {
                match (left_click, row, wheel) {
                    (true, Some(i), _) if i == v.selected => {
                        return self.history_key(v, KeyEvent::from(KeyCode::Enter));
                    }
                    (true, Some(i), _) => v.selected = i,
                    (_, _, Some(w)) => v.selected = step(v.selected, w, v.entries.len()),
                    _ if outside => return,
                    _ => {}
                }
                self.mode = Mode::History(v);
            }
            Mode::Disk(mut d) => {
                match (left_click, row, wheel) {
                    (true, Some(i), _) if i == d.selected => return self.disk_key(d, KeyEvent::from(KeyCode::Enter)),
                    (true, Some(i), _) => d.selected = i,
                    (_, _, Some(w)) => d.selected = step(d.selected, w, d.largest.len()),
                    _ if outside => return,
                    _ => {}
                }
                self.mode = Mode::Disk(d);
            }
            // Click previews, a second click keeps it; a click outside goes
            // back to the theme in use before.
            Mode::Themes(mut t) => {
                match (left_click, row, wheel) {
                    (true, Some(i), _) if i == t.selected => return self.keep_theme(&t),
                    (true, Some(i), _) => t.selected = i,
                    (_, _, Some(w)) => t.selected = step(t.selected, w, t.themes.len()),
                    _ if outside => return self.revert_theme(&t),
                    _ => {}
                }
                self.preview_theme(&t);
                self.mode = Mode::Themes(t);
            }
            Mode::Versions(mut pick) => {
                if let Some(d) = wheel {
                    pick.selected = if d < 0 {
                        pick.selected.saturating_sub(1)
                    } else {
                        pick.selected
                            .saturating_add(1)
                            .min(pick.versions.len().saturating_sub(1))
                    };
                }
                let row = self
                    .hits
                    .popup_rows
                    .iter()
                    .find(|(r, _)| r.contains(pos))
                    .map(|(_, i)| *i);
                match (left_click, row) {
                    // Clicking the selected row again installs it.
                    (true, Some(i)) if i == pick.selected => self.pick_version(pick),
                    (true, Some(i)) => {
                        pick.selected = i;
                        self.mode = Mode::Versions(pick);
                    }
                    (true, None) if pick.can_hold && self.hits.hold_toggle.contains(pos) => {
                        pick.hold = !pick.hold;
                        self.mode = Mode::Versions(pick);
                    }
                    (true, None) if !self.hits.popup.contains(pos) => self.log("cancelled"),
                    _ => self.mode = Mode::Versions(pick),
                }
            }
            Mode::Help => {
                if let Some(d) = wheel {
                    self.scroll_popup(d);
                }
                if !left_click {
                    self.mode = Mode::Help;
                }
            }
            Mode::Confirm(p) => {
                if let Some(d) = wheel {
                    self.scroll_popup(d);
                }
                if left_click && self.hits.confirm_yes.contains(pos) {
                    self.execute(p);
                } else if left_click && (self.hits.confirm_no.contains(pos) || !self.hits.popup.contains(pos)) {
                    self.log("cancelled");
                } else {
                    self.mode = Mode::Confirm(p);
                }
            }
            mode => {
                // Typing continues unless the click lands somewhere else.
                if matches!(mode, Mode::Input) && !(left_click && !self.hits.input.contains(pos)) {
                    self.mode = Mode::Input;
                }
                self.on_mouse_normal(m, pos);
            }
        }
    }

    /// Jump a scrollbar to the mouse row (click or drag on the track).
    fn drag_bar(&mut self, bar: Bar, y: u16) {
        let r = match bar {
            Bar::List => self.hits.list_bar,
            Bar::Details => self.hits.details_bar,
            Bar::Log => self.hits.log_bar,
        };
        let span = r.height.saturating_sub(1);
        if r.height == 0 {
            return;
        }
        let frac = if span == 0 {
            0.0
        } else {
            f64::from(y.max(r.y).min(r.y.saturating_add(span)).saturating_sub(r.y)) / f64::from(span)
        };
        let at = |max: usize| (frac * max as f64).round() as usize;
        match bar {
            Bar::List => {
                if let Some(last) = self.pane().view.len().checked_sub(1) {
                    self.select_row(at(last).min(last));
                }
            }
            Bar::Details => self.detail_scroll = u16::try_from(at(usize::from(self.detail_max))).unwrap_or(u16::MAX),
            Bar::Log => self.log_scroll = self.log_max.saturating_sub(at(self.log_max)),
        }
    }

    fn bar_at(&self, pos: Position) -> Option<Bar> {
        [
            (self.hits.list_bar, Bar::List),
            (self.hits.details_bar, Bar::Details),
            (self.hits.log_bar, Bar::Log),
        ]
        .into_iter()
        .find(|(r, _)| r.contains(pos))
        .map(|(_, b)| b)
    }

    fn on_mouse_normal(&mut self, m: MouseEvent, pos: Position) {
        match m.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(bar) = self.dragging {
                    self.drag_bar(bar, pos.y);
                }
            }
            MouseEventKind::Up(_) => self.dragging = None,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if self.hits.details.contains(pos) {
                    self.detail_scroll = if down {
                        self.detail_scroll.saturating_add(3)
                    } else {
                        self.detail_scroll.saturating_sub(3)
                    };
                } else if self.hits.log.contains(pos) {
                    self.log_scroll = if down {
                        self.log_scroll.saturating_sub(1)
                    } else {
                        self.log_scroll.saturating_add(1).min(self.log_max)
                    };
                } else {
                    self.move_sel(if down { 3 } else { -3 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(bar) = self.bar_at(pos) {
                    self.dragging = Some(bar);
                    self.drag_bar(bar, pos.y);
                } else if let Some(&(_, tab)) = self.hits.tabs.iter().find(|(r, _)| r.contains(pos)) {
                    self.set_tab(tab);
                } else if self.hits.input.contains(pos) {
                    self.mode = Mode::Input;
                } else if let Some(row) = self.list_row_at(pos) {
                    self.select_row(row);
                    // "> [ ] " : the checkbox occupies columns 2..5 of the row.
                    if pos.x < self.hits.list.x.saturating_add(6) {
                        self.toggle_mark_row(row);
                    }
                } else if let Some(&(_, s)) = self.hits.sections.iter().find(|(r, _)| r.contains(pos)) {
                    self.toggle_section(s);
                } else if self.hits.status_source.contains(pos) {
                    self.perform(Action::Source);
                } else if self.hits.status_mode.contains(pos) {
                    self.perform(Action::RunMode);
                } else if self.hits.log_clear.contains(pos) {
                    self.perform(Action::ClearLog);
                } else if let Some(&(_, action)) = self.hits.footer.iter().find(|(r, _)| r.contains(pos)) {
                    self.perform(action);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(row) = self.list_row_at(pos) {
                    self.select_row(row);
                    self.toggle_mark_row(row);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking() {
        let mut v: Vec<Package> = ["street", "tree-doc", "abc", "tree", "subtree"]
            .iter()
            .map(|n| Package::new(*n, ""))
            .collect();
        rank(&mut v, "tree");
        let names: Vec<&str> = v.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["tree", "tree-doc", "street", "subtree", "abc"]);

        let mut v: Vec<Package> = ["tabtopus", "atop", "graykode/tap/abtop"]
            .iter()
            .map(|n| Package::new(*n, ""))
            .collect();
        rank(&mut v, "abtop");
        assert_eq!(v[0].name, "graykode/tap/abtop");
    }

    #[test]
    fn tab_steps_wrap() {
        assert_eq!(Tab::Search.step(-1), Tab::Updates);
        assert_eq!(Tab::Updates.step(1), Tab::Search);
        assert_eq!(Tab::Installed.step(1), Tab::Orphans);
    }

    #[test]
    fn explicit_filter_orphans_and_sections() {
        let mut app = crate::testing::mock_app();
        app.tick();
        assert_eq!(app.panes.installed.view.len(), 50);
        assert_eq!(app.panes.orphans.items.len(), 2);
        assert!(app.held.contains(&("mock".to_string(), "pkg03".to_string())));
        // Toggle explicit-only: the mock marks even-numbered packages explicit.
        app.perform(Action::Explicit);
        app.tick();
        assert_eq!(app.tab, Tab::Installed);
        assert_eq!(app.panes.installed.view.len(), 25);
        app.perform(Action::Explicit);
        assert_eq!(app.panes.installed.view.len(), 50);

        // Sections load for the selection; Files only once expanded.
        app.tick();
        let key = app.selected().map(Package::key).unwrap_or_default();
        assert_eq!(
            app.sections.get(&(key.clone(), Section::Depends)),
            Some(&SectionState::Ready(vec!["libmock".into(), "libz".into()]))
        );
        assert!(!app.sections.contains_key(&(key.clone(), Section::Files)));
        app.perform(Action::Expand);
        app.tick();
        app.tick();
        assert!(matches!(
            app.sections.get(&(key.clone(), Section::Files)),
            Some(SectionState::Ready(_))
        ));
        app.perform(Action::Expand);
        assert_eq!(app.expanded.len(), DEFAULT_EXPANDED.len());
    }

    #[test]
    fn unsupported_lists_are_informational() {
        let mut app = crate::testing::mock_app_with(crate::testing::Mock {
            unsupported: true,
            ..Default::default()
        });
        app.tick();
        assert!(app.panes.orphans.unsupported.is_some());
        assert!(app.log.iter().all(|l| !l.starts_with("error")), "{:?}", app.log);
        app.perform(Action::Explicit);
        app.tick();
        assert!(!app.explicit_only);
    }
}

#[cfg(test)]
mod multi_tests {
    use super::*;
    use crate::testing::{Mock, mock_app_multi};

    #[test]
    fn source_filter_and_routing() {
        let mut app = mock_app_multi(vec![Mock::named("one"), Mock::named("two")]);
        app.tick();
        assert_eq!(app.panes.installed.items.len(), 100);
        app.perform(Action::Source);
        assert_eq!(app.source_filter.as_deref(), Some("one"));
        assert_eq!(app.panes.installed.view.len(), 50);
        app.perform(Action::Source);
        assert_eq!(app.source_filter.as_deref(), Some("two"));
        app.perform(Action::Source);
        assert_eq!(app.source_filter, None);
        // Explicit filter works per source: 25 of each.
        app.perform(Action::Explicit);
        app.tick();
        assert_eq!(app.panes.installed.view.len(), 50);
    }

    #[test]
    fn hold_toggles_and_routes() {
        let mut app = mock_app_multi(vec![Mock::named("one"), Mock::named("two")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Source); // "one" only
        // pkg03 from "one" is held: p releases it.
        for _ in 0..3 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        assert_eq!(app.selected().map(|p| p.name.clone()).as_deref(), Some("pkg03"));
        app.perform(Action::Hold);
        app.tick(); // hold plans are built in the background
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        assert_eq!(p.title, "Release pkg03");
        assert_eq!(p.cmds[0].display(None), "one unhold pkg03");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        // A package that is not held gets held, by its own manager.
        app.perform(Action::Source); // "two" only
        app.on_key(KeyEvent::from(KeyCode::Char('G')));
        app.perform(Action::Hold);
        app.tick(); // hold plans are built in the background
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        assert!(p.title.starts_with("Hold "), "{}", p.title);
        assert_eq!(p.cmds[0].program, "two");
    }

    #[test]
    fn sandbox_export_import_touch_no_files() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.cfg.export_path = Some("/nonexistent-dir/pakdeck.toml".into());
        app.tick();
        app.perform(Action::Export);
        app.tick();
        assert!(
            app.log
                .iter()
                .any(|l| l.starts_with("[dry-run] package list not written")),
            "{:?}",
            app.log
        );
        assert!(
            matches!(app.mode, Mode::Normal),
            "sandbox export must not prompt to write"
        );
        app.perform(Action::Import);
        app.tick();
        assert!(app.log.iter().any(|l| l == "one: nothing to do"), "{:?}", app.log);
    }

    #[test]
    fn partial_failure_logged() {
        let mut app = mock_app_multi(vec![
            Mock::named("ok"),
            Mock {
                fail: true,
                ..Mock::named("bad")
            },
        ]);
        app.tick();
        assert_eq!(app.panes.installed.items.len(), 50);
        assert!(app.log.iter().any(|l| l.starts_with("error: bad:")), "{:?}", app.log);
    }

    #[test]
    fn stale_results_after_reload_are_dropped() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        let old = app.epoch;
        app.reload_all();
        // A held-list answer requested before the reload arrives late.
        let _ = app.tx.send(Msg::Held {
            epoch: old,
            result: Ok(vec![("one".into(), "stale".into())]),
        });
        app.tick();
        assert!(!app.held.contains(&("one".to_string(), "stale".to_string())));
        assert!(app.held.contains(&("one".to_string(), "pkg03".to_string())));
    }

    #[test]
    fn explicit_filter_keeps_sources_that_cannot_tell() {
        let mut app = mock_app_multi(vec![
            Mock::named("one"),
            Mock {
                unsupported: true,
                ..Mock::named("u")
            },
        ]);
        app.tick();
        app.perform(Action::Explicit);
        app.tick();
        // 25 explicit from "one", all 50 from "u" (it cannot tell).
        assert_eq!(app.panes.installed.view.len(), 75);
    }

    #[test]
    fn selection_follows_the_package_and_marks_respect_filters() {
        let mut app = mock_app_multi(vec![Mock::named("one"), Mock::named("two")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        // Rows: pkg00 one, pkg00 two, pkg01 one, ... ; select pkg10 from "two".
        for _ in 0..21 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        assert_eq!(app.selected().map(Package::key).as_deref(), Some("two//pkg10"));
        // A text filter that keeps it visible at another row: it stays selected.
        app.panes.installed.filter = "pkg1".into();
        app.rebuild_view(Tab::Installed);
        assert_eq!(app.selected().map(Package::key).as_deref(), Some("two//pkg10"));
        app.panes.installed.filter.clear();
        app.rebuild_view(Tab::Installed);
        assert_eq!(app.selected().map(Package::key).as_deref(), Some("two//pkg10"));

        // Mark it, then show only "one": the hidden mark is not acted on.
        app.perform(Action::Mark);
        app.perform(Action::Source);
        assert_eq!(app.source_filter.as_deref(), Some("one"));
        let t = app.targets();
        assert!(t.iter().all(|p| p.source == "one"), "{t:?}");
    }

    #[test]
    fn export_asks_before_writing() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.sandbox = false;
        app.dry_run = false;
        let dir = std::env::temp_dir().join(format!("pakdeck-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("list.toml");
        std::fs::write(&file, "hand edited").unwrap();
        app.cfg.export_path = Some(file.display().to_string());
        app.tick();
        app.perform(Action::Export);
        app.tick();
        let Mode::Confirm(p) = &app.mode else {
            panic!("export must ask first")
        };
        assert!(p.title.contains("replaces the existing file"), "{}", p.title);
        // Cancel: the file is untouched.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hand edited");
        // Confirm: replaced.
        app.perform(Action::Export);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('y')));
        assert!(std::fs::read_to_string(&file).unwrap().contains("[one]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prompts_wait_their_turn_and_imports_always_ask() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.cfg.confirm = false;
        app.dry_run = false;
        app.tick();
        // Help is open when the hold plan arrives: it is queued, not lost.
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Help);
        let _ = app.tx.send(Msg::Plan {
            title: "Hold x".into(),
            result: Ok((vec![Cmd::user("one", &["hold", "x"])], Vec::new())),
            draft: Draft::other(),
        });
        app.tick();
        assert!(matches!(app.mode, Mode::Help));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.dry_run = true; // keep the queued run from executing anything
        app.tick();
        assert!(
            matches!(&app.mode, Mode::Confirm(p) if p.title == "Hold x"),
            "queued prompt not shown"
        );
        app.on_key(KeyEvent::from(KeyCode::Esc));
        // confirm = false, yet an import plan is shown before running.
        app.dry_run = false;
        let _ = app.tx.send(Msg::ImportPlan {
            path: "/tmp/list.toml".into(),
            result: Ok((vec![Cmd::user("one", &["install", "x"])], Vec::new())),
        });
        app.tick();
        assert!(matches!(&app.mode, Mode::Confirm(p) if p.force_confirm));
        assert!(app.run_request.is_none() && app.job.is_none());
    }

    #[test]
    fn waiting_plans_are_dropped_on_reload() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.perform(Action::Help);
        let _ = app.tx.send(Msg::Cleanup(Ok(vec![Cmd::user("one", &["clean"])])));
        app.tick();
        app.reload_all();
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.tick();
        assert!(matches!(app.mode, Mode::Normal), "stale plan was shown");
        assert!(
            app.log
                .iter()
                .any(|l| l.ends_with("dropped, the package lists changed; run it again"))
        );
    }

    #[test]
    fn dry_run_switched_on_in_the_popup_prevents_the_write() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.sandbox = false;
        app.dry_run = false;
        let dir = std::env::temp_dir().join(format!("pakdeck-dry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("list.toml");
        app.cfg.export_path = Some(file.display().to_string());
        app.tick();
        app.perform(Action::Export);
        app.tick();
        assert!(matches!(app.mode, Mode::Confirm(_)));
        app.dry_run = true;
        app.on_key(KeyEvent::from(KeyCode::Char('y')));
        assert!(!file.exists(), "dry-run must not write");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn names(app: &App) -> Vec<String> {
        let p = app.pane();
        p.view
            .iter()
            .filter_map(|&i| p.items.get(i))
            .map(|p| p.name.clone())
            .collect()
    }

    #[test]
    fn fuzzy_filter_ranks_matches() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Search); // filter on non-search tabs
        for c in "p49".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        let v = names(&app);
        assert_eq!(v.first().map(String::as_str), Some("pkg49"), "{v:?}");
        // Subsequence match: "pk4" matches pkg40-49 and also pkg04/14/24/34
        // (p, k, 4 in order); the tighter pkg4x matches rank first.
        app.panes.installed.filter = "pk4".into();
        app.rebuild_view(Tab::Installed);
        let v = names(&app);
        assert_eq!(v.len(), 14);
        assert!(v.iter().take(10).all(|n| n.starts_with("pkg4")), "{v:?}");
    }

    #[test]
    fn sort_cycles_through_available_keys() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Sort);
        assert_eq!(app.sort, SortKey::Name);
        app.perform(Action::Sort);
        assert_eq!(app.sort, SortKey::Size);
        assert_eq!(names(&app).first().map(String::as_str), Some("pkg00"), "biggest first");
        app.perform(Action::Sort);
        assert_eq!(app.sort, SortKey::Date);
        assert_eq!(names(&app).first().map(String::as_str), Some("pkg49"), "newest first");
        // Source is skipped with a single package manager.
        app.perform(Action::Sort);
        assert_eq!(app.sort, SortKey::Default);
    }

    #[test]
    fn search_history_steps_back_and_forth() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        for q in ["tree", "vim", "git"] {
            app.perform(Action::Search);
            app.query.clear();
            for c in q.chars() {
                app.on_key(KeyEvent::from(KeyCode::Char(c)));
            }
            app.on_key(KeyEvent::from(KeyCode::Enter));
        }
        assert_eq!(app.search_history, ["tree", "vim", "git"]);
        app.perform(Action::Search);
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.query, "git");
        app.on_key(KeyEvent::from(KeyCode::Up));
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.query, "tree");
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.query, "tree", "stays at the oldest");
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.query, "vim");
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.query, "git", "past the newest: the text typed before (the draft)");
    }

    #[test]
    fn version_picker_installs_and_holds() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Versions);
        app.tick();
        let Mode::Versions(pick) = &app.mode else {
            panic!("no version picker")
        };
        let ids: Vec<&str> = pick.versions.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["2.0", "1.0", "0.9"]);
        assert_eq!(pick.selected, 1, "starts on the installed version");
        assert!(pick.hold, "hold is on by default");
        app.on_key(KeyEvent::from(KeyCode::Char('j'))); // 0.9
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.tick();
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        assert_eq!(p.title, "Install pkg00 0.9 and hold it");
        let shown: Vec<String> = p.cmds.iter().map(|c| c.display(None)).collect();
        assert_eq!(shown, ["one install pkg00=0.9", "one hold pkg00"]);
        // Without the hold.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.perform(Action::Versions);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char(' ')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.tick();
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        assert_eq!(p.cmds.len(), 1);
    }

    fn picker(app: &App) -> &ThemePick {
        match &app.mode {
            Mode::Themes(t) => t,
            _ => panic!("no theme picker open"),
        }
    }

    #[test]
    fn theme_picker_previews_reverts_and_keeps() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        assert!(app.set_theme("dark"));
        let dark = app.colors;
        app.perform(Action::Theme);
        assert_eq!(picker(&app).original, "dark");
        assert_eq!(picker(&app).themes.first().map(|(n, _)| n.as_str()), Some("auto"));
        assert_eq!(picker(&app).selected, 1, "opens on the theme in use, after auto");
        // Moving previews: the colours change at once.
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.theme, "light");
        assert_ne!(app.colors, dark);
        // Esc goes back to the theme in use before.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!((app.theme.as_str(), app.colors), ("dark", dark));
        // Enter keeps it; this test app is sandboxed, so nothing is written
        // and the log says so.
        app.perform(Action::Theme);
        app.on_key(KeyEvent::from(KeyCode::End));
        let last = picker(&app).themes.last().map(|(n, _)| n.clone()).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.theme, last);
        assert_eq!(app.cfg.theme.preset, last);
        assert!(
            app.log.back().is_some_and(|l| l.contains("for this session")),
            "{:?}",
            app.log.back()
        );
        // Unknown names are refused.
        assert!(!app.set_theme("nope"));
        assert_eq!(app.theme, last);
    }

    #[test]
    fn theme_picker_saves_to_the_config_file() {
        let dir = std::env::temp_dir().join(format!("pakdeck-pick-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.sandbox = false;
        app.config_path = Some(path.clone());
        app.perform(Action::Theme);
        let nord = picker(&app).themes.iter().position(|(n, _)| n == "nord").unwrap();
        for _ in picker(&app).selected..nord {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let saved: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.theme.preset, "nord");
        assert!(app.log.back().is_some_and(|l| l.contains("saved to")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_can_be_picked_again_and_is_saved_as_auto() {
        let dir = std::env::temp_dir().join(format!("pakdeck-auto-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.sandbox = false;
        app.config_path = Some(path.clone());
        app.auto_theme = "light".into();
        assert!(app.set_theme("nord"));
        app.perform(Action::Theme);
        app.on_key(KeyEvent::from(KeyCode::Home));
        // "auto" previews what it resolves to here...
        assert_eq!(app.theme, "auto");
        assert_eq!(Some(app.colors), app.theme_colors("light"));
        // ...and is saved as "auto", so the terminal decides again next time.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let saved: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.theme.preset, "auto");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sudo_is_named_only_when_something_needs_root() {
        let user_only = Multi::new(vec![crate::backend::by_name("brew").unwrap()]);
        assert_eq!(backend_line(&user_only, Some("sudo")), "backend: brew");
        let rooted = Multi::new(vec![Arc::new(Mock::named("one"))]);
        assert_eq!(
            backend_line(&rooted, Some("sudo")),
            "backend: one (commands that need root use sudo)"
        );
        assert_eq!(
            backend_line(&rooted, None),
            "backend: one",
            "running as root: no prefix"
        );
        // Several managers: one that needs root is enough.
        let mixed = Multi::new(vec![
            crate::backend::by_name("brew").unwrap(),
            Arc::new(Mock::named("one")),
        ]);
        assert!(backend_line(&mixed, Some("doas")).ends_with("use doas)"));
    }

    #[test]
    fn missing_sudo_is_warned_about_only_when_root_is_needed() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.warn_no_elevation();
        let last_two: Vec<&String> = app.log.iter().rev().take(2).collect();
        assert!(
            last_two.iter().any(|l| l.contains("no sudo or doas found")),
            "{last_two:?}"
        );
        assert!(last_two.iter().any(|l| l.contains("set sudo")), "{last_two:?}");
        // The log pane does not wrap: each line fits 80 columns.
        assert!(last_two.iter().all(|l| l.chars().count() <= 78), "{last_two:?}");
        // A manager that runs as the user (like brew): nothing to warn about.
        let mut user_only = mock_app_multi(vec![Mock {
            user_only: true,
            ..Mock::named("brewish")
        }]);
        let before = user_only.log.len();
        user_only.warn_no_elevation();
        assert_eq!(user_only.log.len(), before, "no root needed, no warning");
    }

    /// An app that "runs" commands by handing them to the main loop (no
    /// processes), with a history file in a temporary directory.
    fn recording_app(dir: &std::path::Path) -> App {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.sandbox = false;
        app.dry_run = false;
        app.run_mode = RunMode::Suspend;
        app.history_path = Some(dir.join("history.jsonl"));
        app.tick();
        app
    }

    /// Confirm the open prompt and play the main loop: the commands "ran".
    fn run_confirmed(app: &mut App, ok: bool) -> Vec<Cmd> {
        app.on_key(KeyEvent::from(KeyCode::Char('y')));
        let cmds = app.run_request.take().expect("nothing was run");
        app.after_run(ok);
        app.tick();
        cmds
    }

    #[test]
    fn history_records_runs_and_undoes_them() {
        let dir = std::env::temp_dir().join(format!("pakdeck-hist-app-{}", std::process::id()));
        let mut app = recording_app(&dir);
        // Install pkg05 (on the Installed tab: recorded with its version).
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        for _ in 0..5 {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        app.perform(Action::Install);
        run_confirmed(&mut app, true);
        // Update pkg01 from the Updates tab: from 1.0 to 2.0.
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        app.perform(Action::Update);
        run_confirmed(&mut app, true);
        // A failed removal is recorded too, and cannot be undone.
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Delete);
        run_confirmed(&mut app, false);
        let entries = crate::history::load(&dir.join("history.jsonl")).unwrap();
        let summary: Vec<(Op, bool)> = entries.iter().map(|e| (e.op, e.ok)).collect();
        assert_eq!(summary, [(Op::Install, true), (Op::Upgrade, true), (Op::Remove, false)]);
        // pkg05 was installed already: nothing for undo to take back.
        assert!(entries[0].items.is_empty(), "{:?}", entries[0].items);
        assert_eq!(entries[1].items[0].from.as_deref(), Some("1.0"));
        assert_eq!(entries[1].items[0].to.as_deref(), Some("2.0"));
        // H: newest first; the failed one says why it cannot be undone.
        app.perform(Action::History);
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let Mode::History(v) = &app.mode else {
            panic!("history closed")
        };
        assert!(
            v.error.as_deref().is_some_and(|e| e.contains("it failed")),
            "{:?}",
            v.error
        );
        // Undo the update: back to 1.0, asked first, and recorded in turn.
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        assert_eq!(p.title, "Undo: Update pkg01");
        assert_eq!(p.cmds[0].display(None), "one install pkg01=1.0");
        run_confirmed(&mut app, true);
        let entries = crate::history::load(&dir.join("history.jsonl")).unwrap();
        let undo = entries.last().unwrap();
        assert_eq!((undo.title.as_str(), undo.op), ("Undo: Update pkg01", Op::Upgrade));
        assert_eq!(undo.items[0].to.as_deref(), Some("1.0"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dry_runs_and_turned_off_history_record_nothing() {
        let dir = std::env::temp_dir().join(format!("pakdeck-hist-off-{}", std::process::id()));
        let mut app = recording_app(&dir);
        app.dry_run = true;
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Install);
        app.on_key(KeyEvent::from(KeyCode::Char('y')));
        assert!(app.run_request.is_none());
        app.dry_run = false;
        app.cfg.history = false;
        app.perform(Action::Install);
        run_confirmed(&mut app, true);
        assert!(crate::history::load(&dir.join("history.jsonl")).unwrap().is_empty());
        app.perform(Action::History);
        assert!(app.log.back().is_some_and(|l| l.contains("history is turned off")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_users_own_themes_are_offered() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.cfg = toml::from_str("[themes.mine]\nbase = \"dracula\"\naccent = \"#010203\"\n").unwrap();
        app.perform(Action::Theme);
        assert_eq!(picker(&app).themes.last().map(|(n, _)| n.as_str()), Some("mine"));
        app.on_key(KeyEvent::from(KeyCode::End));
        assert_eq!(app.colors.accent, ratatui::style::Color::Rgb(1, 2, 3));
    }

    #[test]
    fn version_picker_uses_installed_state_and_releases_holds() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        // Search result "x0": not installed, so nothing is marked installed.
        app.perform(Action::Search);
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.tick();
        app.perform(Action::Versions);
        app.tick();
        let Mode::Versions(pick) = &app.mode else {
            panic!("no picker")
        };
        assert!(!pick.pkg.installed && pick.pkg.version.is_empty(), "{:?}", pick.pkg);
        assert_eq!(pick.selected, 0);
        app.on_key(KeyEvent::from(KeyCode::Esc));
        // pkg03 is held: picking a version releases the hold first.
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        for _ in 0..3 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        app.perform(Action::Versions);
        app.tick();
        let Mode::Versions(pick) = &app.mode else {
            panic!("no picker")
        };
        assert!(pick.held_now && pick.pkg.installed);
        app.on_key(KeyEvent::from(KeyCode::Char('j'))); // 0.9
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.tick();
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        let shown: Vec<String> = p.cmds.iter().map(|c| c.display(None)).collect();
        assert_eq!(shown, ["one unhold pkg03", "one install pkg03=0.9", "one hold pkg03"]);
    }

    #[test]
    fn summaries_match_as_substrings_only() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        // "inst" is a substring of every summary ("installed one"): all match.
        app.panes.installed.filter = "inst".into();
        app.rebuild_view(Tab::Installed);
        assert_eq!(app.panes.installed.view.len(), 50);
        // "ione" is a subsequence of "installed one" but not a substring.
        app.panes.installed.filter = "ione".into();
        app.rebuild_view(Tab::Installed);
        assert_eq!(app.panes.installed.view.len(), 0);
    }

    #[test]
    fn sort_skips_lists_without_the_data_and_history_keeps_the_draft() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Sort);
        app.perform(Action::Sort);
        assert_eq!(app.sort, SortKey::Size);
        assert_eq!(app.panes.installed.sort, SortKey::Size);
        // Search results have no sizes: that list keeps its relevance order.
        assert_eq!(app.panes.search.sort, SortKey::Default);

        app.search_history = vec!["tree".into()];
        app.perform(Action::Search);
        app.on_key(KeyEvent::from(KeyCode::Char('1')));
        app.tab = Tab::Search;
        app.query = "dra".into();
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.query, "tree");
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.query, "dra", "draft restored");
    }

    #[test]
    fn reload_keeps_the_scroll_position() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        for _ in 0..30 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        *app.panes.installed.state.offset_mut() = 20;
        app.reload_all();
        app.tick();
        assert_eq!(app.panes.installed.state.offset(), 20, "reload moved the view");
        assert_eq!(app.selected().map(|p| p.name.clone()).as_deref(), Some("pkg30"));
    }

    #[test]
    fn a_new_filter_starts_at_the_top_even_when_the_offset_still_fits() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        for _ in 0..8 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        *app.panes.installed.state.offset_mut() = 5;
        // "p" keeps all 50 rows, so offset 5 would still fit.
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        app.on_key(KeyEvent::from(KeyCode::Char('p')));
        assert_eq!(app.panes.installed.state.offset(), 0);
    }

    fn tree(app: &App) -> &DepTree {
        match &app.mode {
            Mode::Tree(t) => t,
            _ => panic!("no tree open"),
        }
    }

    #[test]
    fn dependency_tree_expands_lazily_and_marks_cycles() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Tree);
        app.tick();
        // pkg00 -> libmock, libz (loaded with the root).
        let names = |app: &App| -> Vec<String> {
            let t = tree(app);
            t.visible()
                .iter()
                .filter_map(|i| t.nodes.get(*i))
                .map(|n| n.name.clone())
                .collect()
        };
        assert_eq!(names(&app), ["pkg00", "libmock", "libz"]);
        // Expand libmock -> libbase, then libbase -> libmock (a cycle).
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.tick();
        assert_eq!(names(&app), ["pkg00", "libmock", "libbase", "libmock", "libz"]);
        let t = tree(&app);
        let cycle = t.visible().get(3).and_then(|i| t.nodes.get(*i)).map(|n| n.cycle);
        assert_eq!(cycle, Some(true), "the repeated libmock is a cycle");
        // The cycle cannot be expanded.
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.tick();
        assert_eq!(names(&app).len(), 5);
        // h on the cycle goes to its parent; h again collapses it.
        app.on_key(KeyEvent::from(KeyCode::Char('h')));
        assert_eq!(tree(&app).selected_row(), 2);
        app.on_key(KeyEvent::from(KeyCode::Char('h')));
        assert_eq!(names(&app), ["pkg00", "libmock", "libbase", "libz"]);
        // The tree key again flips to "required by".
        app.on_key(KeyEvent::from(KeyCode::Char('t')));
        app.tick();
        assert_eq!(tree(&app).direction, Section::RequiredBy);
        // g jumps to the package (pkg00 is in the Installed list).
        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.selected().map(|p| p.name.clone()).as_deref(), Some("pkg00"));
    }

    #[test]
    fn tree_selection_stays_on_its_package_when_children_arrive() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Tree);
        app.tick();
        // Expand libmock, and move on to libz before its children arrive.
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(tree(&app).selected_row(), 2);
        app.tick();
        // libbase was inserted above: libz moved down a row, and stays selected.
        let t = tree(&app);
        assert_eq!(t.selected_row(), 3);
        assert_eq!(t.nodes.get(t.selected).map(|n| n.name.as_str()), Some("libz"));
    }

    #[test]
    fn disk_usage_totals_and_largest() {
        let mut app = mock_app_multi(vec![Mock::named("one"), Mock::named("two")]);
        app.tick();
        app.perform(Action::Disk);
        let Mode::Disk(d) = &app.mode else {
            panic!("no disk view")
        };
        // Each mock has 50 packages of 50000..1000 bytes: 1,275,000 bytes.
        assert_eq!(d.totals.len(), 2);
        assert!(
            d.totals
                .iter()
                .all(|t| t.bytes == 1_275_000 && t.sized == 50 && t.packages == 50)
        );
        assert_eq!(d.largest.len(), 15);
        assert_eq!(d.largest.first().map(|p| p.size), Some(Some(50_000)));
        assert!(d.measuring.is_empty(), "sizes were all known");
        // Enter goes to the selected package.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.tab, Tab::Installed);
        assert_eq!(app.selected().map(|p| p.name.clone()).as_deref(), Some("pkg00"));
    }

    #[test]
    fn repositories_add_remove_and_filter() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.perform(Action::Repos);
        app.tick();
        let Mode::Repos(v) = &app.mode else {
            panic!("no repos view")
        };
        assert_eq!(v.repos[0].name, "mock/extra");
        assert_eq!(v.repos[0].source, "one");
        // Add: type a name, Enter -> confirm with the backend's command.
        app.on_key(KeyEvent::from(KeyCode::Char('a')));
        for c in "user/new".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        assert_eq!(p.cmds[0].display(None), "one repo-add user/new");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        // An invalid name keeps the popup and the text, and says why.
        app.perform(Action::Repos);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('a')));
        for c in "foobar".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let Mode::Repos(v) = &app.mode else {
            panic!("the popup closed on an invalid name")
        };
        assert_eq!(v.adding.as_deref(), Some("foobar"));
        assert!(
            v.error.as_deref().is_some_and(|e| e.contains("owner/name")),
            "{:?}",
            v.error
        );
        // Typing clears the message; Esc leaves the input line.
        app.on_key(KeyEvent::from(KeyCode::Backspace));
        assert!(matches!(&app.mode, Mode::Repos(v) if v.error.is_none()));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(&app.mode, Mode::Repos(v) if v.adding.is_none()));
        // Installed packages per repository: the mock's 25 even ones.
        assert!(matches!(&app.mode, Mode::Repos(v) if v.installed == [25]));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        // Remove the selected repository.
        app.perform(Action::Repos);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('d')));
        let Mode::Confirm(p) = &app.mode else {
            panic!("no confirm")
        };
        assert_eq!(p.cmds[0].display(None), "one repo-remove mock/extra");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        // Filter Installed by it: the mock's even packages.
        app.perform(Action::Repos);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('f')));
        assert_eq!(app.tab, Tab::Installed);
        assert_eq!(app.panes.installed.view.len(), 25);
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.panes.installed.view.len(), 50, "Esc clears the repository filter");
    }

    #[test]
    fn disk_measures_missing_sizes_once_and_drops_stale_results() {
        let mut app = mock_app_multi(vec![Mock {
            sizeless: true,
            ..Mock::named("brewish")
        }]);
        app.tick();
        assert!(app.panes.installed.items.iter().all(|p| p.size.is_none()));
        // A result from before a reload is ignored entirely.
        let old = app.epoch.wrapping_sub(1);
        let _ = app.tx.send(Msg::Sizes {
            epoch: old,
            source: "brewish".into(),
            result: Ok(vec![("pkg00".into(), 7)]),
        });
        app.tick();
        assert!(app.panes.installed.items.iter().all(|p| p.size.is_none()));
        assert!(app.sizes.is_empty(), "a stale result must not count as measured");
        // z starts the measurement; the popup says so until it arrives.
        app.perform(Action::Disk);
        let Mode::Disk(d) = &app.mode else {
            panic!("no disk view")
        };
        assert_eq!(d.measuring, ["brewish"]);
        app.tick();
        let Mode::Disk(d) = &app.mode else {
            panic!("no disk view")
        };
        assert!(d.measuring.is_empty());
        assert_eq!(d.totals.first().map(|t| (t.bytes, t.sized)), Some((122_500, 50)));
        assert_eq!(d.largest.first().map(|p| p.name.as_str()), Some("pkg49"));
        // Size sort sees the measured sizes at once.
        assert!(app.panes.installed.items.iter().all(|p| p.size.is_some()));
        // Measured once per reload.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.perform(Action::Disk);
        let Mode::Disk(d) = &app.mode else {
            panic!("no disk view")
        };
        assert!(d.measuring.is_empty());
        app.on_key(KeyEvent::from(KeyCode::Esc));
        // r reloads the Installed list: the measured sizes come back with it.
        app.perform(Action::Reload);
        app.tick();
        assert!(!app.panes.installed.loading);
        assert!(
            app.panes.installed.items.iter().all(|p| p.size.is_some()),
            "sizes lost on reload"
        );
    }

    #[test]
    fn disk_during_a_reload_says_why_sizes_are_missing() {
        let mut app = mock_app_multi(vec![Mock {
            sizeless: true,
            ..Mock::named("brewish")
        }]);
        app.tick();
        app.panes.installed.loading = true;
        app.perform(Action::Disk);
        let Mode::Disk(d) = &app.mode else {
            panic!("no disk view")
        };
        assert!(d.measuring.is_empty());
        assert_eq!(d.postponed, ["brewish"]);
        // Once the list is back, the next open measures.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.panes.installed.loading = false;
        app.perform(Action::Disk);
        let Mode::Disk(d) = &app.mode else {
            panic!("no disk view")
        };
        assert_eq!(d.measuring, ["brewish"]);
        assert!(d.postponed.is_empty());
    }

    #[test]
    fn popup_actions_follow_the_key_bindings() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.cfg.keys.add = 'n';
        app.cfg.keys.delete = 'x';
        app.cfg.keys.source = 'F';
        app.cfg.keys.tree = 'Y';
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.perform(Action::Repos);
        app.tick();
        // The old letters do nothing now.
        for c in ['a', 'd', 'f'] {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
            assert!(matches!(&app.mode, Mode::Repos(v) if v.adding.is_none()), "{c} acted");
        }
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(matches!(&app.mode, Mode::Repos(v) if v.adding.is_some()));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(matches!(&app.mode, Mode::Confirm(_)));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.perform(Action::Repos);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('F')));
        assert!(app.repo_filter.is_some());
        // In the tree, the tree key flips the direction; r no longer does.
        app.perform(Action::Tree);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('r')));
        assert_eq!(tree(&app).direction, Section::Depends);
        app.on_key(KeyEvent::from(KeyCode::Char('Y')));
        assert_eq!(tree(&app).direction, Section::RequiredBy);
    }

    #[test]
    fn go_to_package_falls_back_to_any_source_and_clears_only_what_hides_it() {
        let mut app = mock_app_multi(vec![Mock::named("one")]);
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        // Another source's tree names a package installed under "one".
        app.go_to_package("other", "pkg05");
        assert_eq!(app.selected().map(|p| p.name.clone()).as_deref(), Some("pkg05"));
        // Visible already: nothing is cleared.
        app.perform(Action::Explicit);
        app.tick();
        assert!(app.explicit_only);
        app.go_to_package("one", "pkg04");
        assert!(app.explicit_only, "pkg04 is explicit, so the filter stays");
        assert_eq!(app.selected().map(|p| p.name.clone()).as_deref(), Some("pkg04"));
        // Hidden by explicit-only: cleared, and the log says so.
        app.go_to_package("one", "pkg05");
        assert!(!app.explicit_only);
        assert_eq!(app.selected().map(|p| p.name.clone()).as_deref(), Some("pkg05"));
        assert!(
            app.log.back().is_some_and(|l| l.contains("cleared explicit only")),
            "{:?}",
            app.log.back()
        );
    }

    #[test]
    fn repo_nouns_and_add_target() {
        let v = RepoView {
            kind: "Repositories",
            repos: vec![
                Repo {
                    source: "a".into(),
                    ..Repo::default()
                },
                Repo {
                    source: "b".into(),
                    ..Repo::default()
                },
            ],
            installed: vec![0, 0],
            selected: 1,
            adding: None,
            error: None,
            add_hint: "",
        };
        assert_eq!((v.noun().as_str(), v.add_target()), ("repository", "b"));
        let taps = RepoView {
            kind: "Taps",
            repos: Vec::new(),
            installed: Vec::new(),
            selected: 0,
            adding: None,
            error: None,
            add_hint: "",
        };
        assert_eq!((taps.noun().as_str(), taps.add_target()), ("tap", ""));
    }

    #[test]
    fn no_repositories_without_support() {
        let mut app = mock_app_multi(vec![Mock {
            unsupported: true,
            ..Mock::named("u")
        }]);
        app.perform(Action::Repos);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.log
                .back()
                .is_some_and(|l| l.contains("no active package manager has repositories"))
        );
    }
}
