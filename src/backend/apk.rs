//! Alpine Linux: apk.

use super::{Backend, Cmd, Package, Section, capture, run, split_name_version, unsupported};
use anyhow::Result;

pub struct Apk;

impl Backend for Apk {
    fn name(&self) -> &'static str {
        "apk"
    }

    fn search(&self, query: &str) -> Result<Vec<Package>> {
        Ok(parse_search(&run("apk", &["search", "-v", query.trim()])?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        let out = run("apk", &["info", "-v"])?;
        Ok(out
            .lines()
            .filter(|l| !l.contains(char::is_whitespace))
            .filter_map(split_name_version)
            .map(|(n, v)| Package::new(n, v))
            .collect())
    }

    fn updates(&self) -> Result<Vec<Package>> {
        Ok(parse_updates(&run("apk", &["version", "-l", "<"])?))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        // apk 3 prints the details but exits 1 for packages that are not installed.
        let out = capture("apk", &["info", "-a", &pkg.name], &[1])?;
        if out.trim().is_empty() {
            run("apk", &["info", &pkg.name])
        } else {
            Ok(out)
        }
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("apk", &["add"]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("apk", &["del"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            vec![Cmd::root("apk", &["update"]), Cmd::root("apk", &["upgrade"])]
        } else {
            vec![Cmd::root("apk", &["add", "-u"]).names(pkgs)]
        }
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        // apk removes orphaned dependencies on `del`; only the cache remains.
        Ok(vec![Cmd::root("apk", &["cache", "clean"])])
    }

    fn explicit(&self) -> Result<Vec<String>> {
        let world = std::fs::read_to_string("/etc/apk/world")?;
        Ok(parse_world(&world))
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        unsupported("apk removes unneeded dependencies automatically, so there are no orphans")
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let flag = match s {
            Section::Size => "-s",
            Section::Depends => "-R",
            Section::RequiredBy => "-r",
            Section::Files => "-L",
        };
        // apk 3 exits 1 for packages that are not installed.
        let list = parse_list(&capture("apk", &["info", flag, &pkg.name], &[1])?);
        Ok(if s == Section::Depends {
            resolve_deps(&list)
        } else {
            list
        })
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(parse_world_held(&std::fs::read_to_string("/etc/apk/world")?))
    }

    /// apk pins a package by recording an exact version in /etc/apk/world.
    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        if !on {
            return Ok(vec![Cmd::root("apk", &["add"]).names(pkgs)]);
        }
        // The command line passes bare names; look up installed versions.
        let installed = if pkgs.iter().any(|p| p.version.is_empty()) {
            self.installed()?
        } else {
            Vec::new()
        };
        let pinned = pkgs
            .iter()
            .map(|p| {
                let version = match p.version.as_str() {
                    "" => installed
                        .iter()
                        .find(|i| i.name == p.name)
                        .map(|i| i.version.clone())
                        .unwrap_or_default(),
                    v => v.to_string(),
                };
                if version.is_empty() {
                    anyhow::bail!("apk: cannot hold {}: it is not installed", p.name);
                }
                Ok(format!("{}={version}", p.name))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(vec![Cmd::root("apk", &["add"]).strings(pinned)])
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root("apk", &["update"])]
    }
}

/// "name-1.2-r0 - description"
pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let (full, desc) = l.split_once(" - ").unwrap_or((l, ""));
            let (n, v) = split_name_version(full.trim())?;
            Some(Package::new(n, v).desc(desc.trim()))
        })
        .collect()
}

/// "name-1.0-r0   < 1.1-r0"
pub(crate) fn parse_updates(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            let [pkg, "<", new, ..] = t.as_slice() else {
                return None;
            };
            let (n, v) = split_name_version(pkg)?;
            Some(Package::new(n, v).upgrade_to(*new))
        })
        .collect()
}

/// `apk info -R/-r/-L/-s`: "tree-2.3-r0 depends on:" headers, then items.
pub(crate) fn parse_list(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with(':') && !l.starts_with("WARNING"))
        .map(String::from)
        .collect()
}

/// How an `apk info -R` dependency names what it needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dep<'a> {
    /// A package name (version constraint dropped).
    Package(&'a str),
    /// A file ("/bin/sh"): its owner provides it.
    Path(&'a str),
    /// A provided name ("so:libc.musl-aarch64.so.1", "cmd:sh", "pc:zlib").
    Provided(&'a str),
}

pub(crate) fn classify(dep: &str) -> Dep<'_> {
    let dep = dep.trim();
    let bare = dep.split(['<', '>', '=', '~']).next().unwrap_or(dep);
    if dep.starts_with('/') {
        Dep::Path(dep)
    } else if dep.contains(':') {
        Dep::Provided(bare)
    } else {
        Dep::Package(bare)
    }
}

/// Dependencies as package names: files and provided names resolved to
/// the package that provides them; unresolvable ones kept as they are.
fn resolve_deps(deps: &[String]) -> Vec<String> {
    let first = |out: Result<String>| out.ok().and_then(|o| parse_list(&o).into_iter().next());
    let mut out: Vec<String> = Vec::new();
    for d in deps {
        let name = match classify(d) {
            Dep::Package(n) => Some(n.to_string()),
            Dep::Path(p) => first(capture("apk", &["info", "-q", "--who-owns", p], &[1])),
            Dep::Provided(p) => first(capture("apk", &["search", "-x", "-q", p], &[1])),
        }
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| d.clone());
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// /etc/apk/world: one dependency per word, possibly with a version
/// constraint or repository tag ("foo>=1.2", "bar@edge").
pub(crate) fn parse_world(world: &str) -> Vec<String> {
    world
        .split_whitespace()
        .filter(|w| !w.starts_with('!'))
        .map(|w| w.split(['<', '>', '=', '~', '@']).next().unwrap_or(w).to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

/// World entries pinned to an exact version ("tree=2.3.2-r0").
pub(crate) fn parse_world_held(world: &str) -> Vec<String> {
    world
        .split_whitespace()
        .filter_map(|w| {
            let (name, ver) = w.split_once('=')?;
            (!name.ends_with(['<', '>', '~']) && !ver.is_empty()).then(|| name.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn dependency_kinds() {
        // `apk info -R bash` on Alpine 3.22.
        let list = super::parse_list(
            "bash-5.3.9-r1 depends on:\n/bin/sh\nso:libc.musl-aarch64.so.1\nso:libreadline.so.8\nncurses-terminfo-base>=6\n\n",
        );
        let kinds: Vec<super::Dep> = list.iter().map(|d| super::classify(d)).collect();
        assert_eq!(
            kinds,
            [
                super::Dep::Path("/bin/sh"),
                super::Dep::Provided("so:libc.musl-aarch64.so.1"),
                super::Dep::Provided("so:libreadline.so.8"),
                super::Dep::Package("ncurses-terminfo-base"),
            ]
        );
    }

    use super::*;

    #[test]
    fn search() {
        let p = parse_search(
            "ripgrep-14.1.0-r0 - ripgrep recursively searches directories\nripgrep-doc-14.1.0-r0 - ripgrep (documentation)\n",
        );
        assert_eq!(
            p[0],
            Package::new("ripgrep", "14.1.0-r0").desc("ripgrep recursively searches directories")
        );
        assert_eq!(p[1].name, "ripgrep-doc");
    }

    #[test]
    fn updates() {
        let p = parse_updates(
            "Installed:                                Available:\nbusybox-1.36.1-r15                      < 1.36.1-r16\n",
        );
        assert_eq!(p, vec![Package::new("busybox", "1.36.1-r15").upgrade_to("1.36.1-r16")]);
    }

    #[test]
    fn info_lists_and_world() {
        let l = parse_list("tree-2.3.2-r0 depends on:\nso:libc.musl-aarch64.so.1\n\n");
        assert_eq!(l, ["so:libc.musl-aarch64.so.1"]);
        assert_eq!(
            parse_world("alpine-base\nbash>=5\nfoo@edge !bad\n"),
            ["alpine-base", "bash", "foo"]
        );
    }

    #[test]
    fn world_held() {
        assert_eq!(
            parse_world_held("alpine-base\ntree=2.3.2-r0\nbash>=5\nfoo~1.2\n"),
            ["tree"]
        );
    }
}
