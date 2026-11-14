//! Debian / Ubuntu: apt-get, apt-cache, dpkg-query.

use super::{
    Backend, Cmd, Package, Section, VersionChoice, capture, field, fields, files_only, human_size, lines, mtime,
    plain_versions, run,
};
use anyhow::Result;

pub struct Apt;

impl Backend for Apt {
    fn name(&self) -> &'static str {
        "apt"
    }

    fn search(&self, query: &str) -> Result<Vec<Package>> {
        let mut args = vec!["search"];
        args.extend(query.split_whitespace());
        Ok(parse_search(&run("apt-cache", &args)?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        let fmt =
            "-f=${db:Status-Abbrev}\t${Package}\t${Version}\t${Installed-Size}\t${Architecture}\t${binary:Summary}\n";
        let mut v = parse_installed(&run("dpkg-query", &["-W", fmt])?);
        // dpkg records no install time; its file list is written on install.
        for p in &mut v {
            let dir = std::path::Path::new("/var/lib/dpkg/info");
            p.installed_at = mtime(&dir.join(format!("{}.list", p.name)))
                .or_else(|| mtime(&dir.join(format!("{}:{}.list", p.name, p.repo))));
            p.repo.clear();
        }
        Ok(v)
    }

    fn updates(&self) -> Result<Vec<Package>> {
        Ok(parse_upgradable(&run("apt", &["list", "--upgradable"])?))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        run("apt-cache", &["show", "--no-all-versions", &pkg.name])
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("apt-get", &["install"]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("apt-get", &["remove"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            vec![Cmd::root("apt-get", &["update"]), Cmd::root("apt-get", &["upgrade"])]
        } else {
            vec![Cmd::root("apt-get", &["install", "--only-upgrade"]).names(pkgs)]
        }
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("apt-get", &["autoremove"]),
            Cmd::root("apt-get", &["clean"]),
        ])
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        // apt-mark never prompts.
        cmds.into_iter()
            .map(|c| if c.program == "apt-get" { c.flag_at(0, "-y") } else { c })
            .collect()
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(lines(&run("apt-mark", &["showmanual"])?))
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        // Simulated autoremove: needs no root and changes nothing.
        Ok(parse_autoremove(&run("apt-get", &["-s", "autoremove"])?))
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let name = pkg.name.as_str();
        Ok(match s {
            Section::Size => {
                let info = fields(&run("apt-cache", &["show", "--no-all-versions", name])?);
                field(&info, "Installed-Size")
                    .and_then(|kib| kib.parse::<u64>().ok())
                    .map(|kib| human_size(kib.saturating_mul(1024)))
                    .into_iter()
                    .collect()
            }
            Section::Depends => parse_depends(&run("apt-cache", &["depends", "--important", name])?),
            Section::RequiredBy => parse_rdepends(&run("apt-cache", &["rdepends", "--installed", name])?),
            Section::Files => files_only(lines(&capture("dpkg-query", &["-L", name], &[])?)),
        })
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(lines(&run("apt-mark", &["showhold"])?))
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("apt-mark", &[if on { "hold" } else { "unhold" }]).names(pkgs),
        ])
    }

    fn owns(&self, cmd: &Cmd) -> bool {
        matches!(cmd.program.as_str(), "apt-get" | "apt-mark")
    }

    fn versions(&self, pkg: &Package) -> Result<Vec<VersionChoice>> {
        Ok(plain_versions(parse_madison(&run(
            "apt-cache",
            &["madison", &pkg.name],
        )?)))
    }

    fn installs_versions(&self) -> bool {
        true
    }

    fn install_version(&self, pkg: &Package, version: &str) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("apt-get", &["install", "--allow-downgrades"]).strings([format!("{}={version}", pkg.name)]),
        ])
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root("apt-get", &["update"])]
    }
}

pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| l.split_once(" - "))
        .map(|(name, desc)| Package::new(name.trim(), "").desc(desc.trim()))
        .collect()
}

pub(crate) fn parse_installed(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.splitn(6, '\t');
            let (status, name, ver) = (f.next()?, f.next()?, f.next()?);
            let kib = f.next().and_then(|s| s.trim().parse::<u64>().ok());
            // The architecture, kept in `repo` only until install times are read.
            let arch = f.next().unwrap_or("").trim();
            // "ii" = installed, "hi" = installed and held.
            (status.get(1..2) == Some("i")).then(|| {
                Package::new(name, ver)
                    .sized(kib.map(|k| k.saturating_mul(1024)))
                    .repo(arch)
                    .desc(f.next().unwrap_or("").trim())
            })
        })
        .collect()
}

/// `apt list --upgradable` lines: "name/repo,repo2 1.2.4 amd64 [upgradable from: 1.2.3]"
pub(crate) fn parse_upgradable(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let mut t = l.split_whitespace();
            let (name, repo) = t.next()?.split_once('/')?;
            let new = t.next()?;
            let old = l.split_once("from: ")?.1.trim_end_matches(']').trim();
            Some(Package::new(name, old).repo(repo).upgrade_to(new))
        })
        .collect()
}

/// `apt-get -s autoremove`: "Remv libfoo1 [1.2-3]"
pub(crate) fn parse_autoremove(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("Remv ")?;
            let mut t = rest.split_whitespace();
            let name = t.next()?;
            let ver = t.next().unwrap_or("").trim_matches(|c| c == '[' || c == ']');
            Some(Package::new(name, ver))
        })
        .collect()
}

/// `apt-cache depends --important`: the "Depends:" and "PreDepends:"
/// targets as package names. Architecture qualifiers ("perl:any") are
/// dropped. A virtual package ("<debconf-2.0>") is replaced by one of the
/// providers listed below it, preferring one already among the
/// dependencies (an alternative "|Depends: debconf"); without providers it
/// is left out, since nothing can be installed or expanded under that name.
pub(crate) fn parse_depends(out: &str) -> Vec<String> {
    let mut deps: Vec<String> = Vec::new();
    // A virtual dependency waiting for its provider lines: (providers seen).
    let mut virtual_providers: Option<Vec<String>> = None;
    let finish = |providers: Option<Vec<String>>, deps: &mut Vec<String>| {
        if let Some(p) = providers {
            let pick = p.iter().find(|x| deps.contains(x)).or_else(|| p.first()).cloned();
            if let Some(pick) = pick
                && !deps.contains(&pick)
            {
                deps.push(pick);
            }
        }
    };
    for line in out.lines() {
        if let Some((_, dep)) = line.split_once("Depends:") {
            finish(virtual_providers.take(), &mut deps);
            let dep = dep.trim();
            if dep.starts_with('<') && dep.ends_with('>') {
                virtual_providers = Some(Vec::new());
                continue;
            }
            let name = dep.split(':').next().unwrap_or(dep).to_string();
            if !name.is_empty() && !deps.contains(&name) {
                deps.push(name);
            }
        } else if let Some(providers) = virtual_providers.as_mut() {
            // Providers are indented deeper than the dependency lines.
            if line.starts_with("    ") {
                let p = line.trim();
                providers.push(p.split(':').next().unwrap_or(p).to_string());
            } else {
                finish(virtual_providers.take(), &mut deps);
            }
        }
    }
    finish(virtual_providers.take(), &mut deps);
    deps
}

/// `apt-cache rdepends`: names listed after "Reverse Depends:".
pub(crate) fn parse_rdepends(out: &str) -> Vec<String> {
    let after = out.split_once("Reverse Depends:").map(|(_, r)| r).unwrap_or("");
    lines(&after.replace('|', " "))
}

/// `apt-cache madison`: "  tree | 2.1.0-1 | http://deb.debian.org/debian trixie/main amd64 Packages"
pub(crate) fn parse_madison(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            l.split('|')
                .nth(1)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search() {
        let p = parse_search(
            "ripgrep - Recursively searches directories for a regex pattern\nfd-find - Simple, fast and user-friendly alternative to find\n",
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].name, "fd-find");
        assert_eq!(p[1].description, "Simple, fast and user-friendly alternative to find");
    }

    #[test]
    fn installed() {
        // status, name, version, installed size (KiB), architecture, summary
        let p = parse_installed(
            "ii \tbash\t5.2.15-2+b2\t7000\tamd64\tGNU Bourne Again SHell\nrc \told\t1.0\t\tamd64\tgone\nhi \tvim\t2:9.0\t\tall\tVi IMproved\n",
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].description, "GNU Bourne Again SHell");
        assert_eq!(p[0].size, Some(7000 * 1024));
        assert_eq!(p[1].name, "vim");
        assert_eq!(p[1].size, None);
    }

    #[test]
    fn upgradable() {
        let p = parse_upgradable(
            "Listing... Done\nlibssl3/stable-security 3.0.15-1~deb12u1 amd64 [upgradable from: 3.0.14-1~deb12u2]\n",
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "libssl3");
        assert_eq!(p[0].repo, "stable-security");
        assert_eq!(p[0].version, "3.0.14-1~deb12u2");
        assert_eq!(p[0].new_version.as_deref(), Some("3.0.15-1~deb12u1"));
    }

    #[test]
    fn orphans_and_deps() {
        let p = parse_autoremove("NOTE: simulation\nRemv libfoo1 [1.2-3]\nRemv bar [2.0] [amd64]\nInst x\n");
        assert_eq!(p, vec![Package::new("libfoo1", "1.2-3"), Package::new("bar", "2.0")]);
        let d = parse_depends("tree\n  Depends: libc6\n |Depends: a\n  Depends: b\n  Depends: libc6\n");
        assert_eq!(d, ["libc6", "a", "b"]);
        // Real output: a virtual package with providers, an alternative that
        // is one of them, an arch qualifier, and a virtual with none.
        let d = parse_depends(
            "libpam-modules\n  PreDepends: libaudit1\n |PreDepends: debconf\n  PreDepends: <debconf-2.0>\n    cdebconf\n    debconf\n  PreDepends: libpam-modules-bin\n  Depends: perl:any\n  Depends: <nothing-provides-me>\n",
        );
        assert_eq!(d, ["libaudit1", "debconf", "libpam-modules-bin", "perl"]);
        let d = parse_depends("x\n  Depends: <mail-transport-agent>\n    exim4-daemon-light\n    postfix\n");
        assert_eq!(d, ["exim4-daemon-light"]);
        let r = parse_rdepends("tree\nReverse Depends:\n  foo\n |bar\n  foo\n");
        assert_eq!(r, ["foo", "bar"]);
    }

    #[test]
    fn madison() {
        let v = parse_madison(
            "      tree |    2.1.0-1 | http://deb.debian.org/debian trixie/main amd64 Packages\n      tree | 1.8.0-1+b1 | http://deb.debian.org/debian bookworm/main amd64 Packages\n",
        );
        assert_eq!(v, ["2.1.0-1", "1.8.0-1+b1"]);
        let c = Apt.install_version(&Package::new("tree", ""), "1.8.0-1+b1").unwrap();
        assert_eq!(c[0].display(None), "apt-get install --allow-downgrades tree=1.8.0-1+b1");
    }
}
