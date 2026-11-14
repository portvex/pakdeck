//! A record of what pakdeck ran, and undoing it.
//!
//! Every action that finished (successfully or not) is one JSON line in
//! `$XDG_STATE_HOME/pakdeck/history.jsonl` (default
//! `~/.local/state/pakdeck/history.jsonl`), newest last. Dry runs are not
//! recorded. Entries are appended (one write per run); once the file holds
//! twice `KEEP` entries it is cut back to the last `KEEP`.
//!
//! Undo plans the opposite action: an install becomes a remove, a remove
//! an install, an upgrade installs the previous versions (where the package
//! manager can), and a hold a release. The undo is recorded too, so it can
//! be undone in turn.

use crate::backend::{Backend, Cmd, Multi, Package, VersionChoice};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Entries kept in the file.
pub const KEEP: usize = 1000;

/// What an entry did, as far as undo is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Install,
    Remove,
    /// Upgrades and installs of a chosen version: undone by installing the
    /// version each package had before.
    Upgrade,
    Hold,
    Unhold,
    /// Cleanup, sync, repositories, imports: recorded, not undoable.
    Other,
}

/// One package an entry acted on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Item {
    pub source: String,
    pub name: String,
    /// The repository or kind (Homebrew "formula" / "cask").
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repo: String,
    /// The installed version before, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The version it was changed to, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl Item {
    /// The package as `op` changes it: `from` is the installed version
    /// before (none for an install), `to` the version after (none for a
    /// removal or a hold). For an install `p.version` is the version being
    /// installed (a search result's), for the others the installed one.
    pub fn of(op: Op, p: &Package) -> Self {
        let version = (!p.version.is_empty()).then(|| p.version.clone());
        let (from, to) = match op {
            Op::Install => (None, version),
            Op::Upgrade => (version, p.new_version.clone()),
            Op::Remove | Op::Hold | Op::Unhold | Op::Other => (version, None),
        };
        Self {
            source: p.source.clone(),
            name: p.name.clone(),
            repo: p.repo.clone(),
            from,
            to,
        }
    }

    fn package(&self) -> Package {
        Package::new(self.name.as_str(), self.from.clone().unwrap_or_default())
            .source(self.source.as_str())
            .repo(self.repo.as_str())
    }
}

/// What an action will record once it has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub op: Op,
    /// The packages it changes. Only those it really changes: a package
    /// that was installed (or held) already is left out, so undo never
    /// removes (or releases) what the user had before.
    pub items: Vec<Item>,
    /// The entry this run undoes.
    pub undoes: Option<u64>,
}

impl Draft {
    pub fn new(op: Op, pkgs: &[Package]) -> Self {
        Self {
            op,
            items: pkgs.iter().map(|p| Item::of(op, p)).collect(),
            undoes: None,
        }
    }

    pub fn other() -> Self {
        Self::new(Op::Other, &[])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Seconds since the Unix epoch.
    pub time: u64,
    /// Identifies the entry (milliseconds since the epoch when recorded).
    #[serde(default)]
    pub id: u64,
    /// The entry this one undid, if it is an undo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<u64>,
    pub op: Op,
    pub title: String,
    #[serde(default)]
    pub items: Vec<Item>,
    /// The commands as shown to the user.
    #[serde(default)]
    pub commands: Vec<String>,
    pub ok: bool,
}

impl Entry {
    pub fn new(draft: Draft, title: &str, cmds: &[Cmd], sudo: Option<&str>, ok: bool) -> Self {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Self {
            time: now(),
            id: millis,
            undoes: draft.undoes,
            op: draft.op,
            title: title.to_string(),
            items: draft.items,
            commands: cmds.iter().map(|c| c.display(sudo)).collect(),
            ok,
        }
    }

    /// Why this entry cannot be undone, if it cannot.
    pub fn not_undoable(&self) -> Option<&'static str> {
        if !self.ok {
            Some("it failed")
        } else if self.op == Op::Other {
            Some("only installs, removals, updates and holds can")
        } else if self.items.is_empty() {
            Some("it changed nothing (the packages were already as asked)")
        } else {
            None
        }
    }

    /// Why this entry cannot be undone, given the whole history (also:
    /// it was undone already).
    pub fn not_undoable_in(&self, all: &[Entry]) -> Option<&'static str> {
        if self.id != 0 && all.iter().any(|e| e.ok && e.undoes == Some(self.id)) {
            return Some("it was undone already");
        }
        self.not_undoable()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `$XDG_STATE_HOME/pakdeck/history.jsonl`, or `~/.local/state/pakdeck/...`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))?;
    Some(base.join("pakdeck").join("history.jsonl"))
}

/// Entries in the file, oldest first. Lines that do not parse (a newer
/// format, a torn write) are skipped; a missing file is an empty history.
pub fn load(path: &Path) -> Result<Vec<Entry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(parse(&text))
}

pub fn parse(text: &str) -> Vec<Entry> {
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Add `entry` as one line at the end (following a symlink, keeping the
/// file's permissions). Past `2 * KEEP` lines the file is rewritten,
/// atomically, with the last `KEEP` entries.
pub fn append(path: &Path, entry: &Entry) -> Result<()> {
    use std::io::Write;
    let mut line = serde_json::to_string(entry).context("encoding a history entry")?;
    line.push('\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))?;
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // Two pakdeck processes at once: if one trims (rewrites) while the other
    // appends, that one line can be lost. Rare, and it only costs history,
    // so no lock file.
    if text.lines().count() > KEEP.saturating_mul(2) {
        let lines: Vec<&str> = text.lines().collect();
        let keep: String = lines
            .iter()
            .skip(lines.len().saturating_sub(KEEP))
            .map(|l| format!("{l}\n"))
            .collect();
        crate::util::write_atomic(path, &keep)?;
    }
    Ok(())
}

/// The packages a run of `op` really changes, which is what undo may take
/// back: an install only what is not installed yet, a hold only what is not
/// held, a release only what is held; everything for the other operations.
/// Shared by the TUI and the command line.
pub fn changed(
    op: Op,
    pkgs: &[Package],
    is_installed: impl Fn(&Package) -> bool,
    is_held: impl Fn(&Package) -> bool,
) -> Vec<Package> {
    pkgs.iter()
        .filter(|p| match op {
            Op::Install => !is_installed(p),
            Op::Hold => !is_held(p),
            Op::Unhold => is_held(p),
            Op::Remove | Op::Upgrade | Op::Other => true,
        })
        .cloned()
        .collect()
}

/// The draft for a command-line run of `op` on `pkgs` (names as typed),
/// recorded the same way the TUI records it: sources filled in (a plain
/// name goes to the first package manager), versions from the installed
/// list, only the packages the run really changes, and for an update of
/// everything the packages the update list offers.
pub fn cli_draft(backend: &Multi, op: &str, pkgs: &[Package]) -> Draft {
    let op = match op {
        "install" => Op::Install,
        "remove" => Op::Remove,
        "upgrade" => Op::Upgrade,
        "hold" => Op::Hold,
        "unhold" => Op::Unhold,
        _ => return Draft::other(),
    };
    if op == Op::Upgrade && pkgs.is_empty() {
        return Draft::new(op, &backend.updates().unwrap_or_default());
    }
    let first = backend.names().first().copied().unwrap_or_default();
    let installed = backend.installed().unwrap_or_default();
    let held = backend.held_tagged().unwrap_or_default();
    let now = |p: &Package| installed.iter().find(|i| i.name == p.name && i.source == p.source);
    let filled: Vec<Package> = pkgs
        .iter()
        .map(|p| {
            let mut p = p.clone();
            if p.source.is_empty() {
                p.source = first.to_string();
            }
            if let Some(i) = now(&p) {
                p.version = i.version.clone();
                p.repo = i.repo.clone();
            }
            p
        })
        .collect();
    let is_held = |p: &Package| held.iter().any(|(s, n)| *s == p.source && *n == p.name);
    Draft::new(op, &changed(op, &filled, |p| now(p).is_some(), is_held))
}

/// Why `entry` cannot be undone here, if it cannot: the reasons the entry
/// itself gives, plus what the active package managers can do. Decided
/// without running anything, so the History popup marks entries up front.
pub fn why_not(backend: &Multi, entry: &Entry, all: &[Entry]) -> Option<String> {
    if let Some(why) = entry.not_undoable_in(all) {
        return Some(why.to_string());
    }
    let part = |source: &str| backend.parts().iter().find(|b| b.name() == source).cloned();
    let here: Vec<&Item> = entry.items.iter().filter(|i| part(&i.source).is_some()).collect();
    if here.is_empty() {
        return Some("none of its package managers is active here".into());
    }
    if entry.op == Op::Upgrade
        && !here
            .iter()
            .any(|i| part(&i.source).is_some_and(|b| b.installs_versions()))
    {
        let mut names: Vec<&str> = here.iter().map(|i| i.source.as_str()).collect();
        names.dedup();
        return Some(format!("{} cannot install an older version", names.join(", ")));
    }
    None
}

/// The commands that undo `entry`: (title, commands, notes about packages
/// that cannot be restored).
pub fn undo_plan(backend: &Multi, entry: &Entry, all: &[Entry]) -> Result<(String, Vec<Cmd>, Vec<String>)> {
    if let Some(why) = why_not(backend, entry, all) {
        bail!("cannot undo: {why}");
    }
    let active = backend.names();
    let (here, gone): (Vec<&Item>, Vec<&Item>) = entry.items.iter().partition(|i| active.contains(&i.source.as_str()));
    let mut notes: Vec<String> = gone
        .iter()
        .map(|i| format!("{}: skipped ({} is not active here)", i.name, i.source))
        .collect();
    let pkgs: Vec<Package> = here.iter().map(|i| i.package()).collect();
    if pkgs.is_empty() {
        bail!("cannot undo: none of its package managers is active here");
    }
    let title = format!("Undo: {}", entry.title);
    let cmds = match entry.op {
        Op::Install => backend.remove(&pkgs),
        Op::Remove => backend.install(&pkgs),
        Op::Hold => backend.hold(&pkgs, false)?,
        Op::Unhold => backend.hold(&pkgs, true)?,
        Op::Upgrade => {
            for source in here
                .iter()
                .map(|i| i.source.as_str())
                .collect::<std::collections::BTreeSet<_>>()
            {
                if let Some(note) = backend
                    .parts()
                    .iter()
                    .find(|b| b.name() == source)
                    .and_then(|b| b.rollback_note())
                {
                    notes.push(format!("{source}: {note}"));
                }
            }
            let mut cmds = Vec::new();
            for (item, pkg) in here.iter().zip(&pkgs) {
                let Some(from) = &item.from else {
                    notes.push(format!("{}: skipped (its earlier version was not recorded)", item.name));
                    continue;
                };
                let choice = VersionChoice::plain(from.as_str());
                match crate::backend::version_plan(backend, pkg, &choice, false, false) {
                    Ok((c, n)) => {
                        cmds.extend(c);
                        notes.extend(n);
                    }
                    Err(e) => notes.push(format!("{} stays: {e:#}", item.name)),
                }
            }
            cmds
        }
        Op::Other => Vec::new(),
    };
    if cmds.is_empty() {
        bail!("cannot undo: {}", notes.join("; "));
    }
    Ok((title, cmds, notes))
}

/// What undoing `entry` does, for recording the undo itself.
pub fn undo_draft(entry: &Entry) -> Draft {
    let swap = |i: &Item| Item {
        from: i.to.clone(),
        to: i.from.clone(),
        ..i.clone()
    };
    let op = match entry.op {
        Op::Install => Op::Remove,
        Op::Remove => Op::Install,
        Op::Hold => Op::Unhold,
        Op::Unhold => Op::Hold,
        Op::Upgrade => Op::Upgrade,
        Op::Other => Op::Other,
    };
    Draft {
        op,
        items: entry.items.iter().map(swap).collect(),
        undoes: Some(entry.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Mock;
    use std::sync::Arc;

    fn entry(op: Op, items: Vec<Item>, ok: bool) -> Entry {
        Entry {
            time: 1_700_000_000,
            id: 1_700_000_000_000,
            undoes: None,
            op,
            title: "Something".into(),
            items,
            commands: vec!["mock do".into()],
            ok,
        }
    }

    fn item(name: &str, from: Option<&str>, to: Option<&str>) -> Item {
        Item {
            source: "one".into(),
            name: name.into(),
            repo: String::new(),
            from: from.map(String::from),
            to: to.map(String::from),
        }
    }

    fn shown(c: &[Cmd]) -> Vec<String> {
        c.iter().map(|c| c.display(None)).collect()
    }

    #[test]
    fn undo_reverses_installs_removals_holds_and_upgrades() {
        let m = Multi::new(vec![Arc::new(Mock::named("one"))]);
        let (title, c, _) = undo_plan(&m, &entry(Op::Install, vec![item("tree", None, None)], true), &[]).unwrap();
        assert_eq!(
            (title.as_str(), shown(&c)),
            ("Undo: Something", vec!["one remove tree".to_string()])
        );
        let (_, c, _) = undo_plan(&m, &entry(Op::Remove, vec![item("tree", Some("1.0"), None)], true), &[]).unwrap();
        assert_eq!(shown(&c), ["one install tree"]);
        let (_, c, _) = undo_plan(&m, &entry(Op::Hold, vec![item("tree", None, None)], true), &[]).unwrap();
        assert_eq!(shown(&c), ["one unhold tree"]);
        // An upgrade goes back to the recorded version; a package without
        // one is skipped with a note.
        let e = entry(
            Op::Upgrade,
            vec![item("pkg01", Some("1.0"), Some("2.0")), item("x", None, Some("2.0"))],
            true,
        );
        let (_, c, notes) = undo_plan(&m, &e, &[]).unwrap();
        assert_eq!(shown(&c), ["one install pkg01=1.0"]);
        assert!(notes.iter().any(|n| n.contains("x: skipped")), "{notes:?}");
    }

    #[test]
    fn what_cannot_be_undone_says_why() {
        let m = Multi::new(vec![Arc::new(Mock::named("one"))]);
        let failed = entry(Op::Install, vec![item("tree", None, None)], false);
        assert!(format!("{:#}", undo_plan(&m, &failed, &[]).unwrap_err()).contains("it failed"));
        let other = entry(Op::Other, Vec::new(), true);
        assert!(
            format!("{:#}", undo_plan(&m, &other, &[]).unwrap_err())
                .contains("only installs, removals, updates and holds can")
        );
        let elsewhere = entry(
            Op::Install,
            vec![Item {
                source: "gone".into(),
                ..item("tree", None, None)
            }],
            true,
        );
        assert!(
            format!("{:#}", undo_plan(&m, &elsewhere, &[]).unwrap_err())
                .contains("none of its package managers is active here")
        );
    }

    #[test]
    fn undo_is_recorded_as_the_opposite() {
        let up = entry(Op::Upgrade, vec![item("pkg01", Some("1.0"), Some("2.0"))], true);
        let d = undo_draft(&up);
        assert_eq!(d.op, Op::Upgrade);
        assert_eq!(
            (d.items[0].from.as_deref(), d.items[0].to.as_deref()),
            (Some("2.0"), Some("1.0"))
        );
        assert_eq!(undo_draft(&entry(Op::Install, Vec::new(), true)).op, Op::Remove);
        assert_eq!(undo_draft(&entry(Op::Unhold, Vec::new(), true)).op, Op::Hold);
    }

    #[test]
    fn file_round_trip_keeps_the_last_entries_and_skips_bad_lines() {
        let dir = std::env::temp_dir().join(format!("pakdeck-history-{}", std::process::id()));
        let path = dir.join("state").join("history.jsonl");
        assert!(load(&path).unwrap().is_empty(), "no file: empty history");
        for i in 0..(2 * KEEP + 1) {
            let mut e = entry(Op::Install, vec![item("tree", None, None)], true);
            e.time = i as u64;
            append(&path, &e).unwrap();
        }
        // Past twice KEEP, cut back to the last KEEP, oldest dropped first.
        let all = load(&path).unwrap();
        assert_eq!(all.len(), KEEP);
        assert_eq!(all.first().map(|e| e.time), Some(KEEP as u64 + 1));
        assert_eq!(all.last().map(|e| e.time), Some(2 * KEEP as u64));
        // A torn or foreign line is skipped, not fatal.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json\n");
        std::fs::write(&path, text).unwrap();
        assert_eq!(load(&path).unwrap().len(), KEEP);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn versions_follow_the_operation() {
        // An install from Search: the version shown is what gets installed.
        let found = Package::new("ripgrep", "14.1.0").source("one");
        let i = Item::of(Op::Install, &found);
        assert_eq!((i.from, i.to.as_deref()), (None, Some("14.1.0")));
        let installed = Package::new("tree", "2.1").source("one");
        let r = Item::of(Op::Remove, &installed);
        assert_eq!((r.from.as_deref(), r.to), (Some("2.1"), None));
        let up = Item::of(Op::Upgrade, &installed.clone().upgrade_to("2.2"));
        assert_eq!((up.from.as_deref(), up.to.as_deref()), (Some("2.1"), Some("2.2")));
        let h = Item::of(Op::Hold, &installed);
        assert_eq!((h.from.as_deref(), h.to), (Some("2.1"), None));
    }

    #[test]
    fn cli_drafts_record_what_really_changes() {
        let m = Multi::new(vec![Arc::new(Mock::named("one"))]);
        // A plain name goes to the first manager; a missing package is kept.
        let d = cli_draft(&m, "install", &[Package::new("newpkg", ""), Package::new("pkg05", "")]);
        assert_eq!(d.op, Op::Install);
        assert_eq!(
            d.items
                .iter()
                .map(|i| (i.source.as_str(), i.name.as_str()))
                .collect::<Vec<_>>(),
            [("one", "newpkg")]
        );
        // A removal records the installed version.
        let d = cli_draft(&m, "remove", &[Package::new("pkg05", "")]);
        assert_eq!(d.items[0].from.as_deref(), Some("1.0"));
        // Holding what is held already (pkg03) changes nothing; releasing it does.
        assert!(cli_draft(&m, "hold", &[Package::new("pkg03", "")]).items.is_empty());
        assert_eq!(cli_draft(&m, "unhold", &[Package::new("pkg03", "")]).items.len(), 1);
        // Updating everything records what the update list offers.
        let d = cli_draft(&m, "upgrade", &[]);
        assert_eq!(
            d.items
                .iter()
                .map(|i| (i.name.as_str(), i.to.as_deref()))
                .collect::<Vec<_>>(),
            [("pkg01", Some("2.0"))]
        );
        assert_eq!(cli_draft(&m, "cleanup", &[]).op, Op::Other);
    }

    #[test]
    fn an_entry_is_undone_once_and_arch_warns_about_partial_downgrades() {
        let m = Multi::new(vec![Arc::new(Mock::named("one"))]);
        let installed = entry(Op::Install, vec![item("tree", None, None)], true);
        let undo = Entry {
            id: 42,
            undoes: Some(installed.id),
            ..entry(Op::Remove, vec![item("tree", None, None)], true)
        };
        let all = vec![installed.clone(), undo];
        let err = undo_plan(&m, &installed, &all).unwrap_err();
        assert!(format!("{err:#}").contains("undone already"), "{err:#}");
        assert_eq!(installed.not_undoable_in(&all), Some("it was undone already"));
        // pacman-like managers: going back for a few packages is a partial
        // downgrade, and undo says so.
        let arch = Multi::new(vec![Arc::new(Mock {
            partial_rollback: true,
            ..Mock::named("one")
        })]);
        let up = entry(Op::Upgrade, vec![item("pkg01", Some("1.0"), Some("2.0"))], true);
        let (_, cmds, notes) = undo_plan(&arch, &up, &[]).unwrap();
        assert_eq!(shown(&cmds), ["one install pkg01=1.0"]);
        assert!(notes.iter().any(|n| n.contains("partial downgrade")), "{notes:?}");
    }

    #[test]
    fn realistic_line_parses() {
        let line = r#"{"time":1763030700,"op":"upgrade","title":"Update stats","items":[{"source":"brew","name":"stats","repo":"cask","from":"3.0.25","to":"3.0.26"}],"commands":["brew upgrade --cask --greedy stats"],"ok":true}"#;
        let e = parse(line);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].items[0].from.as_deref(), Some("3.0.25"));
        assert_eq!(e[0].not_undoable(), None);
    }
}
