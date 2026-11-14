//! TOML configuration, loaded from $XDG_CONFIG_HOME/pakdeck/config.toml
//! (falling back to ~/.config/pakdeck/config.toml).

use crate::theme::{Theme, UserTheme};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const TEMPLATE: &str = r##"# pakdeck configuration

# Package manager. Auto-detected (the system one) when unset:
# apt, dnf, yum, zypper, pacman, apk, xbps, pkg, nix, brew, flatpak, snap,
# winget, scoop.
# backend = "apt"

# Use several package managers at once, e.g. ["apt", "flatpak", "snap"] or
# ["brew", "nix"]; ["all"] enables every one found. Overrides `backend`.
# The AUR is only used when listed (not by "all"): ["pacman", "aur"]
# (needs paru or yay).
# backends = ["all"]

# File for export (E) and import (I). Default: ~/pakdeck-packages.toml
# export_path = "~/pakdeck-packages.toml"

# Show commands instead of running them.
dry_run = false

# Ask for confirmation before running a command.
confirm = true

# Where package-manager commands run: "embedded" shows them in a terminal
# pane inside pakdeck; "suspend" leaves the TUI and runs them in your terminal.
run_mode = "embedded"

# Mouse support (click, wheel). Disabling it restores plain text selection;
# with it on, most terminals still select text with Shift+drag.
mouse = true

# Privilege prefix for root commands. Auto: none as root, else sudo, else doas.
# Set to "" to never prefix.
# sudo = "doas"

# Record what pakdeck runs (installs, removals, updates, holds...) in
# ~/.local/state/pakdeck/history.jsonl; H shows it and can undo an entry.
history = true

[theme]
# "auto" asks the terminal for its background colour and picks "dark" or
# "light" ("monochrome" when NO_COLOR is set). T opens a picker with a live
# preview ("auto" included); Enter there saves the choice here. Built-in
# themes:
#   dark, light, high-contrast,
#   terminal (only the terminal's 16 colours), monochrome (no colour),
#   colorblind, colorblind-light (Okabe-Ito colours),
#   catppuccin-mocha, catppuccin-latte, gruvbox-dark, gruvbox-light, nord,
#   dracula, solarized-dark, solarized-light, tokyo-night
# plus your own, defined below. Without 24-bit colour support (COLORTERM is
# not "truecolor"), exact colours are shown as the nearest of 256.
preset = "auto"
# Override single colours of whichever theme is active: names (red, cyan,
# darkgray, ...), indices ("42") or hex ("#89b4fa").
# accent = "cyan"
# selected_bg = "#444444"
# installed = "green"
# marked = "yellow"
# update = "magenta"
# error = "red"
# on_accent = "black"   (text on accent-coloured buttons)
# muted = "gray"        (inactive buttons)
# dim = "darkgray"      (versions, borders, hints)
# bg = "#1e1e2e"        (background; unset: the terminal's)
# fg = "#cdd6f4"        (text; unset: the terminal's)

# Your own themes: a built-in `base` (default "dark") and the colours to
# change, with the same keys as above. They appear in the T picker. Also:
# `kind` ("dark", "light" or "terminal", shown in the picker; default: the
# base's) and `reverse_selection` (show the selection reversed).
# [themes.mine]
# base = "nord"
# accent = "#ff79c6"

[keys]
install = "i"
delete = "d"
update = "u"
update_all = "U"
cleanup = "c"
sync = "s"
reload = "r"
sort = "o"
versions = "v"
theme = "T"
tree = "t"
disk = "z"
repos = "R"
history = "H"
clear_log = "L"
# Inside the repositories popup: `add` adds one, `delete` removes the
# selected one, `source` filters the Installed tab by it. In the dependency
# tree, `tree` again switches between dependencies and the packages that
# need it.
add = "a"
hold = "p"
source = "f"
export = "E"
import = "I"
explicit = "e"
expand = "x"
run_mode = "M"
mark = " "
search = "/"
dry_run = "D"
help = "?"
quit = "q"
"##;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub backend: Option<String>,
    pub backends: Vec<String>,
    pub export_path: Option<String>,
    pub dry_run: bool,
    pub confirm: bool,
    pub run_mode: RunMode,
    pub mouse: bool,
    pub sudo: Option<String>,
    /// Record what pakdeck runs (H shows it; undo works from it).
    pub history: bool,
    pub theme: Theme,
    /// The user's own themes, by name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub themes: BTreeMap<String, UserTheme>,
    pub keys: Keys,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: None,
            backends: Vec::new(),
            export_path: None,
            dry_run: false,
            confirm: true,
            run_mode: RunMode::Embedded,
            mouse: true,
            sudo: None,
            history: true,
            theme: Theme::default(),
            themes: BTreeMap::new(),
            keys: Keys::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    #[default]
    Embedded,
    Suspend,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keys {
    pub install: char,
    pub delete: char,
    pub update: char,
    pub update_all: char,
    pub cleanup: char,
    pub sync: char,
    pub reload: char,
    pub sort: char,
    pub versions: char,
    pub theme: char,
    pub tree: char,
    pub disk: char,
    pub repos: char,
    pub history: char,
    pub clear_log: char,
    /// Add a repository, inside the repositories popup.
    pub add: char,
    pub hold: char,
    pub source: char,
    pub export: char,
    pub import: char,
    pub explicit: char,
    pub expand: char,
    pub run_mode: char,
    pub mark: char,
    pub search: char,
    pub dry_run: char,
    pub help: char,
    pub quit: char,
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            install: 'i',
            delete: 'd',
            update: 'u',
            update_all: 'U',
            cleanup: 'c',
            sync: 's',
            reload: 'r',
            sort: 'o',
            versions: 'v',
            theme: 'T',
            tree: 't',
            disk: 'z',
            repos: 'R',
            history: 'H',
            clear_log: 'L',
            add: 'a',
            hold: 'p',
            source: 'f',
            export: 'E',
            import: 'I',
            explicit: 'e',
            expand: 'x',
            run_mode: 'M',
            mark: ' ',
            search: '/',
            dry_run: 'D',
            help: '?',
            quit: 'q',
        }
    }
}

/// Expand a leading `~/` to the home directory.
pub fn expand_home(p: &str) -> PathBuf {
    // USERPROFILE: Windows has no HOME.
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (p.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

impl Config {
    /// Where export (E) writes and import (I) reads.
    pub fn export_file(&self) -> PathBuf {
        expand_home(self.export_path.as_deref().unwrap_or("~/pakdeck-packages.toml"))
    }
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("pakdeck").join("config.toml"))
}

impl Config {
    /// Load from an explicit path (must exist) or the default path (optional).
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => match default_path() {
                Some(p) if p.exists() => p,
                _ => return Ok(Self::default()),
            },
        };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // Keys the user set win over defaults added in newer versions: a
        // default that collides with an explicit binding is unbound.
        let raw: toml::Table = toml::from_str(&text).unwrap_or_default();
        let explicit: Vec<String> = raw
            .get("keys")
            .and_then(|k| k.as_table())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        cfg.keys.yield_to(&explicit);
        cfg.validate_themes()
            .with_context(|| format!("invalid theme in {}", path.display()))?;
        cfg.keys
            .validate()
            .with_context(|| format!("invalid [keys] in {}", path.display()))?;
        Ok(cfg)
    }
}

impl Config {
    /// `preset` names a theme that exists; the user's own themes neither
    /// shadow a built-in nor start from an unknown base.
    pub fn validate_themes(&self) -> Result<()> {
        for (name, t) in &self.themes {
            if name == "auto" || crate::theme::builtin(name).is_some() {
                anyhow::bail!("[themes.{name}]: {name:?} is a built-in theme; pick another name");
            }
            if let Some(base) = &t.base
                && crate::theme::builtin(base).is_none()
            {
                anyhow::bail!("[themes.{name}]: unknown base theme {base:?}");
            }
            if let Some(kind) = &t.kind
                && crate::theme::Kind::parse(kind).is_none()
            {
                anyhow::bail!("[themes.{name}]: kind is \"dark\", \"light\" or \"terminal\", not {kind:?}");
            }
            // A typo would silently keep the base colour: say so instead.
            for (key, value) in t.overrides() {
                if let Some(v) = value
                    && crate::theme::parse_color(v).is_none()
                {
                    anyhow::bail!("[themes.{name}]: {key} = {v:?} is not a colour (a name, \"42\" or \"#89b4fa\")");
                }
            }
        }
        let p = self.theme.preset.as_str();
        if p != "auto" && crate::theme::builtin(p).is_none() && !self.themes.contains_key(p) {
            let names: Vec<&str> = crate::theme::BUILTIN.iter().map(|b| b.name).collect();
            anyhow::bail!(
                "unknown theme preset {p:?} (auto, {}, or one of [themes])",
                names.join(", ")
            );
        }
        Ok(())
    }
}

/// `path` with the home directory shown as `~` (short enough for the log).
pub fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A config file pakdeck creates itself: only what was chosen, so every other
/// setting keeps following pakdeck's defaults.
const NEW_CONFIG: &str = "# pakdeck configuration. `pakdeck --print-config` prints every setting with its\n# default and an explanation.\n\n[theme]\n";

/// Write `preset = NAME` into the `[theme]` table of the config file at
/// `path`, keeping everything else in it (comments included). A missing
/// file is created holding just that.
pub fn save_theme(path: &Path, name: &str) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => NEW_CONFIG.to_string(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
    let theme = doc
        .entry("theme")
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .context("[theme] is not a table")?;
    theme.insert("preset", toml_edit::value(name));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    crate::util::write_atomic(path, &doc.to_string())
}

/// Keys handled before configurable actions never reach them, and
/// navigation keys are checked after: an action bound to one of these
/// would silently shadow navigation.
const RESERVED_KEYS: &str = "hjklgGJK1234";

/// Actions added in this release; only their defaults may yield.
const NEW_KEYS: [&str; 8] = [
    "sort",
    "versions",
    "theme",
    "tree",
    "disk",
    "repos",
    "history",
    "clear_log",
];

/// An action with no key (a default that yielded to a user binding).
pub const UNBOUND: char = '\0';

impl Keys {
    /// The global actions (normal mode). Popup-only keys such as `add` are
    /// not here: they never meet these, see `popup_keys`.
    fn all(&self) -> [(&'static str, char); 27] {
        [
            ("tree", self.tree),
            ("disk", self.disk),
            ("repos", self.repos),
            ("history", self.history),
            ("clear_log", self.clear_log),
            ("sort", self.sort),
            ("versions", self.versions),
            ("theme", self.theme),
            ("install", self.install),
            ("delete", self.delete),
            ("update", self.update),
            ("update_all", self.update_all),
            ("cleanup", self.cleanup),
            ("sync", self.sync),
            ("reload", self.reload),
            ("hold", self.hold),
            ("source", self.source),
            ("export", self.export),
            ("import", self.import),
            ("explicit", self.explicit),
            ("expand", self.expand),
            ("run_mode", self.run_mode),
            ("mark", self.mark),
            ("search", self.search),
            ("dry_run", self.dry_run),
            ("help", self.help),
            ("quit", self.quit),
        ]
    }

    fn set(&mut self, name: &str, c: char) {
        let slot = match name {
            "sort" => &mut self.sort,
            "versions" => &mut self.versions,
            "theme" => &mut self.theme,
            "tree" => &mut self.tree,
            "disk" => &mut self.disk,
            "repos" => &mut self.repos,
            "history" => &mut self.history,
            "clear_log" => &mut self.clear_log,
            "install" => &mut self.install,
            "delete" => &mut self.delete,
            "update" => &mut self.update,
            "update_all" => &mut self.update_all,
            "cleanup" => &mut self.cleanup,
            "sync" => &mut self.sync,
            "reload" => &mut self.reload,
            "hold" => &mut self.hold,
            "source" => &mut self.source,
            "export" => &mut self.export,
            "import" => &mut self.import,
            "explicit" => &mut self.explicit,
            "expand" => &mut self.expand,
            "run_mode" => &mut self.run_mode,
            "mark" => &mut self.mark,
            "search" => &mut self.search,
            "dry_run" => &mut self.dry_run,
            "help" => &mut self.help,
            "quit" => &mut self.quit,
            _ => return,
        };
        *slot = c;
    }

    /// Unbind keys added in this release whose defaults collide with keys
    /// the user set explicitly (`explicit` names the actions present in
    /// the config's [keys]), so older configs keep working. Other clashes
    /// are still reported by `validate`.
    pub fn yield_to(&mut self, explicit: &[String]) {
        let all = self.all();
        for (name, key) in all {
            if !NEW_KEYS.contains(&name) {
                continue;
            }
            let user_set = explicit.iter().any(|e| e == name);
            let taken = all
                .iter()
                .any(|(other, k)| *k == key && *other != name && explicit.iter().any(|e| e == other));
            if !user_set && taken {
                self.set(name, UNBOUND);
            }
        }
    }

    /// Actions left without a key by `yield_to`.
    pub fn unbound(&self) -> Vec<&'static str> {
        self.all()
            .into_iter()
            .filter(|(_, k)| *k == UNBOUND)
            .map(|(n, _)| n)
            .collect()
    }

    /// Reject two actions on one key and actions on navigation keys.
    /// The keys active inside the repositories popup: its own `add`, the
    /// global bindings it reuses, and `quit`, which closes it.
    fn popup_keys(&self) -> [(&'static str, char); 4] {
        [
            ("add", self.add),
            ("delete", self.delete),
            ("source", self.source),
            ("quit", self.quit),
        ]
    }

    pub fn validate(&self) -> Result<()> {
        // Global keys clash with each other; popup keys only with the keys
        // of their popup (a modal popup never sees the rest).
        for set in [self.all().as_slice(), self.popup_keys().as_slice()] {
            for (i, (name, key)) in set.iter().enumerate().filter(|(_, (_, k))| *k != UNBOUND) {
                if RESERVED_KEYS.contains(*key) {
                    anyhow::bail!("{name} = {key:?}: that key is used for navigation");
                }
                if let Some((other, _)) = set.iter().skip(i.saturating_add(1)).find(|(_, k)| k == key) {
                    anyhow::bail!("{name} and {other} are both bound to {key:?}");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_matches_default() {
        let c: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn popup_keys_are_checked_within_their_popup() {
        // A global binding on "a" no longer costs the popup its add key.
        let c: Config = toml::from_str("[keys]\ninstall = \"a\"\n").unwrap();
        let mut keys = c.keys;
        keys.yield_to(&["install".into()]);
        assert_eq!(keys.add, 'a');
        assert!(keys.validate().is_ok());
        // `add` may share a key with a global action it never meets...
        let k = Keys {
            add: 'i',
            ..Keys::default()
        };
        assert!(k.validate().is_ok());
        // ...but not with the popup's own keys, or with navigation.
        for clash in ['d', 'f', 'q', 'j'] {
            let k = Keys {
                add: clash,
                ..Keys::default()
            };
            assert!(k.validate().is_err(), "add = {clash:?} accepted");
        }
    }

    #[test]
    fn partial_config() {
        let c: Config = toml::from_str("backend = \"apk\"\n[keys]\ninstall = \"a\"\n").unwrap();
        assert_eq!(c.backend.as_deref(), Some("apk"));
        assert_eq!(c.keys.install, 'a');
        assert_eq!(c.keys.delete, 'd');
    }

    #[test]
    fn new_defaults_yield_to_user_keys() {
        // An older config bound `v` to delete; the new `versions` default
        // (v) must not make it invalid.
        let mut keys: Keys = toml::from_str("delete = \"v\"").unwrap();
        keys.yield_to(&["delete".to_string()]);
        assert_eq!(keys.delete, 'v');
        assert_eq!(keys.versions, UNBOUND);
        assert!(keys.validate().is_ok());
        assert_eq!(keys.unbound(), ["versions"]);
        // Older actions never yield: a clash with them is still an error.
        let mut clash: Keys = toml::from_str("delete = \"i\"").unwrap();
        clash.yield_to(&["delete".to_string()]);
        assert!(clash.validate().is_err());
        // Two explicit bindings on one key are still an error.
        let mut both: Keys = toml::from_str("delete = \"x\"\nhold = \"x\"").unwrap();
        both.yield_to(&["delete".to_string(), "hold".to_string()]);
        assert!(both.validate().is_err());
    }

    #[test]
    fn key_validation() {
        assert!(Keys::default().validate().is_ok());
        let clash = Keys {
            hold: 'i',
            ..Keys::default()
        };
        assert!(clash.validate().unwrap_err().to_string().contains("both bound"));
        let nav = Keys {
            hold: 'h',
            ..Keys::default()
        };
        assert!(nav.validate().unwrap_err().to_string().contains("navigation"));
    }

    #[test]
    fn colors_parse() {
        use crate::theme::color;
        use ratatui::style::Color;
        // Overrides apply on top of any theme; old configs still parse.
        let t: Theme = toml::from_str("preset = \"light\"\naccent = \"#ff0000\"").unwrap();
        let cfg = Config {
            theme: t,
            ..Config::default()
        };
        let c = crate::theme::resolve(&cfg.theme, &cfg.themes, "light", true).unwrap();
        assert_eq!((c.accent, c.on_accent), (Color::Rgb(255, 0, 0), Color::White));
        let old: Theme = toml::from_str("accent = \"cyan\"\nselected_bg = \"darkgray\"").unwrap();
        assert_eq!(old.preset, "auto", "an old config means auto");
        assert_eq!(color("#ff0000", Color::Reset), Color::Rgb(255, 0, 0));
    }

    #[test]
    fn theme_validation() {
        let ok: Config = toml::from_str("[theme]\npreset = \"mine\"\n[themes.mine]\nbase = \"dracula\"\n").unwrap();
        assert!(ok.validate_themes().is_ok());
        for bad in [
            "[theme]\npreset = \"nope\"\n",
            "[themes.nord]\naccent = \"red\"\n",
            "[themes.auto]\n",
            "[themes.x]\nbase = \"nope\"\n",
            "[themes.x]\naccent = \"#12345\"\n",
            "[themes.x]\nfg = \"notacolour\"\n",
            "[themes.x]\nkind = \"sepia\"\n",
        ] {
            let cfg: Config = toml::from_str(bad).unwrap();
            assert!(cfg.validate_themes().is_err(), "accepted {bad:?}");
        }
        assert!(toml::from_str::<Config>("[themes.x]\nbogus = 1\n").is_err());
        for b in crate::theme::BUILTIN {
            let cfg = Config {
                theme: Theme {
                    preset: b.name.into(),
                    ..Theme::default()
                },
                ..Config::default()
            };
            assert!(cfg.validate_themes().is_ok(), "{}", b.name);
        }
    }

    #[test]
    fn save_theme_keeps_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("pakdeck-theme-{}", std::process::id()));
        let f = dir.join("sub").join("config.toml");
        // Missing file: only the theme, so every other setting keeps
        // following pakdeck's defaults (and new key defaults never yield).
        save_theme(&f, "nord").unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.contains("preset = \"nord\""), "{text}");
        assert!(text.contains("--print-config"), "{text}");
        assert!(!text.contains("[keys]") && !text.contains("dry_run"), "{text}");
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.theme.preset, "nord");
        assert_eq!(
            Config {
                theme: Theme::default(),
                ..cfg
            },
            Config::default()
        );
        // Existing file: comments and other settings stay.
        std::fs::write(&f, "# mine\ndry_run = true\n\n[theme]\naccent = \"red\" # keep\n").unwrap();
        save_theme(&f, "dracula").unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.starts_with("# mine\ndry_run = true"), "{text}");
        assert!(text.contains("accent = \"red\" # keep"), "{text}");
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!((cfg.theme.preset.as_str(), cfg.dry_run), ("dracula", true));
        // A file that does not parse is an error and stays as it was.
        std::fs::write(&f, "this is = = not toml").unwrap();
        assert!(save_theme(&f, "nord").is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "this is = = not toml");
        // `theme` that is not a table is an error too.
        std::fs::write(&f, "theme = \"nord\"\n").unwrap();
        assert!(save_theme(&f, "nord").is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "theme = \"nord\"\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
