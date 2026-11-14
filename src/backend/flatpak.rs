//! Flatpak applications in the system installation (pakdeck runs flatpak with
//! sudo, which acts on the system installation; per-user installs are not
//! listed). Packages are application IDs; `Package::repo` is the remote
//! (e.g. flathub). Runtimes are dependencies, not listed packages.

use super::{Backend, Cmd, Package, Section, lines, run, walk_files};
use anyhow::Result;

pub struct Flatpak;

/// `--columns=...` output is tab-separated when not writing to a terminal.
fn tsv(out: &str, min_cols: usize) -> Vec<Vec<&str>> {
    out.lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .filter(|c| c.len() >= min_cols)
        .collect()
}

fn col<'a>(row: &[&'a str], i: usize) -> &'a str {
    row.get(i).copied().unwrap_or_default().trim()
}

/// Group packages by remote: `flatpak install <remote> <ids>...`.
fn by_remote(pkgs: &[Package]) -> Vec<(String, Vec<Package>)> {
    let mut groups: Vec<(String, Vec<Package>)> = Vec::new();
    for p in pkgs {
        match groups.iter_mut().find(|(r, _)| *r == p.repo) {
            Some((_, v)) => v.push(p.clone()),
            None => groups.push((p.repo.clone(), vec![p.clone()])),
        }
    }
    groups
}

impl Flatpak {
    fn info_text(&self, pkg: &Package) -> Result<String> {
        run("flatpak", &["info", "--system", &pkg.name]).or_else(|_| {
            let remote = if pkg.repo.is_empty() {
                "flathub"
            } else {
                pkg.repo.as_str()
            };
            run("flatpak", &["remote-info", "--system", remote, &pkg.name])
        })
    }
}

impl Backend for Flatpak {
    fn name(&self) -> &'static str {
        "flatpak"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        let out = run(
            "flatpak",
            &["search", "--columns=application,version,description,remotes", q.trim()],
        )?;
        Ok(parse_search(&out))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        let out = run(
            "flatpak",
            &[
                "list",
                "--system",
                "--app",
                "--columns=application,version,description,origin,size",
            ],
        )?;
        Ok(parse_search(&out))
    }

    fn updates(&self) -> Result<Vec<Package>> {
        let out = run(
            "flatpak",
            &[
                "remote-ls",
                "--system",
                "--updates",
                "--app",
                "--columns=application,version,origin",
            ],
        )?;
        Ok(parse_updates(&out))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        self.info_text(pkg)
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        by_remote(pkgs)
            .into_iter()
            .map(|(remote, v)| {
                let mut c = Cmd::root("flatpak", &["install"]);
                if !remote.is_empty() {
                    c = c.strings([remote]);
                }
                c.names(&v)
            })
            .collect()
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("flatpak", &["uninstall"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("flatpak", &["update"]).names(pkgs)]
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        // Removes runtimes and extensions no installed app uses.
        Ok(vec![Cmd::root("flatpak", &["uninstall", "--unused"])])
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds.into_iter()
            .map(|c| {
                if c.args.first().is_some_and(|a| a == "mask") {
                    c
                } else {
                    c.flag_at(1, "-y")
                }
            })
            .collect()
    }

    /// `remote:id`, so an import installs from the same remote.
    fn export_name(&self, pkg: &Package) -> String {
        if pkg.repo.is_empty() {
            pkg.name.clone()
        } else {
            format!("{}:{}", pkg.repo, pkg.name)
        }
    }

    fn import_package(&self, entry: &str) -> Package {
        match entry.split_once(':') {
            Some((remote, id)) => Package::new(id, "").repo(remote),
            None => Package::new(entry, ""),
        }
    }

    fn explicit(&self) -> Result<Vec<String>> {
        // Only applications are listed; runtimes are their dependencies.
        Ok(self.installed()?.into_iter().map(|p| p.name).collect())
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(parse_masks(&run("flatpak", &["mask", "--system"])?))
    }

    /// `flatpak mask` stops a ref from being updated (or installed).
    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        let args: &[&str] = if on {
            &["mask", "--system"]
        } else {
            &["mask", "--system", "--remove"]
        };
        Ok(pkgs
            .iter()
            .map(|p| Cmd::root("flatpak", args).names(std::slice::from_ref(p)))
            .collect())
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let info = parse_info(&self.info_text(pkg)?);
        let get = |k: &str| info.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        Ok(match s {
            Section::Size => get("Installed").into_iter().collect(),
            Section::Depends => ["Runtime", "Sdk"].iter().filter_map(|k| get(k)).collect(),
            Section::RequiredBy => Vec::new(),
            Section::Files => {
                let dir = run("flatpak", &["info", "--system", "--show-location", &pkg.name])?;
                walk_files(std::path::Path::new(dir.trim()), 5000)
            }
        })
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root("flatpak", &["update", "--appstream"])]
    }
}

/// application, version, description, remote(s)
pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    tsv(out, 2)
        .iter()
        .filter(|r| col(r, 0).contains('.'))
        .map(|r| {
            // `remotes` may list several ("flathub,fedora"); take the first.
            let remote = col(r, 3).split(',').next().unwrap_or_default();
            // `list` adds an installed-size column ("1.3 MB").
            Package::new(col(r, 0), col(r, 1))
                .desc(col(r, 2))
                .repo(remote)
                .sized(super::parse_size(col(r, 4)))
        })
        .collect()
}

/// application, version, origin
pub(crate) fn parse_updates(out: &str) -> Vec<Package> {
    tsv(out, 2)
        .iter()
        .filter(|r| col(r, 0).contains('.'))
        .map(|r| Package::new(col(r, 0), "").repo(col(r, 2)).upgrade_to(col(r, 1)))
        .collect()
}

/// `flatpak info` / `remote-info`: right-aligned "      Key: value" lines.
pub(crate) fn parse_info(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let (k, v) = l.trim_start().split_once(": ")?;
            (!k.contains(' ')).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// `flatpak mask`: a header line, then one pattern per line.
pub(crate) fn parse_masks(out: &str) -> Vec<String> {
    lines(out)
        .into_iter()
        .filter(|l| !l.ends_with(':') && !l.contains(' '))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        let s = parse_search(
            "io.sourceforge.pipewalker.PipeWalker\t1.1\t\"Complete the circuit\" puzzle game\tflathub,other\nNo matches found\n",
        );
        assert_eq!(
            s,
            vec![
                Package::new("io.sourceforge.pipewalker.PipeWalker", "1.1")
                    .desc("\"Complete the circuit\" puzzle game")
                    .repo("flathub")
            ]
        );
        let u = parse_updates("org.mozilla.firefox\t132.0\tflathub\n");
        assert_eq!(
            u,
            vec![
                Package::new("org.mozilla.firefox", "")
                    .repo("flathub")
                    .upgrade_to("132.0")
            ]
        );
        let i = parse_info(
            "\nPipeWalker - puzzle\n\n        ID: io.x.Y\n Installed: 1.3 MB\n   Runtime: org.freedesktop.Platform/aarch64/26.08\n",
        );
        assert!(i.contains(&("Installed".into(), "1.3 MB".into())));
        assert!(i.contains(&("Runtime".into(), "org.freedesktop.Platform/aarch64/26.08".into())));
        assert_eq!(
            parse_masks("Masked patterns:\n  org.mozilla.firefox\n"),
            ["org.mozilla.firefox"]
        );
    }

    #[test]
    fn install_groups_by_remote() {
        let p = [
            Package::new("a.b.C", "").repo("flathub"),
            Package::new("d.e.F", "").repo("flathub"),
            Package::new("g.h.I", ""),
        ];
        let c: Vec<String> = Flatpak.install(&p).iter().map(|c| c.display(None)).collect();
        assert_eq!(c, ["flatpak install flathub a.b.C d.e.F", "flatpak install g.h.I"]);
        let y: Vec<String> = Flatpak
            .assume_yes(Flatpak.install(&p[..1]))
            .iter()
            .map(|c| c.display(None))
            .collect();
        assert_eq!(y, ["flatpak install -y flathub a.b.C"]);
    }

    #[test]
    fn export_keeps_remote() {
        let p = Package::new("org.mozilla.firefox", "1").repo("flathub");
        assert_eq!(Flatpak.export_name(&p), "flathub:org.mozilla.firefox");
        let back = Flatpak.import_package("flathub:org.mozilla.firefox");
        assert_eq!(
            (back.name.as_str(), back.repo.as_str()),
            ("org.mozilla.firefox", "flathub")
        );
        assert_eq!(Flatpak.import_package("org.x.Y").repo, "");
    }
}
