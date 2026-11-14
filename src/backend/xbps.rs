//! Void Linux: xbps-query, xbps-install, xbps-remove.

use super::{Backend, Cmd, Package, Section, capture, lines, run};
use anyhow::Result;

pub struct Xbps;

/// "tree-2.1.1_1" -> ("tree", "2.1.1_1")
fn split_pkgver(s: &str) -> Option<(&str, &str)> {
    s.rsplit_once('-').filter(|(n, v)| !n.is_empty() && !v.is_empty())
}

impl Backend for Xbps {
    fn name(&self) -> &'static str {
        "xbps"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        Ok(parse_search(&run("xbps-query", &["-Rs", q.trim()])?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(parse_installed(&run("xbps-query", &["-l"])?))
    }

    fn updates(&self) -> Result<Vec<Package>> {
        // Dry run of a system update; exit 6 or 17 mean "nothing to do".
        Ok(parse_updates(&capture("xbps-install", &["-un"], &[6, 17])?))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        run("xbps-query", &[&pkg.name]).or_else(|_| run("xbps-query", &["-R", &pkg.name]))
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("xbps-install", &[]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("xbps-remove", &["-R"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            vec![Cmd::root("xbps-install", &["-Su"])]
        } else {
            vec![Cmd::root("xbps-install", &["-u"]).names(pkgs)]
        }
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        // -O: clean the cache, -o: remove orphans.
        Ok(vec![Cmd::root("xbps-remove", &["-Oo"])])
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        // xbps-pkgdb never prompts.
        cmds.into_iter()
            .map(|c| {
                if c.program == "xbps-pkgdb" {
                    c
                } else {
                    c.flag_at(0, "-y")
                }
            })
            .collect()
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(parse_pkgvers(&run("xbps-query", &["-m"])?)
            .into_iter()
            .map(|p| p.name)
            .collect())
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        Ok(parse_pkgvers(&run("xbps-query", &["-O"])?))
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let name = pkg.name.as_str();
        // Local database first, then the repositories (-R).
        let q = |args: &[&str]| -> Result<String> {
            let mut a = args.to_vec();
            a.push(name);
            run("xbps-query", &a).or_else(|_| {
                a.insert(0, "-R");
                run("xbps-query", &a)
            })
        };
        Ok(match s {
            Section::Size => lines(&q(&["-p", "installed_size"])?),
            Section::Depends => parse_deps(&q(&["-x"])?),
            Section::RequiredBy => parse_deps(&run("xbps-query", &["-X", name])?),
            Section::Files => lines(&q(&["-f"])?),
        })
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(parse_pkgvers(&run("xbps-query", &["-H"])?)
            .into_iter()
            .map(|p| p.name)
            .collect())
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("xbps-pkgdb", &["-m", if on { "hold" } else { "unhold" }]).names(pkgs),
        ])
    }

    fn owns(&self, cmd: &Cmd) -> bool {
        cmd.program.starts_with("xbps-")
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root("xbps-install", &["-S"])]
    }
}

/// `xbps-query -Rs`: "[*] tree-2.1.1_1   Recursive directory listing"
pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let rest = l.trim_start().strip_prefix('[')?.split_once(']')?.1.trim_start();
            let (pkgver, desc) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let (n, v) = split_pkgver(pkgver)?;
            Some(Package::new(n, v).desc(desc.trim()))
        })
        .collect()
}

/// `xbps-query -l`: "ii tree-2.1.1_1   Recursive directory listing"
pub(crate) fn parse_installed(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let mut t = l.splitn(3, char::is_whitespace);
            let _state = t.next()?;
            let (n, v) = split_pkgver(t.next()?)?;
            Some(Package::new(n, v).desc(t.next().unwrap_or("").trim()))
        })
        .collect()
}

/// `xbps-install -un`: "tree-2.2.0_1 update aarch64 https://repo ..."
pub(crate) fn parse_updates(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let mut t = l.split_whitespace();
            let pkgver = t.next()?;
            (t.next()? == "update").then_some(())?;
            let (n, v) = split_pkgver(pkgver)?;
            Some(Package::new(n, "").upgrade_to(v))
        })
        .collect()
}

/// One pkgver per line ("tree-2.1.1_1").
pub(crate) fn parse_pkgvers(out: &str) -> Vec<Package> {
    out.split_whitespace()
        .filter_map(split_pkgver)
        .map(|(n, v)| Package::new(n, v))
        .collect()
}

/// Dependency patterns or pkgvers: "glibc>=2.39_1", "libfoo-1.0_1" -> names.
pub(crate) fn parse_deps(out: &str) -> Vec<String> {
    let names: Vec<String> = out
        .split_whitespace()
        .map(|d| {
            if let Some((name, _)) = d.find(['<', '>', '=']).and_then(|i| d.split_at_checked(i)) {
                name.to_string()
            } else {
                split_pkgver(d)
                    .map(|(n, _)| n.to_string())
                    .unwrap_or_else(|| d.to_string())
            }
        })
        .collect();
    lines(&names.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        let s = parse_search(
            "[*] tree-2.1.1_1   Recursive directory indented listing\n[-] treecc-0.3.10_2 Tree compiler\n",
        );
        assert_eq!(
            s[0],
            Package::new("tree", "2.1.1_1").desc("Recursive directory indented listing")
        );
        assert_eq!(s[1].name, "treecc");
        let i = parse_installed("ii bash-5.2.32_1   GNU Bourne Again Shell\n");
        assert_eq!(i, vec![Package::new("bash", "5.2.32_1").desc("GNU Bourne Again Shell")]);
        let u =
            parse_updates("tree-2.2.0_1 update aarch64 https://repo-default.voidlinux.org 1 2\nfoo-1_1 install x y\n");
        assert_eq!(u, vec![Package::new("tree", "").upgrade_to("2.2.0_1")]);
        assert_eq!(
            parse_deps("glibc>=2.39_1\nlibfoo-1.0_1\nbar\n"),
            ["glibc", "libfoo", "bar"]
        );
        assert_eq!(parse_pkgvers("a-1_1\nb-c-2_3\n")[1], Package::new("b-c", "2_3"));
    }
}
