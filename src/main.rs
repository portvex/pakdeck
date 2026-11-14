//! pakdeck binary: argument parsing, non-interactive modes and the TUI loop.
//! Nothing here may panic: output errors (closed pipes) are ignored,
//! terminal setup failures are returned as errors.

use pakdeck::{app, backend, config, ui};

use anyhow::{Context, Result, bail};
use app::App;
use backend::{Backend, Cmd, Package, Section, is_unsupported};
use config::Config;
use pakdeck::history::{Draft, Op};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "pakdeck: universal TUI package manager
(apt, dnf/yum, zypper, pacman, apk, xbps, pkg, nix, brew, flatpak, snap,
winget, scoop)

Usage: pakdeck [options]

Options:
  -n, --dry-run          show commands instead of running them
  -y, --yes              answer yes to prompts (package managers, --import)
  -b, --backend NAMES    package managers to use: one, a comma list, or \"all\"
  -c, --config PATH      use this config file
      --print-config     print a default config file and exit
      --list             print installed packages and exit
      --explicit         print explicitly installed package names and exit
      --orphans          print orphaned packages and exit
      --search QUERY     search packages and exit
      --info PKG         print package information and exit
      --details PKG      print size, dependencies, reverse deps and files
      --versions PKG     print the versions that can be installed, newest first
      --repos            print repositories (Homebrew taps) and exit
      --add-repo NAME [URL]
                         add a repository (brew tap; source:NAME picks the
                         manager)
      --remove-repo NAME remove a repository (brew untap)
      --disk             print installed size per package manager and the
                         largest packages
      --history          print what ran, newest first (numbered for --undo)
      --undo [N]         undo history entry N (default 1, the newest); shows the
                         plan and asks first (-n: only show it, -y: do not ask)
      --install-version PKG VERSION
                         install exactly VERSION (up- or downgrade) and hold it
                         (add --no-hold to skip the hold)
      --updates          print available updates and exit
      --export [FILE]    write explicit and held packages (default: export_path;
                         - for stdout)
      --import FILE      install and hold what an exported list names; shows the
                         plan and asks first (-n: only show it, -y: do not ask)
      --install PKG...   install packages without the TUI (source:name picks
                         the manager)
      --remove PKG...    remove packages without the TUI
      --upgrade [PKG...] upgrade packages (all when none given) without the TUI
      --hold PKG...      hold packages back from updates (pin / lock)
      --unhold PKG...    release held packages
      --held             print held packages and exit
      --cleanup          remove orphans and clean caches without the TUI
      --sync             refresh repository metadata without the TUI
  -h, --help             show this help
  -V, --version          show version";

/// Write to stdout, ignoring errors such as a closed pipe (`pakdeck --list | head`).
fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes()).and_then(|_| o.flush());
}

fn err(s: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{s}");
}

#[derive(Default)]
struct Args {
    dry_run: bool,
    yes: bool,
    backend: Option<String>,
    config: Option<PathBuf>,
    print_config: bool,
    list: bool,
    explicit: bool,
    orphans: bool,
    held: bool,
    updates: bool,
    search: Option<String>,
    info: Option<String>,
    details: Option<String>,
    versions: Option<String>,
    repos: bool,
    disk: bool,
    install_version: Option<(String, String)>,
    no_hold: bool,
    export: Option<Option<String>>,
    import: Option<String>,
    history: bool,
    /// `--undo [N]`: the Nth newest entry (1 when not given).
    undo: Option<usize>,
    op: Option<(String, Vec<String>)>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut a = Args::default();
    // `--export` takes an optional file: set when the next word may be it.
    let mut pending_export = false;
    // `--undo` takes an optional entry number.
    let mut pending_undo = false;
    let mut it = std::env::args_os().skip(1).map(|s| {
        s.into_string()
            .map_err(|s| anyhow::anyhow!("argument is not valid UTF-8: {s:?}"))
    });
    while let Some(arg) = it.next() {
        let arg = arg?;
        if std::mem::take(&mut pending_export) && (arg == "-" || !arg.starts_with('-')) {
            a.export = Some(Some(arg));
            continue;
        }
        if std::mem::take(&mut pending_undo)
            && let Ok(n) = arg.parse::<usize>()
        {
            if n == 0 {
                bail!("--undo counts from 1 (the newest entry)");
            }
            a.undo = Some(n);
            continue;
        }
        let mut value = |name: &str| -> Result<String> { it.next().with_context(|| format!("{name} needs a value"))? };
        match arg.as_str() {
            "-n" | "--dry-run" => a.dry_run = true,
            "-y" | "--yes" => a.yes = true,
            "-b" | "--backend" => a.backend = Some(value("--backend")?),
            "-c" | "--config" => a.config = Some(value("--config")?.into()),
            "--print-config" => a.print_config = true,
            "--list" => a.list = true,
            "--explicit" => a.explicit = true,
            "--orphans" => a.orphans = true,
            "--held" => a.held = true,
            "--updates" => a.updates = true,
            "--search" => a.search = Some(value("--search")?),
            "--info" => a.info = Some(value("--info")?),
            "--details" => a.details = Some(value("--details")?),
            "--versions" => a.versions = Some(value("--versions")?),
            "--repos" => a.repos = true,
            "--history" => a.history = true,
            "--undo" => {
                a.undo = Some(1);
                pending_undo = true;
            }
            "--disk" => a.disk = true,
            "--no-hold" => a.no_hold = true,
            "--install-version" => {
                let pkg = value("--install-version")?;
                let ver = value("--install-version")?;
                a.install_version = Some((pkg, ver));
            }
            "--import" => a.import = Some(value("--import")?),
            "--export" => {
                // Optional file: the next word, unless it is another option.
                a.export = Some(None);
                pending_export = true;
            }
            "--install" | "--remove" | "--upgrade" | "--cleanup" | "--sync" | "--hold" | "--unhold" | "--add-repo"
            | "--remove-repo" => {
                let names = it.by_ref().collect::<Result<Vec<String>>>()?;
                if names.is_empty()
                    && matches!(
                        arg.as_str(),
                        "--install" | "--remove" | "--hold" | "--unhold" | "--add-repo" | "--remove-repo"
                    )
                {
                    if arg.ends_with("-repo") {
                        bail!("{arg} needs a repository name");
                    }
                    bail!("{arg} needs package names");
                }
                match (arg.as_str(), names.len()) {
                    ("--remove-repo", 2..) => bail!("--remove-repo takes one repository name"),
                    ("--add-repo", 3..) => bail!("--add-repo takes a repository name and an optional URL"),
                    _ => {}
                }
                a.op = Some((arg.trim_start_matches('-').to_string(), names));
                break;
            }
            "-h" | "--help" => {
                out(&format!("{USAGE}\n"));
                return Ok(None);
            }
            "-V" | "--version" => {
                out(&format!("pakdeck {}\n", env!("CARGO_PKG_VERSION")));
                return Ok(None);
            }
            other => bail!("unknown argument '{other}' (see --help)"),
        }
    }
    Ok(Some(a))
}

#[cfg(unix)]
fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_root() -> bool {
    // Windows package managers elevate themselves (UAC); never prefix.
    true
}

/// How commands that need root get it.
enum Elevation {
    /// pakdeck runs as root (or on Windows, where managers elevate themselves).
    Root,
    /// Prefixed with this (found, or set in the config).
    Prefix(String),
    /// `sudo = ""` in the config: never prefix, on purpose.
    Disabled,
    /// Not root and neither sudo nor doas found: root commands will fail.
    Missing,
}

impl Elevation {
    fn detect(cfg: &Config) -> Self {
        match cfg.sudo.as_deref() {
            Some("") => Elevation::Disabled,
            Some(s) => Elevation::Prefix(s.to_string()),
            None if is_root() => Elevation::Root,
            None if backend::has("sudo") => Elevation::Prefix("sudo".into()),
            None if backend::has("doas") => Elevation::Prefix("doas".into()),
            None => Elevation::Missing,
        }
    }

    fn prefix(&self) -> Option<String> {
        match self {
            Elevation::Prefix(s) => Some(s.clone()),
            _ => None,
        }
    }
}

fn print_packages(pkgs: &[Package]) {
    let mut s = String::new();
    for p in pkgs {
        let new = p.new_version.as_deref().map(|v| format!(" -> {v}")).unwrap_or_default();
        // A note ("updates itself") goes with the description.
        let desc = match &p.note {
            Some(n) if p.description.is_empty() => format!("({n})"),
            Some(n) => format!("{} ({n})", p.description),
            None => p.description.clone(),
        };
        s.push_str(&format!("{}\t{}{}\t{}\t{}\n", p.name, p.version, new, desc, p.source));
    }
    out(&s);
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            err(&format!("pakdeck: {e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };
    if args.print_config {
        out(config::TEMPLATE);
        return Ok(());
    }
    let mut cfg = Config::load(args.config.as_deref())?;
    cfg.dry_run |= args.dry_run;
    if let Some(b) = args.backend.as_deref() {
        // `-b brew,nix` or `-b all` selects several; one name replaces them.
        cfg.backends = b
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let backend = Arc::new(backend::resolve(&cfg.backends, cfg.backend.as_deref())?);
    let warn = |b: &backend::Multi| {
        b.take_warnings()
            .iter()
            .for_each(|w| err(&format!("pakdeck: warning: {w}")))
    };

    // Non-interactive modes, handy for scripting and testing. Each prints
    // warnings for package managers that failed while others answered.
    let result = query_modes(&args, &cfg, &backend);
    warn(&backend);
    if result? {
        return Ok(());
    }

    let elevation = Elevation::detect(&cfg);
    let sudo = elevation.prefix();
    let missing = matches!(elevation, Elevation::Missing);
    if let Some(file) = &args.import {
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
        let (cmds, notes) = pakdeck::export::plan(&backend, &pakdeck::export::parse(&text)?);
        notes.iter().for_each(|n| err(n));
        let cmds = if args.yes { backend.assume_yes(cmds) } else { cmds };
        let shown: String = cmds
            .iter()
            .map(|c| format!("{}\n", c.display(sudo.as_deref())))
            .collect();
        if cfg.dry_run || cmds.is_empty() {
            out(&shown);
            return Ok(());
        }
        // An import file may come from another machine: never run it
        // without showing it, unless --yes says so.
        ask_first(&shown, args.yes, "--import")?;
        let ok = run_commands(&cmds, sudo.as_deref(), missing);
        let title = format!("Import from {file}");
        record_cli(&cfg, Draft::other(), &title, &cmds, sudo.as_deref(), ok);
        if !ok {
            bail!("import failed");
        }
        return Ok(());
    }
    if args.history {
        let Some(path) = pakdeck::history::default_path() else {
            bail!("no history file location (HOME is not set)");
        };
        let entries = pakdeck::history::load(&path)?;
        let mut s = String::new();
        // Newest first, numbered for --undo.
        for (n, e) in entries.iter().rev().enumerate() {
            let status = match pakdeck::history::why_not(&backend, e, &entries) {
                _ if !e.ok => "failed",
                Some(w) if w.contains("undone already") => "undone",
                Some(_) => "cannot-undo",
                None => "ok",
            };
            s.push_str(&format!(
                "{}\t{} UTC\t{status}\t{}\n",
                n.saturating_add(1),
                pakdeck::util::date_time(e.time),
                e.title
            ));
        }
        out(&s);
        return Ok(());
    }
    if let Some(n) = args.undo {
        let Some(path) = pakdeck::history::default_path() else {
            bail!("no history file location (HOME is not set)");
        };
        let entries = pakdeck::history::load(&path)?;
        let Some(entry) = entries.iter().rev().nth(n.saturating_sub(1)) else {
            bail!("no history entry {n} (pakdeck --history lists them)");
        };
        let (title, cmds, notes) = pakdeck::history::undo_plan(&backend, entry, &entries)?;
        notes.iter().for_each(|n| err(n));
        let cmds = if args.yes { backend.assume_yes(cmds) } else { cmds };
        let shown: String = cmds
            .iter()
            .map(|c| format!("{}\n", c.display(sudo.as_deref())))
            .collect();
        if cfg.dry_run {
            out(&shown);
            return Ok(());
        }
        ask_first(&format!("{title}\n{shown}"), args.yes, "--undo")?;
        let ok = run_commands(&cmds, sudo.as_deref(), missing);
        record_cli(
            &cfg,
            pakdeck::history::undo_draft(entry),
            &title,
            &cmds,
            sudo.as_deref(),
            ok,
        );
        if !ok {
            bail!("undo failed");
        }
        return Ok(());
    }
    if let Some((name, version)) = &args.install_version {
        let mut pkg = backend::parse_target(&backend, name);
        backend::validate_name(&pkg.name)?;
        backend::validate_version(version)?;
        // The installed package decides install vs downgrade (and refresh
        // vs install for snap).
        if let Ok(installed) = backend.installed()
            && let Some(p) = installed
                .iter()
                .find(|p| p.name == pkg.name && (pkg.source.is_empty() || p.source == pkg.source))
        {
            pkg.version = p.version.clone();
            pkg.installed = true;
            pkg.source = p.source.clone();
        }
        // VERSION is a choice's id (a version, or a snap channel).
        let choice = backend
            .versions(&pkg)
            .ok()
            .and_then(|v| v.into_iter().find(|c| c.id == *version))
            .unwrap_or_else(|| backend::VersionChoice::plain(version.as_str()));
        let held_now = backend.held_tagged().is_ok_and(|h| {
            h.iter()
                .any(|(s, n)| *n == pkg.name && (pkg.source.is_empty() || *s == pkg.source))
        });
        let (cmds, notes) = backend::version_plan(backend.as_ref(), &pkg, &choice, held_now, !args.no_hold)?;
        let mut target = pkg.clone();
        target.new_version = Some(choice.version.clone());
        let draft = Draft::new(Op::Upgrade, &[target]);
        notes.iter().for_each(|n| err(n));
        let cmds = if args.yes { backend.assume_yes(cmds) } else { cmds };
        if cfg.dry_run {
            out(&cmds
                .iter()
                .map(|c| format!("{}\n", c.display(sudo.as_deref())))
                .collect::<String>());
            return Ok(());
        }
        let ok = run_commands(&cmds, sudo.as_deref(), missing);
        record_cli(
            &cfg,
            draft,
            &format!("Install {name} {version}"),
            &cmds,
            sudo.as_deref(),
            ok,
        );
        if !ok {
            bail!("install-version failed");
        }
        return Ok(());
    }
    if let Some((op, names)) = &args.op
        && matches!(op.as_str(), "add-repo" | "remove-repo")
    {
        // `source:name` picks the package manager (`brew:user/repo`), like
        // package names; a URL's own colon comes after a slash.
        let first = names.first().map(String::as_str).unwrap_or_default();
        let (source, name) = backend::split_source(&backend, first);
        let Some(repos) = backend.repositories() else {
            bail!("no active package manager has repositories");
        };
        let cmds = if op == "add-repo" {
            let spec: Vec<&str> = std::iter::once(name)
                .chain(names.iter().skip(1).map(String::as_str))
                .collect();
            repos.add(source, &spec.join(" "))?
        } else {
            repos.remove(&backend::Repo {
                name: name.to_string(),
                source: source.to_string(),
                ..backend::Repo::default()
            })?
        };
        let cmds = if args.yes { backend.assume_yes(cmds) } else { cmds };
        if cfg.dry_run {
            out(&cmds
                .iter()
                .map(|c| format!("{}\n", c.display(sudo.as_deref())))
                .collect::<String>());
            return Ok(());
        }
        let ok = run_commands(&cmds, sudo.as_deref(), missing);
        record_cli(
            &cfg,
            Draft::other(),
            &format!("{op} {}", names.join(" ")),
            &cmds,
            sudo.as_deref(),
            ok,
        );
        if !ok {
            bail!("{op} failed");
        }
        return Ok(());
    }
    if let Some((op, names)) = &args.op {
        let pkgs: Vec<Package> = names.iter().map(|n| backend::parse_target(&backend, n)).collect();
        for p in &pkgs {
            backend::validate_name(&p.name)?;
        }
        // What the history records (decided before anything changes).
        let draft = if cfg.history && !cfg.dry_run {
            pakdeck::history::cli_draft(&backend, op, &pkgs)
        } else {
            Draft::other()
        };
        let cmds = match op.as_str() {
            "hold" | "unhold" => {
                let (cmds, notes) = backend.hold_with_notes(&pkgs, op == "hold")?;
                notes.iter().for_each(|n| err(n));
                cmds
            }
            "install" => backend.install(&pkgs),
            "remove" => backend.remove(&pkgs),
            "upgrade" => backend.upgrade(&pkgs),
            "cleanup" => backend.cleanup()?,
            _ => backend.sync(),
        };
        warn(&backend);
        let cmds = if args.yes { backend.assume_yes(cmds) } else { cmds };
        if cfg.dry_run {
            out(&cmds
                .iter()
                .map(|c| format!("{}\n", c.display(sudo.as_deref())))
                .collect::<String>());
            return Ok(());
        }
        let ok = run_commands(&cmds, sudo.as_deref(), missing);
        let title = if names.is_empty() {
            op.clone()
        } else {
            format!("{op} {}", names.join(", "))
        };
        record_cli(&cfg, draft, &title, &cmds, sudo.as_deref(), ok);
        if !ok {
            bail!("{op} failed");
        }
        return Ok(());
    }
    // The theme picker saves its choice here.
    let config_path = args.config.clone().or_else(config::default_path);
    run_tui(cfg, backend, sudo, missing, config_path)
}

/// The listing and export modes. Returns true when one ran.
fn query_modes(args: &Args, cfg: &Config, backend: &backend::Multi) -> Result<bool> {
    if let Some(q) = &args.search {
        let mut found = backend.search(q)?;
        app::rank(&mut found, q);
        print_packages(&found);
        return Ok(true);
    }
    if let Some(file) = &args.export {
        let (text, notes) = pakdeck::export::export(backend)?;
        match file.as_deref() {
            Some("-") => out(&text),
            other => {
                let path = other.map(PathBuf::from).unwrap_or_else(|| cfg.export_file());
                if cfg.dry_run {
                    err(&format!("[dry-run] not writing {}", path.display()));
                } else {
                    pakdeck::util::write_atomic(&path, &text)?;
                    err(&format!("exported to {}", path.display()));
                }
            }
        }
        notes.iter().for_each(|n| err(n));
        return Ok(true);
    }
    if let Some(name) = &args.info {
        out(&backend.info(&backend::parse_target(backend, name))?);
        return Ok(true);
    }
    if args.repos {
        let Some(repos) = backend.repositories() else {
            bail!("no active package manager has repositories");
        };
        out(&repos
            .list()?
            .iter()
            .map(|r| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    r.source,
                    r.name,
                    r.packages,
                    if r.official { "official" } else { "third-party" },
                    r.remote
                )
            })
            .collect::<String>());
        return Ok(true);
    }
    if args.disk {
        let mut installed = backend.installed()?;
        // Measure sizes the package lists do not carry (Homebrew).
        backend.fill_sizes(&mut installed);
        let (totals, largest) = backend::disk_summary(&installed, 15);
        // One record per line, the kind first:
        // total<TAB>source<TAB>size<TAB>sized/packages
        // package<TAB>name<TAB>size<TAB>source
        let mut s = String::new();
        for t in &totals {
            s.push_str(&format!(
                "total\t{}\t{}\t{}/{}\n",
                t.source,
                backend::human_size(t.bytes),
                t.sized,
                t.packages
            ));
        }
        for p in &largest {
            s.push_str(&format!(
                "package\t{}\t{}\t{}\n",
                p.name,
                p.size.map(backend::human_size).unwrap_or_default(),
                p.source
            ));
        }
        out(&s);
        return Ok(true);
    }
    if let Some(name) = &args.versions {
        let pkg = backend::parse_target(backend, name);
        out(&backend
            .versions(&pkg)?
            .iter()
            // The id (what --install-version takes), then the version when
            // it differs (snap: channel, version).
            .map(|v| {
                if v.id == v.version {
                    format!("{}\n", v.id)
                } else {
                    format!("{}\t{}\n", v.id, v.version)
                }
            })
            .collect::<String>());
        return Ok(true);
    }
    if let Some(name) = &args.details {
        let pkg = backend::parse_target(backend, name);
        let mut s = String::new();
        for section in Section::ALL {
            s.push_str(&format!("{}:\n", section.title()));
            match backend.section(&pkg, section) {
                Ok(items) => items.iter().for_each(|i| s.push_str(&format!("  {i}\n"))),
                Err(e) if is_unsupported(&e) => s.push_str(&format!("  (not available: {e})\n")),
                Err(e) => s.push_str(&format!("  (error: {e:#})\n")),
            }
        }
        out(&s);
        return Ok(true);
    }
    if args.list {
        print_packages(&backend.installed()?);
        return Ok(true);
    }
    if args.explicit {
        out(&backend
            .explicit_tagged()?
            .iter()
            .map(|(s, n)| format!("{s}\t{n}\n"))
            .collect::<String>());
        return Ok(true);
    }
    if args.held {
        out(&backend
            .held_tagged()?
            .iter()
            .map(|(s, n)| format!("{s}\t{n}\n"))
            .collect::<String>());
        return Ok(true);
    }
    if args.orphans {
        print_packages(&backend.orphans()?);
        return Ok(true);
    }
    if args.updates {
        print_packages(&backend.updates()?);
        return Ok(true);
    }
    Ok(false)
}

/// The interactive TUI.
fn run_tui(
    cfg: Config,
    backend: Arc<backend::Multi>,
    sudo: Option<String>,
    no_elevation: bool,
    config_path: Option<std::path::PathBuf>,
) -> Result<()> {
    // In raw mode Ctrl-C is a key event; while a package manager runs in
    // cooked mode (suspend mode) it would also kill pakdeck, so ignore SIGINT.
    let _ = ctrlc::set_handler(|| {});

    let mouse = cfg.mouse;
    // theme = "auto": NO_COLOR means no colour; otherwise ask the terminal
    // for its background colour before the TUI takes over the screen
    // (terminals that do not answer get "dark").
    // Only "auto" asks the terminal (a terminal that does not answer
    // delays the start); with another theme set, the picker's "auto" entry
    // is decided at the next start.
    let no_color = pakdeck::theme::no_color();
    let asked = cfg.theme.preset == "auto" && !no_color;
    let light = asked
        && matches!(
            terminal_colorsaurus::theme_mode(terminal_colorsaurus::QueryOptions::default()),
            Ok(terminal_colorsaurus::ThemeMode::Light)
        );
    let auto_theme = pakdeck::theme::auto(no_color, light);
    let auto_known = asked || no_color;
    let theme = cfg.theme.preset.clone();
    let mut terminal = ratatui::try_init().context("cannot initialise the terminal")?;
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let mut app = App::new(backend, cfg, sudo);
    if no_elevation {
        app.warn_no_elevation();
    }
    app.truecolor = pakdeck::theme::truecolor();
    app.auto_theme = auto_theme.to_string();
    app.auto_known = auto_known;
    app.config_path = config_path;
    app.history_path = pakdeck::history::default_path();
    app.set_theme(&theme);
    let res = run(&mut terminal, app);
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    res
}

fn run(terminal: &mut DefaultTerminal, mut app: App) -> Result<()> {
    loop {
        app.tick();
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if app.quit {
            if let Some(job) = app.job.as_mut() {
                job.kill();
            }
            return Ok(());
        }
        if let Some(cmds) = app.run_request.take() {
            let ok = suspend_and_run(terminal, &cmds, app.sudo.as_deref(), app.cfg.mouse)?;
            app.after_run(ok);
            continue;
        }
        // Redraw often while a command streams output into the pane.
        let wait = if app.job.is_some() { 30 } else { 100 };
        if event::poll(Duration::from_millis(wait))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
        }
    }
}

/// Leave the TUI, run the commands with the real terminal (so sudo and the
/// package manager can prompt), then return.
fn suspend_and_run(terminal: &mut DefaultTerminal, cmds: &[Cmd], sudo: Option<&str>, mouse: bool) -> Result<bool> {
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    let _ = execute!(std::io::stdout(), cursor::Show);
    // The TUI already warned at startup.
    let ok = run_commands(cmds, sudo, false);
    out("\nPress Enter to return to pakdeck...");
    let _ = std::io::stdin().read_line(&mut String::new());

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    terminal.clear()?;
    Ok(ok)
}

/// Run commands in sequence with inherited stdio; stop at the first failure.
/// Show `shown` and ask before running, unless `--yes`; refuse when there
/// is no terminal to ask on.
fn ask_first(shown: &str, yes: bool, flag: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("{flag} needs --yes when not run from a terminal (use -n to see the plan)");
    }
    out(&format!("{shown}\nRun these commands? [y/N] "));
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("cancelled");
    }
    Ok(())
}

/// Add a finished command-line run to the history (`history = true`).
fn record_cli(cfg: &Config, draft: Draft, title: &str, cmds: &[Cmd], sudo: Option<&str>, ok: bool) {
    if !cfg.history || cmds.is_empty() {
        return;
    }
    let Some(path) = pakdeck::history::default_path() else {
        return;
    };
    let entry = pakdeck::history::Entry::new(draft, title, cmds, sudo, ok);
    if let Err(e) = pakdeck::history::append(&path, &entry) {
        err(&format!("pakdeck: history not saved: {e:#}"));
    }
}

fn run_commands(cmds: &[Cmd], sudo: Option<&str>, no_elevation: bool) -> bool {
    if no_elevation && cmds.iter().any(|c| c.root) {
        let [what, fix] = pakdeck::app::NO_ELEVATION;
        err(&format!("pakdeck: warning: {what}; {fix}"));
    }
    for c in cmds {
        out(&format!("\n\x1b[1;36m$ {}\x1b[0m\n", c.display(sudo)));
        let argv = backend::resolve_argv(c.argv(sudo));
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        match Command::new(program).args(args).env("ZYPP_LOCK_TIMEOUT", "60").status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                out(&format!("\x1b[31mcommand failed ({s})\x1b[0m\n"));
                return false;
            }
            Err(e) => {
                out(&format!("\x1b[31mfailed to start {program}: {e}\x1b[0m\n"));
                return false;
            }
        }
    }
    true
}
