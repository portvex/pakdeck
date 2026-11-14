//! Test support shared by unit tests and fuzz targets. Not part of the public API.

use crate::app::App;
use crate::backend::{Backend, Cmd, Multi, Package, Repo, Repositories, Section, SizeMeter, unsupported};
use crate::config::Config;
use anyhow::{Result, bail};
use std::sync::Arc;

/// An in-memory backend with deterministic data and no side effects.
/// `unsupported`: every optional operation reports `Unsupported`.
/// `fail`: list queries fail.
pub struct Mock {
    pub name: &'static str,
    pub unsupported: bool,
    pub fail: bool,
    pub overrides: &'static [&'static str],
    /// The installed list has no sizes; `sizes` measures them (like brew).
    pub sizeless: bool,
    /// Runs as the user, like brew: no command needs root.
    pub user_only: bool,
    /// The Updates query may take a while (like brew's refresh).
    pub slow_updates: bool,
    /// Going back to earlier versions needs a warning (like pacman).
    pub partial_rollback: bool,
}

impl Default for Mock {
    fn default() -> Self {
        Self::named("mock")
    }
}

impl Mock {
    pub fn named(name: &'static str) -> Self {
        Self {
            name,
            unsupported: false,
            fail: false,
            overrides: &[],
            sizeless: false,
            user_only: false,
            slow_updates: false,
            partial_rollback: false,
        }
    }
}

impl Backend for Mock {
    fn name(&self) -> &'static str {
        self.name
    }
    fn search(&self, q: &str) -> Result<Vec<Package>> {
        Ok((0..30)
            .map(|i| Package::new(format!("{q}{i}"), "1.0").desc("searched"))
            .collect())
    }
    fn installed(&self) -> Result<Vec<Package>> {
        if self.fail {
            bail!("mock failure");
        }
        // Sizes and install times in reverse order of the names, so sorting
        // by them visibly changes the list.
        Ok((0..50u64)
            .map(|i| {
                Package::new(format!("pkg{i:02}"), "1.0")
                    .desc("installed one")
                    .sized((!self.sizeless).then(|| 1000u64.saturating_mul(50u64.saturating_sub(i))))
                    .installed_at(Some(1_700_000_000u64.saturating_add(i)))
            })
            .collect())
    }
    fn updates(&self) -> Result<Vec<Package>> {
        Ok(vec![Package::new("pkg01", "1.0").upgrade_to("2.0")])
    }
    fn info(&self, p: &Package) -> Result<String> {
        Ok(format!("info for {}", p.name))
    }
    fn install(&self, p: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root(self.name, &["install"]).names(p)]
    }
    fn remove(&self, p: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root(self.name, &["remove"]).names(p)]
    }
    fn upgrade(&self, p: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root(self.name, &["upgrade"]).names(p)]
    }
    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![Cmd::root(self.name, &["clean"])])
    }
    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root(self.name, &["sync"])]
    }
    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds.into_iter().map(|c| c.strings(["--yes".to_string()])).collect()
    }
    fn explicit(&self) -> Result<Vec<String>> {
        if self.unsupported {
            return unsupported("mock: no explicit list");
        }
        Ok((0..50).step_by(2).map(|i| format!("pkg{i:02}")).collect())
    }
    fn orphans(&self) -> Result<Vec<Package>> {
        if self.unsupported {
            return unsupported("mock: no orphans");
        }
        Ok(vec![Package::new("pkg48", "1.0"), Package::new("pkg49", "1.0")])
    }
    fn held(&self) -> Result<Vec<String>> {
        if self.unsupported {
            return unsupported("mock: no holds");
        }
        Ok(vec!["pkg03".into()])
    }
    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        if self.unsupported {
            return unsupported("mock: no holds");
        }
        Ok(vec![
            Cmd::user(self.name, &[if on { "hold" } else { "unhold" }]).names(pkgs),
        ])
    }
    fn versions(&self, _p: &Package) -> Result<Vec<crate::backend::VersionChoice>> {
        if self.unsupported {
            return unsupported("mock: no versions");
        }
        Ok(["2.0", "1.0", "0.9"]
            .into_iter()
            .map(crate::backend::VersionChoice::plain)
            .collect())
    }
    fn installs_versions(&self) -> bool {
        true
    }
    fn install_version(&self, p: &Package, v: &str) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root(self.name, &["install"]).strings([format!("{}={v}", p.name)]),
        ])
    }
    fn rollback_note(&self) -> Option<&'static str> {
        self.partial_rollback
            .then_some("going back for only these packages is a partial downgrade")
    }
    fn updates_note(&self) -> Option<String> {
        self.slow_updates.then(|| "Checking slowly.".to_string())
    }
    fn needs_root(&self) -> bool {
        !self.user_only
    }
    fn overrides(&self) -> &'static [&'static str] {
        self.overrides
    }
    fn repositories(&self) -> Option<&dyn Repositories> {
        (!self.unsupported).then_some(self as &dyn Repositories)
    }
    fn size_meter(&self) -> Option<&dyn SizeMeter> {
        self.sizeless.then_some(self as &dyn SizeMeter)
    }
    fn section(&self, p: &Package, s: Section) -> Result<Vec<String>> {
        if self.unsupported {
            return unsupported("mock: no sections");
        }
        Ok(match s {
            Section::Size => vec!["1.0 MiB".into()],
            // pkgNN -> libmock -> libbase -> libmock (a cycle, for the
            // tree); pkgNN -> libz, which has no dependencies.
            Section::Depends => match p.name.as_str() {
                "libmock" => vec!["libbase".into()],
                "libbase" => vec!["libmock".into()],
                "libz" => Vec::new(),
                _ => vec!["libmock".into(), "libz".into()],
            },
            Section::RequiredBy => Vec::new(),
            Section::Files => (0..40).map(|i| format!("/usr/share/{}/file{i}", p.name)).collect(),
        })
    }
}

impl Repositories for Mock {
    fn kind(&self) -> &'static str {
        "Repositories"
    }
    fn add_hint(&self) -> &'static str {
        "owner/name"
    }
    fn list(&self) -> Result<Vec<Repo>> {
        Ok(vec![Repo {
            name: "mock/extra".into(),
            packages: 10,
            source: self.name.into(),
            ..Repo::default()
        }])
    }
    fn add(&self, _source: &str, spec: &str) -> Result<Vec<Cmd>> {
        if !spec.contains('/') {
            bail!("a repository is named owner/name, not {spec:?}");
        }
        Ok(vec![Cmd::user(self.name, &["repo-add", spec])])
    }
    fn remove(&self, repo: &Repo) -> Result<Vec<Cmd>> {
        Ok(vec![Cmd::user(self.name, &["repo-remove", repo.name.as_str()])])
    }
    /// Even-numbered packages come from "mock/extra".
    fn contains(&self, repo: &Repo, p: &Package) -> bool {
        repo.name == "mock/extra"
            && p.name
                .strip_prefix("pkg")
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| n % 2 == 0)
    }
}

/// pkgNN measures NN * 100 bytes.
impl SizeMeter for Mock {
    fn measures(&self, source: &str) -> bool {
        source == self.name
    }
    fn measure(&self, pkgs: &[Package]) -> Result<Vec<(String, u64)>> {
        Ok(pkgs
            .iter()
            .filter_map(|p| {
                let n: u64 = p.name.strip_prefix("pkg")?.parse().ok()?;
                Some((p.key(), n.saturating_mul(100)))
            })
            .collect())
    }
}

/// An app on the given backends in dry-run mode with inline (deterministic)
/// loading that never starts processes.
pub fn mock_app_multi(parts: Vec<Mock>) -> App {
    let cfg = Config {
        dry_run: true,
        ..Config::default()
    };
    let parts: Vec<Arc<dyn Backend>> = parts.into_iter().map(|m| Arc::new(m) as Arc<dyn Backend>).collect();
    let mut app = App::with_options(Arc::new(Multi::new(parts)), cfg, None, true);
    app.sandbox = true;
    app
}

pub fn mock_app_with(backend: Mock) -> App {
    mock_app_multi(vec![backend])
}

pub fn mock_app() -> App {
    mock_app_with(Mock::default())
}
