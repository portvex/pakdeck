//! Several package managers behind one `Backend`. Results are tagged with
//! their source; operations are routed to the package's own manager.
//!
//! A list query succeeds if any part succeeds. Failures of other parts are
//! kept as warnings (see `take_warnings`); parts that do not support the
//! query are skipped silently.

use super::{Backend, Cmd, Package, Repo, Repositories, Section, SizeMeter, is_unsupported, unsupported};
use anyhow::{Result, bail};
use std::sync::{Arc, Mutex};

/// (source, name) pairs, and the sources that answered.
pub type Covered = (Vec<(String, String)>, Vec<String>);

pub struct Multi {
    parts: Vec<Arc<dyn Backend>>,
    warnings: Mutex<Vec<String>>,
}

impl Multi {
    pub fn new(parts: Vec<Arc<dyn Backend>>) -> Self {
        Self {
            parts,
            warnings: Mutex::new(Vec::new()),
        }
    }

    pub fn parts(&self) -> &[Arc<dyn Backend>] {
        &self.parts
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.parts.iter().map(|p| p.name()).collect()
    }

    pub fn is_multi(&self) -> bool {
        self.parts.len() > 1
    }

    /// Warnings from parts that failed while others succeeded.
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .map(|mut w| std::mem::take(&mut *w))
            .unwrap_or_default()
    }

    fn warn(&self, msg: String) {
        if let Ok(mut w) = self.warnings.lock() {
            w.push(msg);
        }
    }

    /// The part a package belongs to; untagged packages go to the first.
    pub fn part(&self, source: &str) -> Option<&Arc<dyn Backend>> {
        self.parts
            .iter()
            .find(|p| p.name() == source)
            .or_else(|| self.parts.first())
    }

    /// Run `f` on every part and merge the results.
    fn gather<T>(
        &self,
        f: impl Fn(&dyn Backend) -> Result<Vec<T>>,
        tag: impl Fn(T, &'static str) -> T,
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut ok = false;
        let mut failures = Vec::new();
        let mut first_unsupported = None;
        for p in &self.parts {
            match f(p.as_ref()) {
                Ok(v) => {
                    ok = true;
                    out.extend(v.into_iter().map(|x| tag(x, p.name())));
                }
                Err(e) if is_unsupported(&e) => {
                    first_unsupported.get_or_insert(e);
                }
                Err(e) => failures.push(format!("{}: {e:#}", p.name())),
            }
        }
        if ok {
            for f in failures {
                self.warn(f);
            }
            return Ok(out);
        }
        if !failures.is_empty() {
            bail!("{}", failures.join("; "));
        }
        match first_unsupported {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    fn packages(&self, f: impl Fn(&dyn Backend) -> Result<Vec<Package>>) -> Result<Vec<Package>> {
        self.gather(f, |p, src| p.source(src))
    }

    /// Installed-state lists (installed, orphans): one entry per package,
    /// under the part that overrides the others (see `Backend::overrides`).
    fn local_packages(&self, f: impl Fn(&dyn Backend) -> Result<Vec<Package>>) -> Result<Vec<Package>> {
        let all = self.packages(f)?;
        Ok(self.drop_overridden(all, |p| (p.source.clone(), p.name.clone())))
    }

    /// Remove entries of a part that another active part overrides (the
    /// AUR claims the foreign packages pacman also lists).
    fn drop_overridden<T>(&self, items: Vec<T>, id: impl Fn(&T) -> (String, String)) -> Vec<T> {
        let rules: Vec<(&'static str, &'static str)> = self
            .parts
            .iter()
            .flat_map(|p| p.overrides().iter().map(move |o| (p.name(), *o)))
            .filter(|(_, o)| self.parts.iter().any(|p| p.name() == *o))
            .collect();
        if rules.is_empty() {
            return items;
        }
        let claimed: std::collections::HashSet<(String, String)> = items
            .iter()
            .map(&id)
            .filter_map(|(src, name)| {
                rules
                    .iter()
                    .find(|(winner, _)| *winner == src)
                    .map(|(_, loser)| (loser.to_string(), name))
            })
            .collect();
        items.into_iter().filter(|x| !claimed.contains(&id(x))).collect()
    }

    /// (source, name) pairs from a per-part name query.
    fn tagged(&self, f: impl Fn(&dyn Backend) -> Result<Vec<String>>) -> Result<Vec<(String, String)>> {
        self.gather(
            |b| Ok(f(b)?.into_iter().map(|n| (String::new(), n)).collect()),
            |(_, n), src| (src.to_string(), n),
        )
    }

    pub fn explicit_tagged(&self) -> Result<Vec<(String, String)>> {
        Ok(self.drop_overridden(self.tagged(|b| b.explicit())?, |x| x.clone()))
    }

    /// Explicit packages plus the sources that could answer. A filter must
    /// only apply to covered sources: packages of a manager that cannot tell
    /// (or failed) stay visible instead of disappearing.
    pub fn explicit_covered(&self) -> Result<Covered> {
        let mut names = Vec::new();
        let mut covered = Vec::new();
        let mut first_err = None;
        for b in &self.parts {
            match b.explicit() {
                Ok(v) => {
                    covered.push(b.name().to_string());
                    names.extend(v.into_iter().map(|n| (b.name().to_string(), n)));
                }
                Err(e) if is_unsupported(&e) => {
                    first_err.get_or_insert(e);
                }
                Err(e) => {
                    self.warn(format!("{}: {e:#}", b.name()));
                    first_err.get_or_insert(e);
                }
            }
        }
        let names = self.drop_overridden(names, |x| x.clone());
        match (covered.is_empty(), first_err) {
            (true, Some(e)) => Err(e),
            _ => Ok((names, covered)),
        }
    }

    /// Hold commands, plus notes naming packages whose manager cannot hold.
    pub fn hold_with_notes(&self, pkgs: &[Package], on: bool) -> Result<(Vec<Cmd>, Vec<String>)> {
        let mut cmds = Vec::new();
        let mut notes = Vec::new();
        let mut unsupported_err = None;
        for (b, v) in self.grouped(pkgs) {
            match b.hold(&v, on) {
                Ok(c) => cmds.extend(c),
                Err(e) if is_unsupported(&e) => {
                    let names: Vec<&str> = v.iter().map(|p| p.name.as_str()).collect();
                    notes.push(format!("{}: skipped {} ({e})", b.name(), names.join(", ")));
                    unsupported_err.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        match (cmds.is_empty(), unsupported_err) {
            (true, Some(e)) => Err(e),
            _ => Ok((cmds, notes)),
        }
    }

    pub fn held_tagged(&self) -> Result<Vec<(String, String)>> {
        Ok(self.drop_overridden(self.tagged(|b| b.held())?, |x| x.clone()))
    }

    /// Packages grouped by the part that owns them, in part order.
    fn grouped(&self, pkgs: &[Package]) -> Vec<(&Arc<dyn Backend>, Vec<Package>)> {
        self.parts
            .iter()
            .map(|part| {
                let mine: Vec<Package> = pkgs
                    .iter()
                    .filter(|p| self.part(&p.source).is_some_and(|owner| Arc::ptr_eq(owner, part)))
                    .cloned()
                    .collect();
                (part, mine)
            })
            .filter(|(_, v)| !v.is_empty())
            .collect()
    }

    fn routed(&self, pkgs: &[Package], f: impl Fn(&dyn Backend, &[Package]) -> Vec<Cmd>) -> Vec<Cmd> {
        self.grouped(pkgs)
            .into_iter()
            .flat_map(|(b, v)| f(b.as_ref(), &v))
            .collect()
    }
}

impl Backend for Multi {
    fn name(&self) -> &'static str {
        match self.parts.as_slice() {
            [only] => only.name(),
            _ => "multi",
        }
    }

    fn search(&self, query: &str) -> Result<Vec<Package>> {
        self.packages(|b| b.search(query))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        self.local_packages(|b| b.installed())
    }

    fn updates(&self) -> Result<Vec<Package>> {
        self.packages(|b| b.updates())
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        self.local_packages(|b| b.orphans())
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(self.explicit_tagged()?.into_iter().map(|(_, n)| n).collect())
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(self.held_tagged()?.into_iter().map(|(_, n)| n).collect())
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        match self.part(&pkg.source) {
            Some(b) => b.info(pkg),
            None => bail!("no package manager configured"),
        }
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        match self.part(&pkg.source) {
            Some(b) => b.section(pkg, s),
            None => bail!("no package manager configured"),
        }
    }

    fn versions(&self, pkg: &Package) -> Result<Vec<super::VersionChoice>> {
        match self.part(&pkg.source) {
            Some(b) => b.versions(pkg),
            None => bail!("no package manager configured"),
        }
    }

    fn install_version(&self, pkg: &Package, version: &str) -> Result<Vec<Cmd>> {
        match self.part(&pkg.source) {
            Some(b) => b.install_version(pkg, version),
            None => bail!("no package manager configured"),
        }
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        self.routed(pkgs, |b, p| b.install(p))
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        self.routed(pkgs, |b, p| b.remove(p))
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            return self.parts.iter().flat_map(|b| b.upgrade(&[])).collect();
        }
        self.routed(pkgs, |b, p| b.upgrade(p))
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        Ok(self.hold_with_notes(pkgs, on)?.0)
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        let mut cmds = Vec::new();
        for b in &self.parts {
            match b.cleanup() {
                Ok(c) => cmds.extend(c),
                Err(e) if self.is_multi() => self.warn(format!("{}: {e:#}", b.name())),
                Err(e) => return Err(e),
            }
        }
        Ok(cmds)
    }

    fn sync(&self) -> Vec<Cmd> {
        self.parts.iter().flat_map(|b| b.sync()).collect()
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds.into_iter()
            .flat_map(|c| match self.parts.iter().find(|b| b.owns(&c)) {
                Some(b) => b.assume_yes(vec![c]),
                None => vec![c],
            })
            .collect()
    }

    fn owns(&self, cmd: &Cmd) -> bool {
        self.parts.iter().any(|b| b.owns(cmd))
    }

    /// With several managers, say which one the wait is for.
    fn updates_note(&self) -> Option<String> {
        let (part, note) = self.parts.iter().find_map(|b| b.updates_note().map(|n| (b, n)))?;
        Some(if self.is_multi() {
            format!("{}: {note}", part.name())
        } else {
            note
        })
    }

    fn installs_versions(&self) -> bool {
        self.parts.iter().any(|b| b.installs_versions())
    }

    fn needs_root(&self) -> bool {
        self.parts.iter().any(|b| b.needs_root())
    }

    fn repositories(&self) -> Option<&dyn Repositories> {
        self.parts
            .iter()
            .any(|b| b.repositories().is_some())
            .then_some(self as &dyn Repositories)
    }

    fn size_meter(&self) -> Option<&dyn SizeMeter> {
        self.parts
            .iter()
            .any(|b| b.size_meter().is_some())
            .then_some(self as &dyn SizeMeter)
    }
}

/// Repositories of every part that has them, routed by `Repo::source`.
impl Repositories for Multi {
    fn kind(&self) -> &'static str {
        let kinds: Vec<&'static str> = self
            .parts
            .iter()
            .filter_map(|b| b.repositories())
            .map(|r| r.kind())
            .collect();
        match kinds.as_slice() {
            [one] => one,
            _ => "Repositories",
        }
    }

    /// The default target's (see `add`).
    fn add_hint(&self) -> &'static str {
        self.repo_part("").map_or("", |r| r.add_hint())
    }

    fn list(&self) -> Result<Vec<Repo>> {
        self.gather(
            |b| match b.repositories() {
                Some(r) => r.list(),
                None => Ok(Vec::new()),
            },
            |mut r, src| {
                r.source = src.to_string();
                r
            },
        )
    }

    fn add(&self, source: &str, spec: &str) -> Result<Vec<Cmd>> {
        match self.repo_part(source) {
            Some(r) => r.add(source, spec),
            None if source.is_empty() => unsupported("no active package manager has repositories"),
            None => unsupported(format!("{source} has no repositories here")),
        }
    }

    fn remove(&self, repo: &Repo) -> Result<Vec<Cmd>> {
        match self.repo_part(&repo.source) {
            Some(r) => r.remove(repo),
            None => unsupported(format!("{} has no repositories here", repo.source)),
        }
    }

    fn contains(&self, repo: &Repo, pkg: &Package) -> bool {
        pkg.source == repo.source && self.repo_part(&repo.source).is_some_and(|r| r.contains(repo, pkg))
    }
}

impl Multi {
    /// The repositories of the part named `source` (empty: the first part
    /// that has repositories).
    fn repo_part(&self, source: &str) -> Option<&dyn Repositories> {
        self.parts
            .iter()
            .filter(|b| source.is_empty() || b.name() == source)
            .find_map(|b| b.repositories())
    }

    /// Fill in missing sizes, measuring where a part can (`--disk`).
    pub fn fill_sizes(&self, pkgs: &mut [Package]) {
        let Some(meter) = self.size_meter() else { return };
        let missing: Vec<Package> = pkgs
            .iter()
            .filter(|p| p.size.is_none() && meter.measures(&p.source))
            .cloned()
            .collect();
        if missing.is_empty() {
            return;
        }
        match meter.measure(&missing) {
            Ok(sizes) => super::apply_sizes(pkgs, &sizes.into_iter().collect()),
            Err(e) => self.warn(format!("measuring sizes: {e:#}")),
        }
    }
}

/// Measures each part's packages with that part's meter.
impl SizeMeter for Multi {
    fn measures(&self, source: &str) -> bool {
        self.parts
            .iter()
            .any(|b| b.name() == source && b.size_meter().is_some_and(|m| m.measures(source)))
    }

    fn measure(&self, pkgs: &[Package]) -> Result<Vec<(String, u64)>> {
        self.gather(
            |b| {
                let mine: Vec<Package> = pkgs.iter().filter(|p| p.source == b.name()).cloned().collect();
                match b.size_meter() {
                    Some(m) if !mine.is_empty() => m.measure(&mine),
                    _ => Ok(Vec::new()),
                }
            },
            |x, _| x,
        )
    }
}

/// `source:name` on the command line picks a backend when several are
/// active; a plain name goes to the first (system) one.
pub fn parse_target(multi: &Multi, arg: &str) -> Package {
    match split_source(multi, arg) {
        ("", name) => Package::new(name, ""),
        (src, name) => Package::new(name, "").source(src),
    }
}

/// Split `source:rest` when `source` names an active package manager;
/// otherwise ("", arg). A URL's own colon never matches a manager name.
pub fn split_source<'a>(multi: &Multi, arg: &'a str) -> (&'a str, &'a str) {
    match arg.split_once(':') {
        Some((src, rest)) if multi.names().contains(&src) => (src, rest),
        _ => ("", arg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Mock;

    fn two() -> Multi {
        Multi::new(vec![Arc::new(Mock::named("one")), Arc::new(Mock::named("two"))])
    }

    #[test]
    fn merges_and_tags() {
        let m = two();
        let installed = m.installed().unwrap();
        assert_eq!(installed.len(), 100);
        assert!(installed.iter().take(50).all(|p| p.source == "one"));
        assert!(installed.iter().skip(50).all(|p| p.source == "two"));
        let explicit = m.explicit_tagged().unwrap();
        assert!(explicit.contains(&("two".into(), "pkg02".into())));
    }

    #[test]
    fn routes_by_source() {
        let m = two();
        let pkgs = [
            Package::new("a", "").source("two"),
            Package::new("b", "").source("one"),
            Package::new("c", "").source("two"),
            Package::new("d", ""),
        ];
        let cmds: Vec<String> = m.install(&pkgs).iter().map(|c| c.display(None)).collect();
        assert_eq!(cmds, ["one install b d", "two install a c"]);
        let all: Vec<String> = m.upgrade(&[]).iter().map(|c| c.display(None)).collect();
        assert_eq!(all, ["one upgrade", "two upgrade"]);
    }

    #[test]
    fn partial_failure_is_a_warning() {
        let m = Multi::new(vec![
            Arc::new(Mock::named("ok")),
            Arc::new(Mock {
                fail: true,
                ..Mock::named("bad")
            }),
        ]);
        assert_eq!(m.installed().unwrap().len(), 50);
        let w = m.take_warnings();
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("bad:"), "{w:?}");
        // All parts failing is an error; all unsupported stays unsupported.
        let bad = Multi::new(vec![Arc::new(Mock {
            fail: true,
            ..Mock::named("bad")
        })]);
        assert!(bad.installed().is_err());
        let none = Multi::new(vec![Arc::new(Mock {
            unsupported: true,
            ..Mock::named("u")
        })]);
        assert!(is_unsupported(&none.orphans().unwrap_err()));
    }

    #[test]
    fn targets_and_yes() {
        let m = two();
        assert_eq!(parse_target(&m, "two:pkg").source, "two");
        assert_eq!(parse_target(&m, "nope:pkg").name, "nope:pkg");
        let cmds = m.assume_yes(m.install(&[Package::new("x", "").source("two")]));
        assert_eq!(cmds[0].display(None), "two install x --yes");
    }

    #[test]
    fn coverage_and_hold_notes() {
        let m = Multi::new(vec![
            Arc::new(Mock::named("one")),
            Arc::new(Mock {
                unsupported: true,
                ..Mock::named("u")
            }),
        ]);
        let (names, covered) = m.explicit_covered().unwrap();
        assert_eq!(covered, ["one"]);
        assert!(names.iter().all(|(s, _)| s == "one"));
        let pkgs = [Package::new("a", "").source("one"), Package::new("b", "").source("u")];
        let (cmds, notes) = m.hold_with_notes(&pkgs, true).unwrap();
        assert_eq!(cmds.len(), 1);
        assert!(notes[0].starts_with("u: skipped b"), "{notes:?}");
    }

    #[test]
    fn sizes_are_measured_by_each_parts_meter() {
        let m = Multi::new(vec![
            Arc::new(Mock {
                sizeless: true,
                ..Mock::named("one")
            }),
            Arc::new(Mock::named("two")),
        ]);
        let meter = m.size_meter().unwrap();
        assert!(meter.measures("one") && !meter.measures("two") && !meter.measures("gone"));
        let pkgs = [
            Package::new("pkg07", "").source("one"),
            Package::new("pkg08", "").source("two"),
        ];
        assert_eq!(meter.measure(&pkgs).unwrap(), [("one//pkg07".to_string(), 700)]);
        // --disk: missing sizes filled where a part can measure them.
        let mut installed = m.installed().unwrap();
        m.fill_sizes(&mut installed);
        assert!(installed.iter().filter(|p| p.source == "one").all(|p| p.size.is_some()));
        // No part measures: no meter at all.
        assert!(two().size_meter().is_none());
    }

    #[test]
    fn repositories_route_by_source() {
        let m = Multi::new(vec![
            Arc::new(Mock::named("one")),
            Arc::new(Mock {
                unsupported: true,
                ..Mock::named("plain")
            }),
            Arc::new(Mock::named("two")),
        ]);
        let r = m.repositories().unwrap();
        let list = r.list().unwrap();
        assert_eq!(
            list.iter().map(|x| x.source.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        let shown = |c: Vec<Cmd>| c.iter().map(|c| c.display(None)).collect::<Vec<_>>();
        assert_eq!(shown(r.add("two", "a/b").unwrap()), ["two repo-add a/b"]);
        assert_eq!(
            shown(r.add("", "a/b").unwrap()),
            ["one repo-add a/b"],
            "empty: the first"
        );
        assert!(r.add("plain", "a/b").is_err_and(|e| is_unsupported(&e)));
        let two_extra = &list[1];
        assert_eq!(shown(r.remove(two_extra).unwrap()), ["two repo-remove mock/extra"]);
        // Only packages of the repository's own source belong to it.
        assert!(r.contains(two_extra, &Package::new("pkg02", "").source("two")));
        assert!(!r.contains(two_extra, &Package::new("pkg02", "").source("one")));
        // No part with repositories: none, so callers can say so.
        let none = Multi::new(vec![Arc::new(Mock {
            unsupported: true,
            ..Mock::named("plain")
        })]);
        assert!(none.repositories().is_none());
    }

    #[test]
    fn updates_note_names_the_manager_when_several_are_active() {
        let slow = || {
            Arc::new(Mock {
                slow_updates: true,
                ..Mock::named("brewish")
            })
        };
        let alone = Multi::new(vec![slow()]);
        assert_eq!(alone.updates_note().as_deref(), Some("Checking slowly."));
        let both = Multi::new(vec![slow(), Arc::new(Mock::named("one"))]);
        assert_eq!(both.updates_note().as_deref(), Some("brewish: Checking slowly."));
        assert_eq!(two().updates_note(), None);
    }

    #[test]
    fn search_keeps_both_sources_installed_lists_do_not() {
        let m = Multi::new(vec![
            Arc::new(Mock::named("pacman")),
            Arc::new(Mock {
                overrides: &["pacman"],
                ..Mock::named("aur")
            }),
        ]);
        let found = m.search("x").unwrap();
        assert_eq!(found.len(), 60, "a repository package is never hidden by an AUR one");
        assert_eq!(m.orphans().unwrap().len(), 2);
        assert!(m.orphans().unwrap().iter().all(|p| p.source == "aur"));
    }

    #[test]
    fn overrides_drop_duplicates() {
        // "aur" overrides "pacman": a package both list shows once, as aur.
        let m = Multi::new(vec![
            Arc::new(Mock::named("pacman")),
            Arc::new(Mock {
                overrides: &["pacman"],
                ..Mock::named("aur")
            }),
        ]);
        let installed = m.installed().unwrap();
        assert_eq!(installed.len(), 50, "each name once");
        assert!(installed.iter().all(|p| p.source == "aur"));
        let explicit = m.explicit_tagged().unwrap();
        assert!(explicit.iter().all(|(s, _)| s == "aur"));
        // Holds follow: pacman's IgnorePkg entry for an AUR package is aur's.
        assert_eq!(m.held_tagged().unwrap(), [("aur".to_string(), "pkg03".to_string())]);
        // Without the overriding part active, nothing is dropped.
        let alone = Multi::new(vec![Arc::new(Mock {
            overrides: &["pacman"],
            ..Mock::named("aur")
        })]);
        assert_eq!(alone.installed().unwrap().len(), 50);
    }
}
