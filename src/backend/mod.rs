//! Package manager backends. Each backend knows how to query its package
//! database (read-only, run in background threads) and how to build the
//! commands for mutating operations (run interactively by the UI).

mod apk;
mod apt;
mod aur;
mod brew;
mod disk;
mod dnf;
mod flatpak;
mod multi;
mod nix;
mod pacman;
mod pkg;
mod rpm;
mod scoop;
mod snap;
mod winget;
mod xbps;
mod zypper;

use anyhow::{Context, Result, bail};
use std::fmt;
use std::process::{Command, Stdio};
use std::sync::Arc;

pub use disk::{DiskUsage, apply_sizes, disk_summary};
pub use multi::{Multi, parse_target, split_source};

/// Extra package details, each loaded lazily in the background.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Size,
    Depends,
    RequiredBy,
    Files,
}

impl Section {
    pub const ALL: [Section; 4] = [Section::Size, Section::Depends, Section::RequiredBy, Section::Files];
    pub fn title(self) -> &'static str {
        match self {
            Section::Size => "Size",
            Section::Depends => "Depends on",
            Section::RequiredBy => "Required by",
            Section::Files => "Files",
        }
    }
}

/// Error marker: the package manager has no way to do this. The UI shows
/// it as an informational message rather than a failure.
#[derive(Debug)]
pub struct Unsupported(pub String);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

pub fn unsupported<T>(what: impl Into<String>) -> Result<T> {
    Err(Unsupported(what.into()).into())
}

pub fn is_unsupported(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Unsupported>().is_some()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// Available version, set for entries in the update list.
    pub new_version: Option<String>,
    pub description: String,
    /// Repository, or for Homebrew the kind ("formula" / "cask").
    pub repo: String,
    pub installed: bool,
    /// The package manager this package belongs to (set by `Multi`).
    pub source: String,
    /// Installed size in bytes, where the list query provides it cheaply.
    pub size: Option<u64>,
    /// Install time (seconds since the Unix epoch), where cheap to get.
    pub installed_at: Option<u64>,
    /// A short remark shown next to the version ("updates itself").
    pub note: Option<String>,
}

impl Package {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            ..Default::default()
        }
    }
    pub fn note(mut self, n: impl Into<String>) -> Self {
        self.note = Some(n.into());
        self
    }
    pub fn desc(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }
    pub fn repo(mut self, r: impl Into<String>) -> Self {
        self.repo = r.into();
        self
    }
    pub fn upgrade_to(mut self, v: impl Into<String>) -> Self {
        self.new_version = Some(v.into());
        self
    }
    pub fn sized(mut self, bytes: Option<u64>) -> Self {
        self.size = bytes;
        self
    }
    pub fn installed_at(mut self, secs: Option<u64>) -> Self {
        self.installed_at = secs;
        self
    }
    pub fn source(mut self, s: impl Into<String>) -> Self {
        self.source = s.into();
        self
    }
    /// Unique key used for caching details and marks.
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.source, self.repo, self.name)
    }
    /// (source, name): identifies an installed package across backends.
    pub fn id(&self) -> (String, String) {
        (self.source.clone(), self.name.clone())
    }
}

/// A command to run interactively. `root` commands get the privilege
/// prefix (sudo/doas) prepended unless already running as root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub root: bool,
}

impl Cmd {
    pub fn user(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            root: false,
        }
    }
    pub fn root(program: &str, args: &[&str]) -> Self {
        Self {
            root: true,
            ..Self::user(program, args)
        }
    }
    pub fn names(mut self, pkgs: &[Package]) -> Self {
        self.args.extend(pkgs.iter().map(|p| p.name.clone()));
        self
    }
    /// Insert `flag` before argument `index` (clamped to the end).
    pub fn flag_at(mut self, index: usize, flag: &str) -> Self {
        let i = index.min(self.args.len());
        self.args.insert(i, flag.to_string());
        self
    }
    pub fn strings(mut self, extra: impl IntoIterator<Item = String>) -> Self {
        self.args.extend(extra);
        self
    }
    pub fn argv(&self, sudo: Option<&str>) -> Vec<String> {
        let mut v = Vec::with_capacity(self.args.len().saturating_add(2));
        if self.root
            && let Some(s) = sudo
        {
            v.push(s.to_string());
        }
        v.push(self.program.clone());
        v.extend(self.args.iter().cloned());
        v
    }
    pub fn display(&self, sudo: Option<&str>) -> String {
        self.argv(sudo)
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=+@,%".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    fn search(&self, query: &str) -> Result<Vec<Package>>;
    fn installed(&self) -> Result<Vec<Package>>;
    fn updates(&self) -> Result<Vec<Package>>;
    fn info(&self, pkg: &Package) -> Result<String>;
    fn install(&self, pkgs: &[Package]) -> Vec<Cmd>;
    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd>;
    /// Upgrade the given packages, or everything when `pkgs` is empty.
    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd>;
    fn cleanup(&self) -> Result<Vec<Cmd>>;
    /// Refresh repository metadata.
    fn sync(&self) -> Vec<Cmd>;

    /// Names of packages the user asked for (not pulled in as dependencies).
    fn explicit(&self) -> Result<Vec<String>> {
        unsupported(format!(
            "{} cannot tell explicitly installed packages apart",
            self.name()
        ))
    }
    /// Installed dependencies nothing needs any more.
    fn orphans(&self) -> Result<Vec<Package>> {
        unsupported(format!("{} has no orphaned-package query", self.name()))
    }
    /// The same commands, made to answer "yes" to every prompt (`--yes`
    /// for unattended use). Managers that never prompt keep them as is.
    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds
    }
    /// Versions of a package that can be installed, newest first.
    fn versions(&self, _pkg: &Package) -> Result<Vec<VersionChoice>> {
        unsupported(format!("{} cannot list versions of a package", self.name()))
    }
    /// Commands that install exactly `version` (up- or downgrading).
    fn install_version(&self, _pkg: &Package, _version: &str) -> Result<Vec<Cmd>> {
        unsupported(format!("{} cannot install a specific version", self.name()))
    }
    /// Whether `install_version` is implemented (checked without running
    /// anything): undo of an update needs it.
    fn installs_versions(&self) -> bool {
        false
    }
    /// Repositories this manager can list, add and remove (Homebrew taps).
    /// One accessor instead of flags beside default methods: a manager has
    /// all of these operations or none.
    fn repositories(&self) -> Option<&dyn Repositories> {
        None
    }
    /// Measuring installed sizes that the package list does not carry
    /// (Homebrew), or `None`.
    fn size_meter(&self) -> Option<&dyn SizeMeter> {
        None
    }
    /// Other package managers whose *installed* entries for the same package
    /// names this one supersedes (the AUR claims the foreign packages pacman
    /// lists). Applies to installed, orphans, explicit and held; never to
    /// search or updates, where both sources stay visible.
    fn overrides(&self) -> &'static [&'static str] {
        &[]
    }
    /// Names of packages held back from updates (pinned / locked / held).
    fn held(&self) -> Result<Vec<String>> {
        unsupported(format!("{} cannot hold packages", self.name()))
    }
    /// Commands to hold (`on`) or release packages.
    fn hold(&self, _pkgs: &[Package], _on: bool) -> Result<Vec<Cmd>> {
        unsupported(format!("{} cannot hold packages", self.name()))
    }
    /// How a package is written to an export file. Managers that need more
    /// than the name to reinstall (flatpak's remote, snap's confinement)
    /// encode it here and decode it in `import_package`.
    fn export_name(&self, pkg: &Package) -> String {
        pkg.name.clone()
    }
    fn import_package(&self, entry: &str) -> Package {
        Package::new(entry, "")
    }
    /// A warning shown when undo takes some of this manager's packages back
    /// to earlier versions (Arch supports only whole-system states).
    fn rollback_note(&self) -> Option<&'static str> {
        None
    }
    /// What the Updates tab says while loading, when the query may take
    /// longer than a list (Homebrew refreshes a stale catalogue first).
    fn updates_note(&self) -> Option<String> {
        None
    }
    /// Whether any command of this manager runs as root (with sudo or
    /// doas). Homebrew, nix profiles, AUR helpers, winget and scoop run as
    /// the user, so there is nothing to elevate.
    fn needs_root(&self) -> bool {
        true
    }
    /// Does `cmd` belong to this package manager? Used to route `--yes`.
    fn owns(&self, cmd: &Cmd) -> bool {
        cmd.program == self.name()
    }
    /// One detail section for a package (one line for `Size`).
    fn section(&self, _pkg: &Package, s: Section) -> Result<Vec<String>> {
        unsupported(format!("{}: '{}' is not available", self.name(), s.title()))
    }
}

/// Run a command non-interactively and capture stdout. Exit codes listed in
/// `ok_codes` are treated as success in addition to 0.
pub fn capture(program: &str, args: &[&str], ok_codes: &[i32]) -> Result<String> {
    capture_env(program, args, ok_codes, &[], &[])
}

/// `capture` with environment changes on top of the defaults below:
/// variables to set, then variables to remove (a removal undoes a default).
pub fn capture_env(
    program: &str,
    args: &[&str],
    ok_codes: &[i32],
    set: &[(&str, String)],
    remove: &[&str],
) -> Result<String> {
    let mut argv: Vec<String> = std::iter::once(program)
        .chain(args.iter().copied())
        .map(String::from)
        .collect();
    argv = resolve_argv(argv);
    let Some((prog, rest)) = argv.split_first() else {
        bail!("empty command")
    };
    let mut cmd = Command::new(prog);
    cmd.args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If another program holds zypper's lock, wait for it instead of failing.
        .env("ZYPP_LOCK_TIMEOUT", "60")
        // Homebrew never refreshes its catalogue during a query, unless
        // the caller removes this (brew's Updates query does).
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1");
    for (name, value) in set {
        cmd.env(name, value);
    }
    for name in remove {
        cmd.env_remove(name);
    }
    if cfg!(target_os = "linux") {
        cmd.env("LC_ALL", "C.UTF-8");
    }
    let out = cmd.output().with_context(|| format!("failed to start {program}"))?;
    let code = out.status.code().unwrap_or(-1);
    if out.status.success() || ok_codes.contains(&code) {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err: String = err.split_whitespace().collect::<Vec<_>>().join(" ");
    let err: String = err.chars().take(300).collect();
    bail!(
        "{program} {} exited with code {code}: {err}",
        args.first().unwrap_or(&"")
    )
}

pub fn run(program: &str, args: &[&str]) -> Result<String> {
    capture(program, args, &[])
}

/// Full path of an executable `bin` in PATH (honouring PATHEXT on Windows).
pub fn which(bin: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let exts: Vec<String> = std::iter::once(String::new())
        .chain(
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
                .split(';')
                .map(|e| e.to_ascii_lowercase()),
        )
        .collect();
    #[cfg(not(windows))]
    let exts = [String::new()];
    for dir in std::env::split_paths(&paths) {
        for ext in &exts {
            let p = dir.join(format!("{bin}{ext}"));
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
}

/// Is `bin` an executable somewhere in PATH?
pub fn has(bin: &str) -> bool {
    which(bin).is_some()
}

/// On Windows, batch-file shims (scoop's `scoop.cmd`) cannot be started
/// directly; run them through `cmd /C`. Elsewhere argv is unchanged.
pub fn resolve_argv(argv: Vec<String>) -> Vec<String> {
    if cfg!(windows)
        && let Some(path) = argv.first().and_then(|p| which(p))
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
    {
        return ["cmd".to_string(), "/C".to_string()].into_iter().chain(argv).collect();
    }
    argv
}

pub const BACKENDS: &[&str] = &[
    "apt", "dnf", "yum", "zypper", "pacman", "aur", "apk", "xbps", "pkg", "nix", "brew", "flatpak", "snap", "winget",
    "scoop",
];

/// Executable that shows a package manager is installed.
fn probe(name: &str) -> &str {
    match name {
        "apt" => "apt-get",
        "xbps" => "xbps-install",
        // "aur" is opt-in only (see `opt_in_only`), so never probed.
        other => other,
    }
}

/// Is this package manager one that exists on the current OS? An unrelated
/// program with the same name (a `pkg` on macOS, say) must not count.
fn native(name: &str) -> bool {
    match name {
        "pkg" => cfg!(target_os = "freebsd"),
        "winget" | "scoop" => cfg!(windows),
        "nix" | "brew" => cfg!(unix),
        _ => cfg!(target_os = "linux"),
    }
}

/// Package managers that are used only when named in `backends`, never
/// through detection or `["all"]`: the AUR is unreviewed user content.
pub fn opt_in_only(name: &str) -> bool {
    name == "aur"
}

/// Every supported package manager present on this machine: the system
/// one first, then the rest (nix, brew, flatpak, snap, ...).
pub fn detect_all() -> Result<Vec<Arc<dyn Backend>>> {
    let primary = detect(None)?;
    let first = primary.name();
    let mut v = vec![primary];
    for name in BACKENDS {
        // yum is dnf's predecessor; never both.
        let dup = *name == first || (*name == "yum" && first == "dnf");
        if !dup && !opt_in_only(name) && native(name) && has(probe(name)) {
            v.push(by_name(name)?);
        }
    }
    Ok(v)
}

/// Resolve the configured backends: a list of names, `["all"]`, or (empty)
/// the single detected one.
pub fn resolve(list: &[String], single: Option<&str>) -> Result<Multi> {
    if list.iter().any(|n| n == "all") {
        return Ok(Multi::new(detect_all()?));
    }
    if !list.is_empty() {
        return Ok(Multi::new(list.iter().map(|n| by_name(n)).collect::<Result<Vec<_>>>()?));
    }
    Ok(Multi::new(vec![detect(single)?]))
}

pub fn by_name(name: &str) -> Result<Arc<dyn Backend>> {
    Ok(match name {
        "apt" => Arc::new(apt::Apt),
        "dnf" | "yum" => Arc::new(dnf::Dnf::new(name)),
        "pacman" => Arc::new(pacman::Pacman),
        "apk" => Arc::new(apk::Apk),
        "brew" | "homebrew" => Arc::new(brew::Brew),
        "zypper" => Arc::new(zypper::Zypper),
        "xbps" => Arc::new(xbps::Xbps),
        "nix" => Arc::new(nix::Nix),
        "pkg" => Arc::new(pkg::Pkg),
        "winget" => Arc::new(winget::Winget),
        "scoop" => Arc::new(scoop::Scoop),
        "flatpak" => Arc::new(flatpak::Flatpak),
        "aur" => Arc::new(aur::Aur::detect()?),
        "snap" => Arc::new(snap::Snap),
        other => bail!("unknown backend '{other}' (expected one of: {})", BACKENDS.join(", ")),
    })
}

/// Pick a backend: the preferred one if given, otherwise the first package
/// manager found in PATH.
pub fn detect(preferred: Option<&str>) -> Result<Arc<dyn Backend>> {
    if let Some(p) = preferred {
        return by_name(p);
    }
    let order: &[(&str, &str)] = if cfg!(target_os = "macos") {
        &[("brew", "brew"), ("nix", "nix")]
    } else if cfg!(target_os = "freebsd") {
        &[("pkg", "pkg")]
    } else if cfg!(windows) {
        &[("winget", "winget"), ("scoop", "scoop")]
    } else {
        &[
            ("apt-get", "apt"),
            ("dnf", "dnf"),
            ("yum", "yum"),
            ("zypper", "zypper"),
            ("pacman", "pacman"),
            ("apk", "apk"),
            ("xbps-install", "xbps"),
            ("nix", "nix"),
            ("brew", "brew"),
        ]
    };
    for (bin, name) in order {
        if has(bin) {
            return by_name(name);
        }
    }
    bail!("no supported package manager found in PATH ({})", BACKENDS.join(", "))
}

/// Run every output parser on arbitrary input (fuzzing entry point).
/// Parsers must never panic, whatever a package manager prints.
#[doc(hidden)]
pub fn fuzz_parsers(input: &str) {
    let _ = apt::parse_search(input);
    let _ = apt::parse_installed(input);
    let _ = apt::parse_upgradable(input);
    let _ = dnf::parse_tabbed(input);
    let _ = dnf::parse_check_update(input);
    let _ = pacman::parse_search(input);
    let _ = pacman::parse_updates(input);
    let _ = apk::parse_search(input);
    let _ = apk::parse_updates(input);
    let _ = brew::parse_search(input, "formula");
    let _ = brew::parse_installed(input);
    let _ = brew::parse_outdated(input);
    let _ = apt::parse_autoremove(input);
    let _ = apt::parse_depends(input);
    let _ = apt::parse_rdepends(input);
    let _ = pacman::parse_qd(input);
    let _ = apk::parse_list(input);
    let _ = apk::parse_world(input);
    let _ = brew::parse_explicit(input);
    let _ = brew::parse_autoremove(input);
    let _ = brew::parse_size(input);
    let _ = rpm::parse_requires(input);
    let _ = zypper::parse_search(input);
    let _ = zypper::parse_updates(input);
    let _ = zypper::parse_user_installed(input);
    let _ = zypper::parse_unneeded(input);
    let _ = xbps::parse_search(input);
    let _ = xbps::parse_installed(input);
    let _ = xbps::parse_updates(input);
    let _ = xbps::parse_pkgvers(input);
    let _ = xbps::parse_deps(input);
    let _ = pkg::parse_search(input);
    let _ = pkg::parse_updates(input);
    let _ = nix::parse_search(input);
    let _ = nix::parse_profile(input);
    let _ = nix::parse_meta(input);
    let _ = nix::store_name_version(input);
    let _ = winget::parse(input);
    let _ = scoop::parse_search(input);
    let _ = scoop::parse_list(input);
    let _ = scoop::parse_status(input);
    let _ = flatpak::parse_search(input);
    let _ = flatpak::parse_updates(input);
    let _ = flatpak::parse_info(input);
    let _ = flatpak::parse_masks(input);
    let _ = snap::parse_find(input);
    let _ = snap::parse_list(input);
    let _ = snap::parse_disabled(input);
    let _ = dnf::parse_versionlock(input);
    let _ = pacman::parse_ignore(input);
    let _ = apk::parse_world_held(input);
    let _ = pkg::parse_locked(input);
    let _ = aligned_table(input);
    let _ = parse_sized(input);
    let _ = brew::parse_du(input);
    let _ = brew::parse_cask_targets(input);
    let _ = rpm::provider_names(input, "x");
    let _ = brew::parse_taps(input);
    let _ = aur::aur_depends(input);
    let _ = apt::parse_madison(input);
    let _ = pacman::parse_cache_file(input);
    let _ = snap::parse_channels(input);
    let _ = winget::parse_versions(input);
    let _ = vercmp(input, "1.0");
    let words: Vec<String> = input.split_whitespace().take(64).map(String::from).collect();
    for (a, b) in words.iter().zip(words.iter().skip(1)) {
        assert_eq!(
            vercmp(a, b),
            vercmp(b, a).reverse(),
            "vercmp not antisymmetric: {a:?} {b:?}"
        );
    }
    let _ = sort_versions(words);
    let _ = parse_size(input);
    let _ = fields(input);
    let _ = pipe_table(input);
    let _ = fixed_table(input);
    let _ = split_name_version(input);
    for line in input.lines() {
        let _ = split_name_version(line);
        let _ = human_size((line.len() as u64).saturating_mul(977));
    }
}

static NULL: serde_json::Value = serde_json::Value::Null;

/// `v[key]` without the panicking `Index` impl: missing keys give `Null`.
pub fn jget<'a>(v: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    v.get(key).unwrap_or(&NULL)
}

/// Repositories a package manager can list, add and remove.
pub trait Repositories: Send + Sync {
    /// What they are called ("Taps").
    fn kind(&self) -> &'static str;
    /// How to write one for `add` ("user/repo, optionally followed by a
    /// git URL").
    fn add_hint(&self) -> &'static str;
    /// Every repository, with `source` set to the owning manager.
    fn list(&self) -> Result<Vec<Repo>>;
    /// Commands that add `spec` (as typed: a name, optionally a URL) to the
    /// manager named `source`; empty means the first that has repositories.
    /// A single manager ignores `source`.
    fn add(&self, source: &str, spec: &str) -> Result<Vec<Cmd>>;
    /// Commands that remove `repo`.
    fn remove(&self, repo: &Repo) -> Result<Vec<Cmd>>;
    /// Does installed `pkg` come from `repo`?
    fn contains(&self, repo: &Repo, pkg: &Package) -> bool;
}

/// Measuring installed sizes on demand. Slow (it walks the disk): call it
/// in the background.
pub trait SizeMeter: Send + Sync {
    /// Can packages of the manager named `source` be measured?
    fn measures(&self, source: &str) -> bool;
    /// Sizes of those `pkgs` that could be measured, as
    /// (`Package::key()`, bytes). Packages it cannot size are left out.
    fn measure(&self, pkgs: &[Package]) -> Result<Vec<(String, u64)>>;
}

/// A package repository a manager can add and remove (Homebrew taps).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Repo {
    pub name: String,
    /// Maintained by the package manager's project itself.
    pub official: bool,
    /// Number of packages the repository offers.
    pub packages: usize,
    /// Where it comes from (a git URL), if known.
    pub remote: String,
    /// The package manager it belongs to (set by `Multi`).
    pub source: String,
}

/// One installable version. `id` is what `install_version` takes (a
/// version, or a snap channel); `version` is the package version it gives,
/// used to mark the installed one and to hold at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionChoice {
    pub id: String,
    pub version: String,
}

impl VersionChoice {
    pub fn plain(v: impl Into<String>) -> Self {
        let v = v.into();
        Self {
            id: v.clone(),
            version: v,
        }
    }
    /// What the picker shows: the id, plus the version when they differ.
    pub fn label(&self) -> String {
        if self.id == self.version || self.version.is_empty() {
            self.id.clone()
        } else {
            format!("{} ({})", self.id, self.version)
        }
    }
}

/// Versions newest first, without duplicates.
pub fn plain_versions(v: Vec<String>) -> Vec<VersionChoice> {
    sort_versions(v).into_iter().map(VersionChoice::plain).collect()
}

/// A version (or channel) that goes into a command run as root: version
/// characters only, so no globs (`*`), relations (`<`, `=`) or options.
pub fn validate_version(v: &str) -> Result<()> {
    let ok =
        !v.is_empty() && !v.starts_with('-') && v.chars().all(|c| c.is_ascii_alphanumeric() || ".:+~-_^@/".contains(c));
    if ok { Ok(()) } else { bail!("invalid version {v:?}") }
}

/// The commands for "install exactly this version" (TUI picker and CLI):
/// release an existing hold first (the package managers refuse to change a
/// held package), install the version, then hold at it when asked.
/// `pkg.installed` / `pkg.version` must describe the installed package.
pub fn version_plan(
    b: &dyn Backend,
    pkg: &Package,
    choice: &VersionChoice,
    held_now: bool,
    hold: bool,
) -> Result<(Vec<Cmd>, Vec<String>)> {
    validate_name(&pkg.name)?;
    validate_version(&choice.id)?;
    let mut cmds = Vec::new();
    let mut notes = Vec::new();
    if held_now {
        cmds.extend(b.hold(std::slice::from_ref(pkg), false)?);
        notes.push(format!(
            "{} is held: the hold is released first (if the install fails, it stays released)",
            pkg.name
        ));
    }
    cmds.extend(b.install_version(pkg, &choice.id)?);
    if pkg.installed && vercmp(&choice.version, &pkg.version) == std::cmp::Ordering::Less {
        notes.push("downgrade: packages that require the newer version are not changed".into());
    }
    if hold {
        let at = Package {
            version: choice.version.clone(),
            ..pkg.clone()
        };
        match b.hold(std::slice::from_ref(&at), true) {
            Ok(c) => cmds.extend(c),
            Err(e) => notes.push(format!("not held: {e:#}")),
        }
    }
    Ok((cmds, notes))
}

/// Compare two version strings the way package managers broadly do:
/// split into runs of digits and of letters, compare digit runs as numbers
/// and letter runs as text; an epoch ("1:") and separators are respected.
/// `~` sorts before anything (Debian/rpm pre-releases).
pub fn vercmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ea, ra) = a
        .split_once(':')
        .filter(|(e, _)| e.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(("0", a));
    let (eb, rb) = b
        .split_once(':')
        .filter(|(e, _)| e.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(("0", b));
    let num = |s: &str| s.trim_start_matches('0').to_string();
    let cmp_num = |x: &str, y: &str| {
        let (x, y) = (num(x), num(y));
        x.len().cmp(&y.len()).then_with(|| x.cmp(&y))
    };
    match cmp_num(ea, eb) {
        Ordering::Equal => {}
        o => return o,
    }
    let mut x = ra.chars().peekable();
    let mut y = rb.chars().peekable();
    loop {
        // Tilde: earlier than everything, even the end of the string.
        match (x.peek() == Some(&'~'), y.peek() == Some(&'~')) {
            (true, true) => {
                x.next();
                y.next();
                continue;
            }
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        while x.peek().is_some_and(|c| !c.is_ascii_alphanumeric() && *c != '~') {
            x.next();
        }
        while y.peek().is_some_and(|c| !c.is_ascii_alphanumeric() && *c != '~') {
            y.next();
        }
        match (x.peek(), y.peek()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            _ => {}
        }
        let dx = x.peek().is_some_and(char::is_ascii_digit);
        let dy = y.peek().is_some_and(char::is_ascii_digit);
        // A digit run is newer than a letter run (as rpm decides), and this
        // must hold in both directions for sorting to be a total order.
        if dx != dy {
            return if dx { Ordering::Greater } else { Ordering::Less };
        }
        let digits = dx;
        let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>, d: bool| -> String {
            let mut s = String::new();
            while let Some(c) = it
                .peek()
                .copied()
                .filter(|c| if d { c.is_ascii_digit() } else { c.is_ascii_alphabetic() })
            {
                s.push(c);
                it.next();
            }
            s
        };
        let sx = take(&mut x, digits);
        let sy = take(&mut y, digits);
        let o = if digits { cmp_num(&sx, &sy) } else { sx.cmp(&sy) };
        if o != Ordering::Equal {
            return o;
        }
    }
}

/// Newest first, without duplicates.
pub fn sort_versions(mut v: Vec<String>) -> Vec<String> {
    v.sort_by(|a, b| vercmp(b, a));
    v.dedup();
    v
}

/// Modification time of a path as seconds since the Unix epoch.
pub fn mtime(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// "1.3 MB", "6.8MB", "175.6KB", "67 KiB" -> bytes (approximate: decimal
/// and binary units are both read as powers of 1024, and localised decimal
/// commas give None; the result is only used for sorting).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at_checked(split)?;
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "bytes" | "" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = n * mult;
    (bytes.is_finite() && bytes >= 0.0).then_some(bytes as u64)
}

/// Reject names that could be read as options or split into several
/// arguments: they come from import files and the command line and are
/// passed to package managers that run as root.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("empty package name");
    }
    if name.starts_with('-') {
        bail!("package name {name:?} starts with '-' (it would be read as an option)");
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("package name {name:?} contains whitespace or control characters");
    }
    if name.chars().count() > 256 {
        bail!("package name too long");
    }
    Ok(())
}

/// Up to `limit` file paths below `dir` (for package managers that install
/// into a directory of their own: nix store paths, snaps, flatpaks).
pub fn walk_files(dir: &std::path::Path, limit: usize) -> Vec<String> {
    fn go(dir: &std::path::Path, out: &mut Vec<String>, limit: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            if out.len() >= limit {
                return;
            }
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => go(&p, out, limit),
                _ => out.push(p.display().to_string()),
            }
        }
    }
    let mut out = Vec::new();
    go(dir, &mut out, limit);
    out
}

/// A table aligned with spaces under a one-line header (snap): columns
/// start where header words start. Rows are cut at those positions.
pub fn aligned_table(out: &str) -> Vec<Vec<(String, String)>> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else { return Vec::new() };
    let header: Vec<char> = header.chars().collect();
    let mut starts = Vec::new();
    let mut prev_space = true;
    for (i, c) in header.iter().enumerate() {
        if *c != ' ' && prev_space {
            starts.push(i);
        }
        prev_space = *c == ' ';
    }
    let cell = |cs: &[char], i: usize| -> String {
        let a = starts.get(i).copied().unwrap_or(cs.len()).min(cs.len());
        let b = starts
            .get(i.saturating_add(1))
            .copied()
            .unwrap_or(cs.len())
            .min(cs.len())
            .max(a);
        cs.get(a..b)
            .unwrap_or_default()
            .iter()
            .collect::<String>()
            .trim()
            .to_string()
    };
    let names: Vec<String> = (0..starts.len()).map(|i| cell(&header, i)).collect();
    lines
        .map(|l| {
            let cs: Vec<char> = l.chars().collect();
            names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), cell(&cs, i)))
                .collect()
        })
        .collect()
}

/// Package file lists (dpkg, rpm) include directories; keep only files.
pub fn files_only(paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .filter(|p| p != "/." && !std::path::Path::new(p).is_dir())
        .collect()
}

/// Non-empty trimmed lines, deduplicated, in order.
pub fn lines(out: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && seen.insert(l.to_string()))
        .map(String::from)
        .collect()
}

/// "Key : value" blocks (pacman, zypper, dnf info), with indented
/// continuation lines appended to the previous value.
pub fn fields(text: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(last) = v.last_mut() {
                let t = line.trim();
                if !t.is_empty() {
                    if !last.1.is_empty() {
                        last.1.push('\n');
                    }
                    last.1.push_str(t);
                }
            }
            continue;
        }
        if let Some((k, val)) = line.split_once(':') {
            v.push((k.trim().to_string(), val.trim().to_string()));
        }
    }
    v
}

pub fn field<'a>(f: &'a [(String, String)], key: &str) -> Option<&'a str> {
    f.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// A `|`-separated table (zypper): rows as (header, cell) pairs.
pub fn pipe_table(out: &str) -> Vec<Vec<(String, String)>> {
    let mut header: Option<Vec<String>> = None;
    let mut rows = Vec::new();
    for line in out.lines() {
        if !line.contains('|') {
            continue;
        }
        if line.chars().all(|c| matches!(c, '-' | '+' | '|' | ' ')) {
            continue;
        }
        let cells: Vec<String> = line.split('|').map(|c| c.trim().to_string()).collect();
        match &header {
            None => header = Some(cells),
            Some(h) => rows.push(h.iter().cloned().zip(cells).collect()),
        }
    }
    rows
}

/// A fixed-width table with a dashed rule under the header (winget, scoop).
/// Column starts come from the rule's dash runs, or from the header words
/// when the rule is one unbroken line. Rows end at the first blank line.
pub fn fixed_table(out: &str) -> Vec<Vec<(String, String)>> {
    // Progress spinners use carriage returns; keep what follows the last one.
    let text: Vec<&str> = out.lines().map(|l| l.rsplit('\r').next().unwrap_or(l)).collect();
    let is_rule = |l: &str| l.contains("--") && l.trim().chars().all(|c| c == '-' || c == ' ');
    let Some(r) = text.iter().position(|l| is_rule(l)) else {
        return Vec::new();
    };
    let (Some(header), Some(rule)) = (r.checked_sub(1).and_then(|h| text.get(h)), text.get(r)) else {
        return Vec::new();
    };
    let header: Vec<char> = header.chars().collect();
    let rule: Vec<char> = rule.chars().collect();
    // Indices where a run of characters matching `pred` begins.
    let starts_of = |cs: &[char], pred: &dyn Fn(char) -> bool| -> Vec<usize> {
        let mut prev = false;
        let mut v = Vec::new();
        for (i, &c) in cs.iter().enumerate() {
            let now = pred(c);
            if now && !prev {
                v.push(i);
            }
            prev = now;
        }
        v
    };
    let mut starts = starts_of(&rule, &|c| c == '-');
    if starts.len() < 2 {
        starts = starts_of(&header, &|c| c != ' ');
    }
    let cell = |cs: &[char], i: usize| -> String {
        let a = starts.get(i).copied().unwrap_or(cs.len()).min(cs.len());
        let b = starts
            .get(i.saturating_add(1))
            .copied()
            .unwrap_or(cs.len())
            .min(cs.len())
            .max(a);
        cs.get(a..b)
            .unwrap_or_default()
            .iter()
            .collect::<String>()
            .trim()
            .to_string()
    };
    let names: Vec<String> = (0..starts.len()).map(|i| cell(&header, i)).collect();
    text.iter()
        .skip(r.saturating_add(1))
        .take_while(|l| !l.trim().is_empty())
        .map(|l| {
            let cs: Vec<char> = l.chars().collect();
            names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), cell(&cs, i)))
                .collect()
        })
        .collect()
}

pub fn col<'a>(row: &'a [(String, String)], name: &str) -> &'a str {
    row.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

/// Bytes as "12.3 MiB".
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len().saturating_sub(1) {
        v /= 1024.0;
        u = u.saturating_add(1);
    }
    match UNITS.get(u) {
        Some(unit) if u > 0 => format!("{v:.1} {unit}"),
        _ => format!("{bytes} B"),
    }
}

/// Split "name-1.2.3-r0" into ("name", "1.2.3-r0") (Alpine style).
pub fn split_name_version(s: &str) -> Option<(&str, &str)> {
    let rel = s.rfind('-')?;
    let ver = s.get(..rel)?.rfind('-')?;
    let (name, rest) = s.split_at_checked(ver)?;
    Some((name, rest.get(1..)?))
}

/// "name\tversion\tsize\ttime\tsummary" lines (rpm, FreeBSD pkg):
/// size in bytes, time in seconds since the epoch; deduplicated by name.
pub(crate) fn parse_sized(out: &str) -> Vec<Package> {
    let mut seen = std::collections::HashSet::new();
    out.lines()
        .filter_map(|l| {
            let mut f = l.splitn(5, '\t');
            let (name, ver) = (f.next()?.trim(), f.next()?.trim());
            let size = f.next().and_then(|s| s.trim().parse::<u64>().ok());
            let time = f.next().and_then(|s| s.trim().parse::<u64>().ok());
            if name.is_empty() || !seen.insert(name.to_string()) {
                return None;
            }
            Some(
                Package::new(name, ver)
                    .sized(size)
                    .installed_at(time)
                    .desc(f.next().unwrap_or("").trim()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        let c = Cmd::root("apt-get", &["install"]).strings(["vim".into(), "it's".into()]);
        assert_eq!(c.display(Some("sudo")), r"sudo apt-get install vim 'it'\''s'");
        assert_eq!(c.display(None), r"apt-get install vim 'it'\''s'");
        assert_eq!(Cmd::user("brew", &["upgrade"]).display(Some("sudo")), "brew upgrade");
    }

    #[test]
    fn assume_yes_flags() {
        let p = [Package::new("tree", "")];
        let yes = |b: &str, f: &dyn Fn(&dyn Backend) -> Vec<Cmd>| -> Vec<String> {
            let backend = by_name(b).unwrap();
            backend
                .assume_yes(f(backend.as_ref()))
                .iter()
                .map(|c| c.display(None))
                .collect()
        };
        assert_eq!(yes("apt", &|b| b.install(&p)), ["apt-get -y install tree"]);
        assert_eq!(yes("dnf", &|b| b.remove(&p)), ["dnf -y remove tree"]);
        assert_eq!(
            yes("zypper", &|b| b.install(&p)),
            ["zypper --non-interactive install tree"]
        );
        assert_eq!(
            yes("pacman", &|b| b.install(&p)),
            ["pacman -S --needed tree --noconfirm"]
        );
        assert_eq!(yes("xbps", &|b| b.remove(&p)), ["xbps-remove -y -R tree"]);
        assert_eq!(yes("pkg", &|b| b.install(&p)), ["pkg install -y tree"]);
        assert_eq!(yes("pkg", &|b| b.sync()), ["pkg update"]);
        assert_eq!(yes("apk", &|b| b.install(&p)), ["apk add tree"]);
        assert_eq!(
            yes("winget", &|b| b.install(&[Package::new("Git.Git", "")])),
            [
                "winget install --id Git.Git --exact --accept-package-agreements --accept-source-agreements --disable-interactivity"
            ]
        );
        // flag_at never panics on an out-of-range index.
        assert_eq!(Cmd::user("x", &[]).flag_at(9, "-y").display(None), "x -y");
    }

    #[test]
    fn native_managers() {
        assert_eq!(native("pkg"), cfg!(target_os = "freebsd"));
        assert_eq!(native("apt"), cfg!(target_os = "linux"));
        assert_eq!(native("winget"), cfg!(windows));
        assert!(native("brew") || cfg!(windows));
    }

    #[test]
    fn version_order() {
        use std::cmp::Ordering::*;
        assert_eq!(vercmp("1.10", "1.9"), Greater);
        assert_eq!(vercmp("2.3.2-r0", "2.3.2-r1"), Less);
        assert_eq!(vercmp("1:1.0", "2.0"), Greater);
        assert_eq!(vercmp("1.0~rc1", "1.0"), Less);
        assert_eq!(vercmp("1.0a", "1.0"), Greater);
        assert_eq!(vercmp("1.0", "1.0"), Equal);
        assert_eq!(vercmp("2.47.0", "2.46.10"), Greater);
        assert_eq!(
            sort_versions(vec!["1.2".into(), "1.10".into(), "1.2".into(), "0.9".into()]),
            ["1.10", "1.2", "0.9"]
        );
    }

    #[test]
    fn vercmp_is_a_total_order() {
        // Mixed digit/letter runs must agree in both directions.
        for (a, b) in [
            ("1.0+dfsg", "1.0.1"),
            ("1.a", "1.1"),
            ("2.0beta", "2.0.1"),
            ("5.2-r1", "5.2.1"),
        ] {
            assert_eq!(vercmp(a, b), vercmp(b, a).reverse(), "{a} vs {b}");
        }
        // Pseudo-random lists over a version-like alphabet: antisymmetry
        // for every pair, and sorting never panics.
        let alphabet: Vec<char> = "0123456789.ab~+-:r".chars().collect();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..300 {
            let list: Vec<String> = (0..12)
                .map(|_| {
                    let len = 1 + (next() % 8) as usize;
                    (0..len)
                        .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                        .collect()
                })
                .collect();
            for a in &list {
                for b in &list {
                    assert_eq!(vercmp(a, b), vercmp(b, a).reverse(), "{a:?} vs {b:?}");
                }
            }
            let _ = sort_versions(list);
        }
    }

    #[test]
    fn version_validation_and_plan() {
        for bad in ["*", "1.0 2.0", "-1", ">=1", "1;rm", "", "1*"] {
            assert!(validate_version(bad).is_err(), "accepted {bad:?}");
        }
        for ok in [
            "2.1.0-1",
            "1:9.0.1234-1ubuntu1",
            "1.0~rc1+dfsg",
            "latest/stable",
            "2.3.2-r0",
        ] {
            assert!(validate_version(ok).is_ok(), "rejected {ok:?}");
        }
        let m = crate::testing::Mock::named("m");
        let pkg = Package {
            installed: true,
            ..Package::new("tree", "2.0")
        };
        let (cmds, notes) = version_plan(&m, &pkg, &VersionChoice::plain("1.0"), true, true).unwrap();
        let shown: Vec<String> = cmds.iter().map(|c| c.display(None)).collect();
        assert_eq!(shown, ["m unhold tree", "m install tree=1.0", "m hold tree"]);
        assert!(notes.iter().any(|n| n.contains("released first")));
        assert!(notes.iter().any(|n| n.starts_with("downgrade")));
        assert!(version_plan(&m, &pkg, &VersionChoice::plain("*"), false, false).is_err());
    }

    /// A manager that says it cannot install a given version uses the
    /// default `install_version` (which runs nothing and says so).
    #[test]
    fn installs_versions_matches_install_version() {
        let p = Package::new("tree", "1.0");
        for name in BACKENDS.iter().filter(|n| **n != "aur") {
            let b = by_name(name).unwrap();
            if !b.installs_versions() {
                let e = b.install_version(&p, "0.9").unwrap_err();
                assert!(is_unsupported(&e), "{name}: {e:#}");
            }
        }
        assert!(!by_name("brew").unwrap().installs_versions());
        assert!(by_name("apt").unwrap().installs_versions());
    }

    /// `needs_root` must match the commands: a manager that says it runs
    /// as the user never builds a root command, and the others do.
    #[test]
    fn needs_root_matches_the_commands() {
        let p = [Package::new("tree", "1.0").repo("formula")];
        // aur needs paru or yay installed to build; its own test covers it.
        // Only command builders that run nothing on this machine.
        for name in BACKENDS.iter().filter(|n| **n != "aur") {
            let b = by_name(name).unwrap();
            let mut cmds = b.install(&p);
            cmds.extend(b.remove(&p));
            cmds.extend(b.upgrade(&p));
            cmds.extend(b.upgrade(&[]));
            cmds.extend(b.sync());
            let root = cmds.iter().any(|c| c.root);
            assert_eq!(b.needs_root(), root, "{name}: needs_root() disagrees with its commands");
        }
        assert!(!by_name("brew").unwrap().needs_root());
        assert!(by_name("apt").unwrap().needs_root());
    }

    #[test]
    fn aur_is_never_detected() {
        assert!(opt_in_only("aur") && !opt_in_only("pacman"));
        if let Ok(all) = detect_all() {
            assert!(all.iter().all(|b| b.name() != "aur"));
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1.5 KiB"), Some(1536));
        assert_eq!(parse_size("6.8MB"), Some(7_130_316));
        assert_eq!(parse_size("12"), Some(12));
        assert_eq!(parse_size("lots"), None);
    }

    #[test]
    fn tables() {
        let z = "S  | Name | Summary          | Type\n---+------+------------------+--------\n   | tree | File listing     | package\ni+ | vim  | Vi IMproved      | package\n";
        let rows = pipe_table(z);
        assert_eq!(rows.len(), 2);
        assert_eq!(col(&rows[1], "S"), "i+");
        assert_eq!(col(&rows[1], "Summary"), "Vi IMproved");

        let w = "\r   - \r   \\ \rName         Id                 Version Source\n-----------------------------------------------\nMicrosoft VS Microsoft.VSCode     1.95.0  winget\nGit          Git.Git            2.47.0  winget\n\n2 packages\n";
        let rows = fixed_table(w);
        assert_eq!(rows.len(), 2);
        assert_eq!(col(&rows[0], "Id"), "Microsoft.VSCode");
        assert_eq!(col(&rows[0], "Name"), "Microsoft VS");
        assert_eq!(col(&rows[1], "Version"), "2.47.0");

        let s = "Name    Installed Version Latest Version Info\n----    ----------------- -------------- ----\ngit     2.46.0            2.47.0\n";
        let rows = fixed_table(s);
        assert_eq!(col(&rows[0], "Installed Version"), "2.46.0");
        assert_eq!(col(&rows[0], "Latest Version"), "2.47.0");
    }

    #[test]
    fn key_value_fields() {
        let f = fields(
            "Name            : tree\nDepends On      : glibc  bash\n                  zlib\nURL             : https://x.org/a:b\n",
        );
        assert_eq!(field(&f, "depends on"), Some("glibc  bash\nzlib"));
        assert_eq!(field(&f, "URL"), Some("https://x.org/a:b"));
        assert_eq!(human_size(68608), "67.0 KiB");
        assert_eq!(human_size(12), "12 B");
    }

    #[test]
    fn name_version() {
        assert_eq!(
            split_name_version("busybox-1.36.1-r15"),
            Some(("busybox", "1.36.1-r15"))
        );
        assert_eq!(
            split_name_version("py3-foo-bar-2.0-r1"),
            Some(("py3-foo-bar", "2.0-r1"))
        );
        assert_eq!(split_name_version("nodash"), None);
    }

    #[test]
    fn sized_lines() {
        let p = parse_sized("tree\t2.1.1-1\t104857\t1730000000\tFile system tree viewer\ntree\tdup\t1\t1\tx\nbad\n");
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].size, p[0].installed_at), (Some(104857), Some(1730000000)));
        assert_eq!(p[0].description, "File system tree viewer");
    }
}
