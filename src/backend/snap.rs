//! Snap packages (snapd). Base snaps, snapd itself and other system snaps
//! count as dependencies, not explicit installs.

use super::{Backend, Cmd, Package, Section, VersionChoice, aligned_table, col, run, unsupported, walk_files};
use anyhow::Result;

pub struct Snap;

const CLASSIC: &str = "classic";

/// Snaps that are infrastructure rather than something the user asked for.
fn is_system(name: &str, notes: &str) -> bool {
    notes
        .split(',')
        .any(|n| matches!(n.trim(), "base" | "core" | "snapd" | "gadget" | "kernel"))
        || name == "snapd"
        || name == "bare"
        || name.starts_with("core")
}

impl Backend for Snap {
    fn name(&self) -> &'static str {
        "snap"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        Ok(parse_find(&run("snap", &["find", q.trim()])?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(parse_list(&run("snap", &["list"])?)
            .into_iter()
            .map(|(p, _)| p)
            .collect())
    }

    fn updates(&self) -> Result<Vec<Package>> {
        Ok(parse_list(&run("snap", &["refresh", "--list"])?)
            .into_iter()
            .map(|(p, _)| Package::new(p.name, "").upgrade_to(p.version))
            .collect())
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        run("snap", &["info", &pkg.name])
    }

    /// Classic-confinement snaps (`Package::repo == "classic"`) need
    /// `--classic`, and one refusal would fail a shared command, so they are
    /// installed separately.
    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        let (classic, strict): (Vec<Package>, Vec<Package>) = pkgs.iter().cloned().partition(|p| p.repo == CLASSIC);
        let mut v = Vec::new();
        if !strict.is_empty() {
            v.push(Cmd::root("snap", &["install"]).names(&strict));
        }
        if !classic.is_empty() {
            v.push(Cmd::root("snap", &["install", "--classic"]).names(&classic));
        }
        v
    }

    /// `name:classic` for classic-confinement snaps.
    fn export_name(&self, pkg: &Package) -> String {
        if pkg.repo == CLASSIC {
            format!("{}:{CLASSIC}", pkg.name)
        } else {
            pkg.name.clone()
        }
    }

    fn import_package(&self, entry: &str) -> Package {
        match entry.split_once(':') {
            Some((name, CLASSIC)) => Package::new(name, "").repo(CLASSIC),
            _ => Package::new(entry, ""),
        }
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("snap", &["remove"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("snap", &["refresh"]).names(pkgs)]
    }

    /// snapd keeps old revisions around (disabled); remove them.
    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(parse_disabled(&run("snap", &["list", "--all"])?)
            .into_iter()
            .map(|(name, rev)| Cmd::root("snap", &["remove", &name, &format!("--revision={rev}")]))
            .collect())
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(parse_list(&run("snap", &["list"])?)
            .into_iter()
            .filter(|(p, notes)| !is_system(&p.name, notes))
            .map(|(p, _)| p.name)
            .collect())
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(parse_list(&run("snap", &["list"])?)
            .into_iter()
            .filter(|(_, notes)| notes.contains("held"))
            .map(|(p, _)| p.name)
            .collect())
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("snap", &["refresh", if on { "--hold" } else { "--unhold" }]).names(pkgs),
        ])
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        match s {
            Section::Files => Ok(walk_files(
                &std::path::Path::new("/snap").join(&pkg.name).join("current"),
                5000,
            )),
            _ => unsupported(format!("snap: '{}' is not available", s.title())),
        }
    }

    /// Channels with a release, as "channel version" (newest track first
    /// as snapd lists them).
    fn versions(&self, pkg: &Package) -> Result<Vec<VersionChoice>> {
        Ok(parse_channels(&run("snap", &["info", &pkg.name])?))
    }

    /// Switch to (or install from) the chosen channel; `channel` is a
    /// `VersionChoice::id`. Classic snaps need --classic to install.
    fn installs_versions(&self) -> bool {
        true
    }

    fn install_version(&self, pkg: &Package, channel: &str) -> Result<Vec<Cmd>> {
        super::validate_version(channel)?;
        // A classic snap needs --classic to install; the CLI does not know the
        // confinement, so ask snapd when it is not given.
        let classic = pkg.repo == CLASSIC
            || (!pkg.installed
                && run("snap", &["info", "--verbose", &pkg.name])
                    .is_ok_and(|info| info.lines().any(|l| l.trim() == "confinement: classic")));
        let mut cmd = if pkg.installed {
            Cmd::root("snap", &["refresh"])
        } else if classic {
            Cmd::root("snap", &["install", "--classic"])
        } else {
            Cmd::root("snap", &["install"])
        };
        cmd = cmd
            .names(std::slice::from_ref(pkg))
            .strings([format!("--channel={channel}")]);
        Ok(vec![cmd])
    }

    fn sync(&self) -> Vec<Cmd> {
        // snapd refreshes its catalogue by itself.
        Vec::new()
    }
}

/// `snap find`: Name, Version, Publisher, Notes, Summary
pub(crate) fn parse_find(out: &str) -> Vec<Package> {
    aligned_table(out)
        .iter()
        .filter(|r| !col(r, "Name").is_empty() && !col(r, "Version").is_empty())
        .map(|r| {
            // Classic snaps must be installed with --classic (see install).
            let classic = col(r, "Notes").split(',').any(|n| n.trim() == CLASSIC);
            Package::new(col(r, "Name"), col(r, "Version"))
                .desc(col(r, "Summary"))
                .repo(if classic { CLASSIC } else { "" })
        })
        .collect()
}

/// `snap list` / `snap refresh --list`: Name, Version, Rev, ..., Notes.
/// Returns each package with its Notes column.
pub(crate) fn parse_list(out: &str) -> Vec<(Package, String)> {
    aligned_table(out)
        .iter()
        .filter(|r| !col(r, "Name").is_empty() && !col(r, "Version").is_empty())
        .map(|r| {
            let notes = col(r, "Notes");
            let mut p = Package::new(col(r, "Name"), col(r, "Version"));
            if notes.split(',').any(|n| n.trim() == CLASSIC) {
                p = p.repo(CLASSIC);
            }
            (p, notes.to_string())
        })
        .collect()
}

/// `snap list --all`: (name, revision) of disabled revisions.
pub(crate) fn parse_disabled(out: &str) -> Vec<(String, String)> {
    aligned_table(out)
        .iter()
        .filter(|r| col(r, "Notes").contains("disabled"))
        .map(|r| (col(r, "Name").to_string(), col(r, "Rev").to_string()))
        .collect()
}

/// `snap info` channels section: "  latest/stable:    2.10 2017-05-11 (42) 20kB -"
/// -> "latest/stable 2.10"; closed channels ("^", "--") are skipped.
pub(crate) fn parse_channels(out: &str) -> Vec<VersionChoice> {
    let Some((_, rest)) = out.split_once("channels:") else {
        return Vec::new();
    };
    rest.lines()
        .take_while(|l| l.starts_with(char::is_whitespace) || l.is_empty())
        .filter_map(|l| {
            let (channel, info) = l.trim().split_once(':')?;
            let version = info.split_whitespace().next()?;
            (!matches!(version, "^" | "--" | "\u{2191}")).then(|| VersionChoice {
                id: channel.trim().to_string(),
                version: version.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find() {
        let out = "Name                                 Version               Publisher                  Notes    Summary\nhello                                2.10                  canonical**                -        GNU Hello, the \"hello world\" snap\nhello-world                          6.4                   canonical**                -        The 'hello-world' of snaps\n";
        let p = parse_find(out);
        assert_eq!(
            p[0],
            Package::new("hello", "2.10").desc("GNU Hello, the \"hello world\" snap")
        );
        assert_eq!(p.len(), 2);
        assert!(parse_find("No matching snaps for \"zzz\"\n").is_empty());
        // Classic snaps are marked so install adds --classic.
        let c = parse_find(
            "Name  Version  Publisher  Notes    Summary\ncode  1.95     vscode**   classic  Code editing.\n",
        );
        assert_eq!(c[0].repo, CLASSIC);
    }

    #[test]
    fn list_explicit_held_disabled() {
        let out = "Name    Version   Rev    Tracking       Publisher   Notes\ncore22  20240111  1122   latest/stable  canonical** base\nhello   2.10      42     latest/stable  canonical** held\nsnapd   2.76.3    23545  latest/stable  canonical** snapd\n";
        let l = parse_list(out);
        assert_eq!(l.len(), 3);
        let explicit: Vec<&str> = l
            .iter()
            .filter(|(p, n)| !is_system(&p.name, n))
            .map(|(p, _)| p.name.as_str())
            .collect();
        assert_eq!(explicit, ["hello"]);
        assert!(l.iter().any(|(p, n)| p.name == "hello" && n.contains("held")));
        let all = "Name   Version Rev Tracking       Publisher   Notes\nhello  2.9     40  latest/stable  canonical** disabled\nhello  2.10    42  latest/stable  canonical** -\n";
        assert_eq!(parse_disabled(all), [("hello".to_string(), "40".to_string())]);
        assert!(parse_list("All snaps up to date.\n").is_empty());
    }

    #[test]
    fn classic_snaps() {
        let l = parse_list(
            "Name  Version Rev Tracking       Publisher  Notes\ncode  1.95    170 latest/stable  vscode**   classic\nhello 2.10    42  latest/stable  canonical  -\n",
        );
        assert_eq!(l[0].0.repo, CLASSIC);
        assert_eq!(Snap.export_name(&l[0].0), "code:classic");
        assert_eq!(Snap.export_name(&l[1].0), "hello");
        let pkgs = [Snap.import_package("code:classic"), Snap.import_package("hello")];
        let cmds: Vec<String> = Snap.install(&pkgs).iter().map(|c| c.display(None)).collect();
        assert_eq!(cmds, ["snap install hello", "snap install --classic code"]);
    }

    #[test]
    fn channels() {
        let info = "name: hello\nsummary: GNU Hello\nchannels:\n  latest/stable:    2.10 2017-05-11 (42) 20kB -\n  latest/candidate: ^\n  latest/edge:      2.11 2024-01-01 (60) 21kB -\ninstalled: 2.10 (42) 20kB -\n";
        let ch = parse_channels(info);
        let ids: Vec<&str> = ch.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["latest/stable", "latest/edge"]);
        assert_eq!(ch[1].version, "2.11");
        assert_eq!(ch[1].label(), "latest/edge (2.11)");
        // Installed: refresh; not installed (a search result): install;
        // classic snaps get --classic.
        let installed = Package {
            installed: true,
            ..Package::new("hello", "2.10")
        };
        let show = |p: &Package| Snap.install_version(p, "latest/edge").unwrap()[0].display(None);
        assert_eq!(show(&installed), "snap refresh hello --channel=latest/edge");
        assert_eq!(
            show(&Package::new("hello", "2.10")),
            "snap install hello --channel=latest/edge"
        );
        assert_eq!(
            show(&Package::new("code", "1.95").repo(CLASSIC)),
            "snap install --classic code --channel=latest/edge"
        );
        assert!(Snap.install_version(&installed, "--evil").is_err());
    }
}
