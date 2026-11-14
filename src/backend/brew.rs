//! Homebrew (macOS and Linuxbrew). Formulae and casks are both supported;
//! `Package::repo` holds the kind ("formula" or "cask").
//!
//! Packages from third-party taps are named by their full name
//! ("user/tap/name"), which is what `brew search` and `brew outdated` print and
//! what brew accepts everywhere; core packages keep their short name.

use super::{Backend, Cmd, Package, Repo, Repositories, Section, SizeMeter, capture, capture_env, jget, lines, run};
use anyhow::{Context, Result};
use serde_json::Value;

pub struct Brew;

const CASK: &str = "cask";
const FORMULA: &str = "formula";

impl Backend for Brew {
    /// Runs as the user: no command ever needs sudo.
    fn needs_root(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "brew"
    }

    fn search(&self, query: &str) -> Result<Vec<Package>> {
        let mut v = Vec::new();
        for (flag, kind) in [("--formula", FORMULA), ("--cask", CASK)] {
            // brew exits 1 when nothing matches.
            let out = capture("brew", &["search", flag, query.trim()], &[1])?;
            v.extend(parse_search(&out, kind));
        }
        Ok(v)
    }

    fn installed(&self) -> Result<Vec<Package>> {
        parse_installed(&run("brew", &["info", "--json=v2", "--installed"])?)
    }

    /// Every outdated package, casks that update themselves included
    /// (`--greedy`; plain `brew outdated` hides them). The catalogue is
    /// refreshed first when it is older than four hours.
    fn updates(&self) -> Result<Vec<Package>> {
        let (set, remove) = refresh_env(REFRESH_AFTER_SECS, |name| std::env::var_os(name).is_some());
        let all = capture_env("brew", &["outdated", "--greedy", "--json=v2"], &[], &set, &remove)?;
        let plain = run("brew", &["outdated", "--json=v2"])?;
        Ok(mark_self_updating(parse_outdated(&all)?, &parse_outdated(&plain)?))
    }

    /// Only when the refresh can happen: the user's own
    /// HOMEBREW_NO_AUTO_UPDATE turns it off.
    fn updates_note(&self) -> Option<String> {
        refresh_note(|name| std::env::var_os(name).is_some())
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        match pkg.repo.as_str() {
            CASK => run("brew", &["info", "--cask", &pkg.name]),
            FORMULA => run("brew", &["info", "--formula", &pkg.name]),
            _ => run("brew", &["info", &pkg.name]),
        }
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        grouped(&["install"], pkgs)
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        grouped(&["uninstall"], pkgs)
    }

    /// `--greedy`: casks that update themselves are upgraded too, as the
    /// Updates tab lists them.
    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            return vec![Cmd::user("brew", &["upgrade", "--greedy"])];
        }
        let of = |kind: &str| -> Vec<Package> { pkgs.iter().filter(|p| p.repo == kind).cloned().collect() };
        let (formulae, casks) = (of(FORMULA), of(CASK));
        // Named on the command line: the kind is unknown, and it may be a
        // cask that updates itself (--greedy is harmless for a formula).
        let unknown: Vec<Package> = pkgs
            .iter()
            .filter(|p| p.repo != FORMULA && p.repo != CASK)
            .cloned()
            .collect();
        let mut v = Vec::new();
        if !formulae.is_empty() {
            v.push(Cmd::user("brew", &["upgrade"]).names(&formulae));
        }
        if !casks.is_empty() {
            v.push(Cmd::user("brew", &["upgrade", "--cask", "--greedy"]).names(&casks));
        }
        if !unknown.is_empty() {
            v.push(Cmd::user("brew", &["upgrade", "--greedy"]).names(&unknown));
        }
        v
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::user("brew", &["autoremove"]),
            Cmd::user("brew", &["cleanup"]),
        ])
    }

    fn explicit(&self) -> Result<Vec<String>> {
        parse_explicit(&run("brew", &["info", "--json=v2", "--installed"])?)
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        Ok(parse_autoremove(&run("brew", &["autoremove", "--dry-run"])?))
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let name = pkg.name.as_str();
        let kind = if pkg.repo == CASK { "--cask" } else { "--formula" };
        Ok(match s {
            Section::Size => parse_size(&self.info(pkg)?).into_iter().collect(),
            Section::Depends => lines(&run("brew", &["deps", "--direct", kind, name])?),
            // Nothing can depend on a cask.
            Section::RequiredBy if pkg.repo == CASK => Vec::new(),
            Section::RequiredBy => lines(&run("brew", &["uses", "--installed", name])?),
            Section::Files => lines(&capture("brew", &["list", kind, name], &[])?),
        })
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(lines(&run("brew", &["list", "--pinned"])?))
    }

    /// brew pins formulae only; casks cannot be pinned.
    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        let formulae: Vec<Package> = pkgs.iter().filter(|p| p.repo != CASK).cloned().collect();
        if formulae.is_empty() && !pkgs.is_empty() {
            return super::unsupported("brew cannot pin casks");
        }
        Ok(vec![
            Cmd::user("brew", &[if on { "pin" } else { "unpin" }]).names(&formulae),
        ])
    }

    fn repositories(&self) -> Option<&dyn Repositories> {
        Some(self)
    }

    fn size_meter(&self) -> Option<&dyn SizeMeter> {
        Some(self)
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::user("brew", &["update"])]
    }
}

/// Taps.
impl Repositories for Brew {
    fn kind(&self) -> &'static str {
        "Taps"
    }

    fn add_hint(&self) -> &'static str {
        "user/repo, optionally followed by a git URL"
    }

    fn list(&self) -> Result<Vec<Repo>> {
        Ok(parse_taps(&run("brew", &["tap-info", "--installed", "--json"])?)?
            .into_iter()
            .map(|r| Repo {
                source: self.name().to_string(),
                ..r
            })
            .collect())
    }

    /// `user/repo`, optionally followed by a git URL.
    fn add(&self, _source: &str, spec: &str) -> Result<Vec<Cmd>> {
        let mut parts = spec.split_whitespace();
        let name = parts.next().unwrap_or_default();
        validate_tap(name)?;
        let mut cmd = Cmd::user("brew", &["tap", name]);
        if let Some(url) = parts.next() {
            super::validate_name(url)?;
            cmd = cmd.strings([url.to_string()]);
        }
        if parts.next().is_some() {
            anyhow::bail!("a tap is added as `user/repo [URL]`, not {spec:?}");
        }
        Ok(vec![cmd])
    }

    /// brew refuses to untap while packages from the tap are installed.
    fn remove(&self, repo: &Repo) -> Result<Vec<Cmd>> {
        validate_tap(&repo.name)?;
        Ok(vec![Cmd::user("brew", &["untap", repo.name.as_str()])])
    }

    /// Tap packages are named "user/tap/name"; core ones have no prefix.
    fn contains(&self, repo: &Repo, pkg: &Package) -> bool {
        let tap = repo.name.trim_end_matches('/');
        pkg.name.strip_prefix(tap).is_some_and(|rest| rest.starts_with('/'))
            || match tap {
                "homebrew/core" => !pkg.name.contains('/') && pkg.repo != CASK,
                "homebrew/cask" => !pkg.name.contains('/') && pkg.repo == CASK,
                _ => false,
            }
    }
}

/// Formulae: their Cellar directory. Casks: their Caskroom directory plus
/// what was moved out of it (apps to /Applications, fonts, prefpanes...);
/// the Caskroom alone keeps only symlinks and metadata for those. Casks
/// that run a .pkg installer put files anywhere and are left unsized.
impl SizeMeter for Brew {
    fn measures(&self, source: &str) -> bool {
        source == self.name()
    }

    fn measure(&self, pkgs: &[Package]) -> Result<Vec<(String, u64)>> {
        let cellar = run("brew", &["--cellar"])?.trim().to_string();
        let caskroom = run("brew", &["--caskroom"])?.trim().to_string();
        let casks = if pkgs.iter().any(|p| p.repo == CASK) {
            parse_cask_targets(&run("brew", &["info", "--cask", "--json=v2", "--installed"])?)?
        } else {
            std::collections::HashMap::new()
        };
        // (package key, paths that make up its size)
        let mut wanted: Vec<(String, Vec<String>)> = Vec::new();
        for p in pkgs {
            let short = p.name.rsplit('/').next().unwrap_or(&p.name);
            let paths = if p.repo == CASK {
                match casks.get(short) {
                    Some(None) => continue,
                    Some(Some(targets)) => std::iter::once(format!("{caskroom}/{short}"))
                        .chain(targets.iter().cloned())
                        .collect(),
                    None => vec![format!("{caskroom}/{short}")],
                }
            } else {
                vec![format!("{cellar}/{short}")]
            };
            let paths: Vec<String> = paths.into_iter().filter(|p| std::path::Path::new(p).exists()).collect();
            if !paths.is_empty() {
                wanted.push((p.key(), paths));
            }
        }
        let all: Vec<&str> = wanted.iter().flat_map(|(_, p)| p.iter().map(String::as_str)).collect();
        if all.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["-sk", "--"];
        args.extend(all);
        // du exits 1 when an entry vanished meanwhile; keep what it printed.
        let measured: std::collections::HashMap<String, u64> =
            parse_du(&capture("du", &args, &[1])?).into_iter().collect();
        Ok(wanted
            .into_iter()
            .filter_map(|(key, paths)| {
                let sizes: Vec<u64> = paths
                    .iter()
                    .filter_map(|p| measured.get(p.trim_end_matches('/')).copied())
                    .collect();
                (!sizes.is_empty()).then(|| (key, sizes.into_iter().fold(0u64, u64::saturating_add)))
            })
            .collect())
    }
}

/// Artifact kinds whose files brew moves out of the Caskroom to `target`.
const MOVED: &[&str] = &[
    "app",
    "audio_unit_plugin",
    "colorpicker",
    "dictionary",
    "font",
    "input_method",
    "internet_plugin",
    "keyboard_layout",
    "mdimporter",
    "prefpane",
    "qlplugin",
    "screen_saver",
    "service",
    "suite",
    "vst3_plugin",
    "vst_plugin",
];

/// `brew info --cask --json=v2 --installed`: per cask token, the moved
/// artifacts' target paths, or `None` when a .pkg installer or script put
/// files elsewhere (the size cannot be known).
pub(crate) fn parse_cask_targets(json: &str) -> Result<std::collections::HashMap<String, Option<Vec<String>>>> {
    let v: Value = serde_json::from_str(json).context("parsing brew cask JSON")?;
    Ok(jget(&v, "casks")
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let token = jget(c, "token").as_str()?.to_string();
            let artifacts = jget(c, "artifacts").as_array().cloned().unwrap_or_default();
            let installer = artifacts.iter().any(|a| {
                a.as_object()
                    .is_some_and(|o| o.contains_key("pkg") || o.contains_key("installer"))
            });
            if installer {
                return Some((token, None));
            }
            let targets = artifacts
                .iter()
                .filter_map(|a| a.as_object())
                .filter(|o| MOVED.iter().any(|k| o.contains_key(*k)))
                .filter_map(|o| o.get("target").and_then(Value::as_str))
                .map(String::from)
                .collect();
            Some((token, Some(targets)))
        })
        .collect())
}

/// One command for formulae and one with --cask for casks.
fn grouped(base: &[&str], pkgs: &[Package]) -> Vec<Cmd> {
    let (casks, formulae): (Vec<Package>, Vec<Package>) = pkgs.iter().cloned().partition(|p| p.repo == CASK);
    let mut v = Vec::new();
    if !formulae.is_empty() {
        v.push(Cmd::user("brew", base).names(&formulae));
    }
    if !casks.is_empty() {
        let mut args = base.to_vec();
        args.push("--cask");
        v.push(Cmd::user("brew", &args).names(&casks));
    }
    v
}

pub(crate) fn parse_search(out: &str, kind: &str) -> Vec<Package> {
    out.lines()
        .filter(|l| !l.starts_with("==>"))
        .filter_map(|l| {
            let mut t = l.split_whitespace();
            let name = t.next()?;
            // Allow an "installed" marker after the name, nothing more.
            (t.count() <= 1).then(|| Package::new(name, "").repo(kind))
        })
        .collect()
}

/// A version field that may be a string or an array of strings (last wins).
fn version_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.last().map(version_of).unwrap_or_default(),
        Value::Object(o) => o.get("version").map(version_of).unwrap_or_default(),
        _ => String::new(),
    }
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// `full` (e.g. "user/tap/name") when present, else `short`.
fn name_of(v: &Value, full: &str, short: &str) -> String {
    match v[full].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => str_of(&v[short]),
    }
}

pub(crate) fn parse_installed(json: &str) -> Result<Vec<Package>> {
    let v: Value = serde_json::from_str(json).context("parsing brew info JSON")?;
    let mut out = Vec::new();
    for f in jget(&v, "formulae").as_array().into_iter().flatten() {
        out.push(
            Package::new(name_of(f, "full_name", "name"), version_of(jget(f, "installed")))
                .desc(str_of(jget(f, "desc")))
                .repo(FORMULA)
                .installed_at(
                    jget(f, "installed")
                        .as_array()
                        .and_then(|a| a.last())
                        .and_then(|i| jget(i, "time").as_u64()),
                ),
        );
    }
    for c in jget(&v, "casks").as_array().into_iter().flatten() {
        out.push(
            Package::new(name_of(c, "full_token", "token"), version_of(jget(c, "installed")))
                .desc(str_of(jget(c, "desc")))
                .repo(CASK),
        );
    }
    Ok(out)
}

/// Refresh the catalogue for the Updates query when it is older than
/// this, unless the user set HOMEBREW_NO_AUTO_UPDATE or
/// HOMEBREW_AUTO_UPDATE_SECS themselves.
const REFRESH_AFTER_SECS: u32 = 4 * 3600;

/// Environment for the Updates query: (variables to set, variables to
/// remove). Removing HOMEBREW_NO_AUTO_UPDATE undoes `capture`'s default;
/// `user_set` says whether the user's environment sets a name.
/// What the Updates tab says while loading: only when a refresh can
/// happen (`refresh_env` removes HOMEBREW_NO_AUTO_UPDATE).
pub(crate) fn refresh_note(user_set: impl Fn(&str) -> bool) -> Option<String> {
    let (_, remove) = refresh_env(REFRESH_AFTER_SECS, user_set);
    (!remove.is_empty()).then(|| {
        "Checking for updates. Homebrew first refreshes its catalogue if it is older than 4 hours; \
         this can take a minute."
            .to_string()
    })
}

pub(crate) fn refresh_env(
    secs: u32,
    user_set: impl Fn(&str) -> bool,
) -> (Vec<(&'static str, String)>, Vec<&'static str>) {
    if user_set("HOMEBREW_NO_AUTO_UPDATE") {
        return (Vec::new(), Vec::new());
    }
    let set = if user_set("HOMEBREW_AUTO_UPDATE_SECS") {
        Vec::new()
    } else {
        vec![("HOMEBREW_AUTO_UPDATE_SECS", secs.to_string())]
    };
    (set, vec!["HOMEBREW_NO_AUTO_UPDATE"])
}

/// Note the casks that only `--greedy` lists: they update themselves
/// (auto_updates, or version "latest").
pub(crate) fn mark_self_updating(all: Vec<Package>, plain: &[Package]) -> Vec<Package> {
    all.into_iter()
        .map(|p| {
            let listed = plain.iter().any(|q| q.name == p.name && q.repo == p.repo);
            if p.repo == CASK && !listed {
                p.note("updates itself")
            } else {
                p
            }
        })
        .collect()
}

pub(crate) fn parse_outdated(json: &str) -> Result<Vec<Package>> {
    let v: Value = serde_json::from_str(json).context("parsing brew outdated JSON")?;
    let mut out = Vec::new();
    for (key, kind) in [("formulae", FORMULA), ("casks", CASK)] {
        for p in jget(&v, key).as_array().into_iter().flatten() {
            out.push(
                Package::new(str_of(jget(p, "name")), version_of(jget(p, "installed_versions")))
                    .repo(kind)
                    .upgrade_to(version_of(jget(p, "current_version"))),
            );
        }
    }
    Ok(out)
}

/// Formulae installed on request (by full name) plus every cask.
pub(crate) fn parse_explicit(json: &str) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(json).context("parsing brew info JSON")?;
    let mut out = Vec::new();
    for f in jget(&v, "formulae").as_array().into_iter().flatten() {
        let on_request = jget(f, "installed")
            .as_array()
            .into_iter()
            .flatten()
            .any(|i| jget(i, "installed_on_request").as_bool() == Some(true));
        if on_request {
            out.push(name_of(f, "full_name", "name"));
        }
    }
    for c in jget(&v, "casks").as_array().into_iter().flatten() {
        out.push(name_of(c, "full_token", "token"));
    }
    Ok(out)
}

/// `brew autoremove --dry-run`: "==> Would autoremove 2 unneeded formulae:" then names.
pub(crate) fn parse_autoremove(out: &str) -> Vec<Package> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("==>") && !l.contains(' '))
        .map(|n| Package::new(n, "").repo(FORMULA))
        .collect()
}

/// Size from `brew info`: "/opt/homebrew/Cellar/tree/2.3.2 (8 files, 175.6KB) *"
pub(crate) fn parse_size(info: &str) -> Option<String> {
    info.lines().find_map(|l| {
        let inner = l.split_once(" (")?.1.split_once(')')?.0;
        let (files, size) = inner.split_once(", ")?;
        files.ends_with("files").then(|| format!("{size} ({files})"))
    })
}

/// `du -sk` lines: "1234\t/opt/homebrew/Cellar/tree" -> (path, bytes),
/// without a trailing slash.
pub(crate) fn parse_du(out: &str) -> Vec<(String, u64)> {
    out.lines()
        .filter_map(|l| {
            let (kb, path) = l.split_once('\t')?;
            let kb: u64 = kb.trim().parse().ok()?;
            let path = path.trim_end_matches('/');
            (!path.is_empty()).then(|| (path.to_string(), kb.saturating_mul(1024)))
        })
        .collect()
}

/// A tap name is "user/repo" (no options, no whitespace).
fn validate_tap(name: &str) -> Result<()> {
    super::validate_name(name)?;
    match name.split_once('/') {
        Some((u, r))
            if [u, r]
                .iter()
                .all(|part| part.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()))
                && !r.contains('/') =>
        {
            Ok(())
        }
        _ => anyhow::bail!("a tap is named user/repo, not {name:?}"),
    }
}

/// `brew tap-info --installed --json`
pub(crate) fn parse_taps(json: &str) -> Result<Vec<Repo>> {
    let v: Value = serde_json::from_str(json).context("parsing brew tap-info JSON")?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let count = |k: &str| jget(t, k).as_array().map_or(0, Vec::len);
            Repo {
                name: str_of(jget(t, "name")),
                official: jget(t, "official").as_bool().unwrap_or(false),
                packages: count("formula_names").saturating_add(count("cask_tokens")),
                remote: str_of(jget(t, "remote")),
                source: String::new(),
            }
        })
        .filter(|r| !r.name.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search() {
        let p = parse_search(
            "==> Formulae\nripgrep\nripgrep-all\ngraykode/tap/abtop\nIf you meant something else try this\n",
            FORMULA,
        );
        assert_eq!(p.len(), 3);
        assert_eq!(p[2].name, "graykode/tap/abtop");
        assert_eq!(p[0].repo, FORMULA);
    }

    #[test]
    fn installed() {
        let json = r#"{"formulae":[{"name":"wget","full_name":"wget","desc":"Internet file retriever","installed":[{"version":"1.24.5"}]},
                                   {"name":"abtop","full_name":"graykode/tap/abtop","desc":"monitor","installed":[{"version":"0.5.5"}]}],
                       "casks":[{"token":"firefox","full_token":"firefox","desc":"Web browser","installed":"131.0"},
                                {"token":"fuse-t","full_token":"macos-fuse-t/cask/fuse-t","desc":"FUSE","installed":"1.0"},
                                {"token":"old","desc":"no full_token field","installed":"2.0"}]}"#;
        let p = parse_installed(json).unwrap();
        assert_eq!(
            p[0],
            Package::new("wget", "1.24.5")
                .desc("Internet file retriever")
                .repo(FORMULA)
        );
        assert_eq!(p[1].name, "graykode/tap/abtop");
        assert_eq!(p[2], Package::new("firefox", "131.0").desc("Web browser").repo(CASK));
        assert_eq!(p[3].name, "macos-fuse-t/cask/fuse-t");
        assert_eq!(p[4].name, "old");
    }

    #[test]
    fn outdated() {
        let json = r#"{"formulae":[{"name":"git","installed_versions":["2.46.0"],"current_version":"2.47.0"}],
                       "casks":[{"name":"iterm2","installed_versions":"3.5.4","current_version":"3.5.5"}]}"#;
        let p = parse_outdated(json).unwrap();
        assert_eq!(p[0], Package::new("git", "2.46.0").repo(FORMULA).upgrade_to("2.47.0"));
        assert_eq!(p[1].new_version.as_deref(), Some("3.5.5"));
    }

    #[test]
    fn refresh_environment_respects_the_users_settings() {
        let none = |_: &str| false;
        // The Updates query: refresh when older than 4 hours (undoing the
        // default HOMEBREW_NO_AUTO_UPDATE that every query gets).
        assert_eq!(
            refresh_env(14_400, none),
            (
                vec![("HOMEBREW_AUTO_UPDATE_SECS", "14400".to_string())],
                vec!["HOMEBREW_NO_AUTO_UPDATE"]
            )
        );
        // The user's own settings win: no auto-update at all, or their interval.
        assert_eq!(
            refresh_env(14_400, |n| n == "HOMEBREW_NO_AUTO_UPDATE"),
            (Vec::new(), Vec::new())
        );
        assert_eq!(
            refresh_env(14_400, |n| n == "HOMEBREW_AUTO_UPDATE_SECS"),
            (Vec::new(), vec!["HOMEBREW_NO_AUTO_UPDATE"])
        );
        // The loading note promises a refresh only when one can happen.
        assert!(refresh_note(none).is_some_and(|n| n.contains("refreshes its catalogue")));
        assert_eq!(refresh_note(|n| n == "HOMEBREW_NO_AUTO_UPDATE"), None);
    }

    #[test]
    fn self_updating_casks_are_listed_and_noted() {
        // `brew outdated --greedy` on this Mac lists three casks that plain
        // `brew outdated` hides.
        let greedy = r#"{"formulae":[{"name":"git","installed_versions":["2.46.0"],"current_version":"2.47.0"}],
            "casks":[{"name":"warp","installed_versions":["0.2026.07.22.09.01.stable_01"],"current_version":"0.2026.11.11.08.17.stable_01"},
                     {"name":"iterm2","installed_versions":["3.5.4"],"current_version":"3.5.5"}]}"#;
        let plain = r#"{"formulae":[{"name":"git","installed_versions":["2.46.0"],"current_version":"2.47.0"}],
            "casks":[{"name":"iterm2","installed_versions":["3.5.4"],"current_version":"3.5.5"}]}"#;
        let p = mark_self_updating(parse_outdated(greedy).unwrap(), &parse_outdated(plain).unwrap());
        let notes: Vec<(&str, Option<&str>)> = p.iter().map(|p| (p.name.as_str(), p.note.as_deref())).collect();
        assert_eq!(
            notes,
            [("git", None), ("warp", Some("updates itself")), ("iterm2", None)]
        );
    }

    #[test]
    fn upgrades_include_self_updating_casks() {
        assert_eq!(Brew.upgrade(&[])[0].display(None), "brew upgrade --greedy");
        let pkgs = [
            Package::new("git", "").repo(FORMULA),
            Package::new("warp", "").repo(CASK),
        ];
        let shown: Vec<String> = Brew.upgrade(&pkgs).iter().map(|c| c.display(None)).collect();
        assert_eq!(shown, ["brew upgrade git", "brew upgrade --cask --greedy warp"]);
        // From the command line the kind is unknown: --greedy, in case it is
        // a cask that updates itself.
        assert_eq!(
            Brew.upgrade(&[Package::new("warp", "")])[0].display(None),
            "brew upgrade --greedy warp"
        );
    }

    #[test]
    fn grouping() {
        let pkgs = [
            Package::new("git", "").repo(FORMULA),
            Package::new("firefox", "").repo(CASK),
        ];
        let c = grouped(&["install"], &pkgs);
        assert_eq!(c[0].display(None), "brew install git");
        assert_eq!(c[1].display(None), "brew install --cask firefox");
    }

    #[test]
    fn explicit_orphans_size() {
        let json = r#"{"formulae":[{"name":"a","full_name":"a","installed":[{"installed_on_request":true}]},
                                   {"name":"dep","full_name":"dep","installed":[{"installed_on_request":false}]},
                                   {"name":"t","full_name":"u/tap/t","installed":[{"installed_on_request":true}]}],
                       "casks":[{"token":"c","full_token":"c"}]}"#;
        assert_eq!(parse_explicit(json).unwrap(), ["a", "u/tap/t", "c"]);
        let o = parse_autoremove("==> Would autoremove 2 unneeded formulae:\nlibfoo\nbar\n");
        assert_eq!(o.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["libfoo", "bar"]);
        let info = "==> tree: stable 2.3.2\nInstalled\n/opt/homebrew/Cellar/tree/2.3.2 (8 files, 175.6KB) *\n";
        assert_eq!(parse_size(info).as_deref(), Some("175.6KB (8 files)"));
        assert_eq!(parse_size("no size here (really)"), None);
    }

    fn tap(name: &str) -> Repo {
        Repo {
            name: name.into(),
            source: "brew".into(),
            ..Repo::default()
        }
    }

    #[test]
    fn taps() {
        let json = r#"[{"name":"graykode/tap","official":false,"formula_names":["graykode/tap/abtop"],"cask_tokens":[],"remote":"https://github.com/graykode/homebrew-tap"},
                       {"name":"homebrew/cask-fonts","official":true,"formula_names":[],"cask_tokens":["a","b"]}]"#;
        let t = parse_taps(json).unwrap();
        assert_eq!(
            (t[0].name.as_str(), t[0].packages, t[0].official),
            ("graykode/tap", 1, false)
        );
        assert_eq!(t[1].packages, 2);
        assert!(Repositories::contains(
            &Brew,
            &tap("graykode/tap"),
            &Package::new("graykode/tap/abtop", "")
        ));
        assert!(!Repositories::contains(
            &Brew,
            &tap("graykode/tap"),
            &Package::new("graykode/tapx/abtop", "")
        ));
        assert!(Repositories::contains(
            &Brew,
            &tap("homebrew/core"),
            &Package::new("ripgrep", "").repo(FORMULA)
        ));
        assert!(Repositories::contains(
            &Brew,
            &tap("homebrew/cask"),
            &Package::new("firefox", "").repo(CASK)
        ));
        assert_eq!(
            Repositories::add(&Brew, "", "user/repo").unwrap()[0].display(None),
            "brew tap user/repo"
        );
        assert_eq!(
            Repositories::add(&Brew, "", "user/repo https://example.com/x.git").unwrap()[0].display(None),
            "brew tap user/repo https://example.com/x.git"
        );
        for bad in ["--force", "justname", "a/b/c", "user/-x", ""] {
            assert!(Repositories::add(&Brew, "", bad).is_err(), "accepted {bad:?}");
        }
        assert_eq!(
            Repositories::remove(&Brew, &tap("user/repo")).unwrap()[0].display(None),
            "brew untap user/repo"
        );
    }

    #[test]
    fn du_lines() {
        let d = parse_du(
            "176\t/opt/homebrew/Cellar/tree\n975872\t/Applications/Blender.app/\nbad line\n12\t/opt/homebrew/Caskroom/blender\n",
        );
        assert_eq!(
            d,
            [
                ("/opt/homebrew/Cellar/tree".to_string(), 176 * 1024),
                ("/Applications/Blender.app".to_string(), 975_872 * 1024),
                ("/opt/homebrew/Caskroom/blender".to_string(), 12 * 1024),
            ]
        );
    }

    #[test]
    fn cask_targets() {
        // Trimmed `brew info --cask --json=v2 --installed`.
        let json = r#"{"formulae":[],"casks":[
          {"token":"bifrost","artifacts":[{"app":["Bifrost.app"],"target":"/Applications/Bifrost.app"},
             {"zap":[{"trash":["~/Library/Application Support/Bifrost"]}]}]},
          {"token":"android-platform-tools","artifacts":[{"binary":["/opt/homebrew/Caskroom/android-platform-tools/37.0.1/platform-tools/adb"],"target":"/opt/homebrew/bin/adb"}]},
          {"token":"font-x","artifacts":[{"font":["X-Bold.ttf"],"target":"/Users/u/Library/Fonts/X-Bold.ttf"},{"font":["X.ttf"],"target":"/Users/u/Library/Fonts/X.ttf"}]},
          {"token":"fuse-t","artifacts":[{"uninstall":[{"pkgutil":["org.fuse-t.*"]}]},{"pkg":["fuse-t-macos-installer-1.2.9.pkg"]}]},
          {"artifacts":[]}]}"#;
        let t = parse_cask_targets(json).unwrap();
        assert_eq!(t["bifrost"], Some(vec!["/Applications/Bifrost.app".to_string()]));
        assert_eq!(
            t["android-platform-tools"],
            Some(Vec::new()),
            "binaries are symlinks into the Caskroom"
        );
        assert_eq!(t["font-x"].as_ref().map(Vec::len), Some(2));
        assert_eq!(t["fuse-t"], None, "pkg installers cannot be sized");
        assert_eq!(t.len(), 4);
        assert!(parse_cask_targets("not json").is_err());
    }
}
