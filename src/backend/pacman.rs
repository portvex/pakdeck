//! Arch Linux and derivatives: pacman (and checkupdates from pacman-contrib
//! when available, which checks for updates without touching the sync db).

use super::{
    Backend, Cmd, Package, Section, VersionChoice, capture, field, fields, has, lines, mtime, plain_versions,
    unsupported,
};
use anyhow::Result;

pub struct Pacman;

impl Backend for Pacman {
    fn rollback_note(&self) -> Option<&'static str> {
        Some(
            "going back for only these packages is a partial downgrade, which Arch does not support; \
             check that nothing else needs the newer versions",
        )
    }

    fn name(&self) -> &'static str {
        "pacman"
    }

    fn search(&self, query: &str) -> Result<Vec<Package>> {
        let mut args = vec!["-Ss"];
        args.extend(query.split_whitespace());
        // pacman exits 1 when nothing matches.
        Ok(parse_search(&capture("pacman", &args, &[1])?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        let out = capture("pacman", &["-Q"], &[])?;
        let local = std::path::Path::new("/var/lib/pacman/local");
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(n, v)| {
                // The local database entry: its desc file has %SIZE%, and its
                // directory is written at install time.
                let dir = local.join(format!("{n}-{}", v.trim()));
                let size = std::fs::read_to_string(dir.join("desc"))
                    .ok()
                    .and_then(|d| desc_size(&d));
                Package::new(n, v.trim()).sized(size).installed_at(mtime(&dir))
            })
            .collect())
    }

    fn updates(&self) -> Result<Vec<Package>> {
        let out = if has("checkupdates") {
            capture("checkupdates", &[], &[2])? // 2 = no updates
        } else {
            capture("pacman", &["-Qu"], &[1])?
        };
        Ok(parse_updates(&out))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        capture("pacman", &["-Si", &pkg.name], &[]).or_else(|_| capture("pacman", &["-Qi", &pkg.name], &[]))
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("pacman", &["-S", "--needed"]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("pacman", &["-Rns"]).names(pkgs)]
    }

    fn upgrade(&self, _pkgs: &[Package]) -> Vec<Cmd> {
        // Partial upgrades are unsupported on Arch; always do a full upgrade.
        vec![Cmd::root("pacman", &["-Syu"])]
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        let orphans = capture("pacman", &["-Qtdq"], &[1])?;
        let orphans: Vec<String> = orphans.split_whitespace().map(String::from).collect();
        let mut v = Vec::new();
        if !orphans.is_empty() {
            v.push(Cmd::root("pacman", &["-Rns"]).strings(orphans));
        }
        v.push(Cmd::root("pacman", &["-Sc"]));
        Ok(v)
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds.into_iter()
            .map(|c| c.strings(["--noconfirm".to_string()]))
            .collect()
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(lines(&capture("pacman", &["-Qqe"], &[1])?))
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        Ok(parse_qd(&capture("pacman", &["-Qtd"], &[1])?))
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let name = pkg.name.as_str();
        if s == Section::Files {
            return Ok(lines(&capture("pacman", &["-Qlq", name], &[])?));
        }
        // Installed packages know their reverse dependencies; fall back to
        // the sync database for the rest.
        let info = capture("pacman", &["-Qi", name], &[]).or_else(|_| capture("pacman", &["-Si", name], &[]))?;
        let f = fields(&info);
        let list = |key: &str| -> Vec<String> {
            field(&f, key)
                .filter(|v| *v != "None")
                .map(|v| v.split_whitespace().map(String::from).collect())
                .unwrap_or_default()
        };
        Ok(match s {
            Section::Size => field(&f, "Installed Size").map(String::from).into_iter().collect(),
            Section::Depends => resolve_deps(&list("Depends On")),
            _ => list("Required By"),
        })
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(parse_ignore(&std::fs::read_to_string("/etc/pacman.conf")?))
    }

    fn hold(&self, _pkgs: &[Package], _on: bool) -> Result<Vec<Cmd>> {
        unsupported("pacman: add the package to IgnorePkg in /etc/pacman.conf to hold it")
    }

    /// Versions in the package cache (pacman keeps old downloads), plus the
    /// one in the sync database.
    fn versions(&self, pkg: &Package) -> Result<Vec<VersionChoice>> {
        let mut v: Vec<String> = cache_files(&pkg.name).into_iter().map(|(ver, _)| ver).collect();
        if let Ok(info) = capture("pacman", &["-Si", &pkg.name], &[]) {
            v.extend(field(&fields(&info), "Version").map(String::from));
        }
        Ok(plain_versions(v))
    }

    fn installs_versions(&self) -> bool {
        true
    }

    fn install_version(&self, pkg: &Package, version: &str) -> Result<Vec<Cmd>> {
        if let Some((_, path)) = cache_files(&pkg.name).into_iter().find(|(v, _)| v == version) {
            return Ok(vec![Cmd::root("pacman", &["-U"]).strings([path])]);
        }
        let repo = capture("pacman", &["-Si", &pkg.name], &[]).ok();
        if repo.as_deref().map(fields).as_deref().and_then(|f| field(f, "Version")) == Some(version) {
            return Ok(vec![Cmd::root("pacman", &["-S"]).names(std::slice::from_ref(pkg))]);
        }
        anyhow::bail!(
            "pacman: {} {version} is neither in the package cache nor in the repositories",
            pkg.name
        )
    }

    fn sync(&self) -> Vec<Cmd> {
        // A bare -Sy leads to partial upgrades, so sync means a full upgrade.
        vec![Cmd::root("pacman", &["-Syu"])]
    }
}

/// "repo/name version (groups) [installed]" followed by an indented description.
pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    let mut v: Vec<Package> = Vec::new();
    for line in out.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(last) = v.last_mut()
                && last.description.is_empty()
            {
                last.description = line.trim().to_string();
            }
            continue;
        }
        let mut t = line.split_whitespace();
        let (Some(full), Some(ver)) = (t.next(), t.next()) else {
            continue;
        };
        let (repo, name) = full.split_once('/').unwrap_or(("", full));
        v.push(Package::new(name, ver).repo(repo));
    }
    v
}

/// "name old -> new"
pub(crate) fn parse_updates(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            match t.as_slice() {
                [name, old, "->", new, ..] => Some(Package::new(*name, *old).upgrade_to(*new)),
                _ => None,
            }
        })
        .collect()
}

/// `pacman -Qtd`: "name version"
pub(crate) fn parse_qd(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, v)| Package::new(n, v.trim()))
        .collect()
}

/// `IgnorePkg = a b` lines of pacman.conf (active, not commented out).
pub(crate) fn parse_ignore(conf: &str) -> Vec<String> {
    conf.lines()
        .filter_map(|l| {
            let (k, v) = l.trim().split_once('=')?;
            (k.trim() == "IgnorePkg").then(|| v.split_whitespace().map(String::from).collect::<Vec<_>>())
        })
        .flatten()
        .collect()
}

const CACHE: &str = "/var/cache/pacman/pkg";

/// (version, path) of cached package files for `name`.
fn cache_files(name: &str) -> Vec<(String, String)> {
    let Ok(dir) = std::fs::read_dir(CACHE) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let (n, v) = parse_cache_file(&file)?;
            (n == name).then(|| (v, e.path().display().to_string()))
        })
        .collect()
}

/// "tree-2.1.1-1-aarch64.pkg.tar.zst" -> ("tree", "2.1.1-1"). Names may
/// contain dashes, so split from the right: arch, pkgrel, pkgver.
pub(crate) fn parse_cache_file(file: &str) -> Option<(String, String)> {
    // A package file exactly: no signatures (.sig) or partial downloads (.part).
    let (stem, ext) = file.split_once(".pkg.tar")?;
    if !matches!(
        ext,
        "" | ".zst" | ".xz" | ".gz" | ".bz2" | ".lz4" | ".lzo" | ".lrz" | ".Z"
    ) {
        return None;
    }
    let (rest, _arch) = stem.rsplit_once('-')?;
    let (rest, rel) = rest.rsplit_once('-')?;
    let (name, ver) = rest.rsplit_once('-')?;
    (!name.is_empty()).then(|| (name.to_string(), format!("{ver}-{rel}")))
}

/// "%SIZE%\n12345\n" in a local database desc file.
fn desc_size(desc: &str) -> Option<u64> {
    let mut lines = desc.lines();
    lines.find(|l| *l == "%SIZE%")?;
    lines.next()?.trim().parse().ok()
}

/// A dependency without its version constraint: "glibc>=2.38" -> "glibc",
/// "libreadline.so=8-64" -> "libreadline.so".
pub(crate) fn dep_name(dep: &str) -> &str {
    dep.split(['<', '>', '=']).next().unwrap_or(dep).trim()
}

/// Dependencies as package names: constraints dropped, and provides
/// ("sh", "libreadline.so") resolved to the package that provides them, as
/// pacman itself does for -Q and -S targets. Unresolvable names stay as
/// they are; duplicates (readline and libreadline.so) are listed once.
pub(crate) fn resolve_deps(deps: &[String]) -> Vec<String> {
    let names: Vec<&str> = deps.iter().map(|d| dep_name(d)).filter(|d| !d.is_empty()).collect();
    if names.is_empty() {
        return Vec::new();
    }
    // One call when everything is installed: -Qq answers in input order.
    let mut args = vec!["-Qq", "--"];
    args.extend(names.iter().copied());
    let batch = capture("pacman", &args, &[1]).map(|o| lines(&o)).unwrap_or_default();
    let resolved: Vec<String> = if batch.len() == names.len() {
        batch
    } else {
        names.iter().map(|n| resolve_one(n)).collect()
    };
    let mut out: Vec<String> = Vec::new();
    for r in resolved {
        if !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// The installed package providing `name`, else the one the sync database
/// would install for it, else `name` itself.
fn resolve_one(name: &str) -> String {
    let first = |out: Result<String>| out.ok().and_then(|o| lines(&o).into_iter().next());
    first(capture("pacman", &["-Qq", "--", name], &[]))
        .or_else(|| first(capture("pacman", &["-Sddp", "--print-format", "%n", "--", name], &[])))
        .unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undoing_an_upgrade_warns_about_partial_downgrades() {
        assert!(Pacman.rollback_note().is_some_and(|n| n.contains("partial downgrade")));
    }

    #[test]
    fn dependency_names() {
        // `pacman -Qi bash` on Arch.
        let info = "Name            : bash\nVersion         : 5.3.3-2\n\
                    Depends On      : readline  libreadline.so=8-64  glibc  ncurses\n\
                    Optional Deps   : bash-completion: for tab completion\n";
        let f = fields(info);
        let deps: Vec<&str> = field(&f, "Depends On")
            .unwrap_or_default()
            .split_whitespace()
            .map(dep_name)
            .collect();
        assert_eq!(deps, ["readline", "libreadline.so", "glibc", "ncurses"]);
        assert_eq!(dep_name("glibc>=2.38"), "glibc");
        assert_eq!(dep_name("python<3.14"), "python");
        assert_eq!(dep_name("sh"), "sh");
        assert_eq!(dep_name("=1"), "");
        assert!(resolve_deps(&["=1".to_string()]).is_empty());
    }

    #[test]
    fn search() {
        let out = "extra/ripgrep 14.1.1-1 [installed]\n    A search tool\ncore/linux 6.11.5.arch1-1 (base)\n    The Linux kernel and modules\n";
        let p = parse_search(out);
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0],
            Package::new("ripgrep", "14.1.1-1").repo("extra").desc("A search tool")
        );
        assert_eq!(p[1].repo, "core");
    }

    #[test]
    fn updates() {
        let p = parse_updates("linux 6.11.4.arch1-1 -> 6.11.5.arch1-1\nfoo 1 -> 2 [ignored]\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].new_version.as_deref(), Some("6.11.5.arch1-1"));
    }

    #[test]
    fn orphans() {
        assert_eq!(
            parse_qd("libfoo 1.0-1\nbar 2-3\n"),
            vec![Package::new("libfoo", "1.0-1"), Package::new("bar", "2-3")]
        );
    }

    #[test]
    fn ignore_pkg() {
        assert_eq!(
            parse_ignore("[options]\n#IgnorePkg = old\nIgnorePkg = linux linux-headers\nIgnorePkg=foo\n"),
            ["linux", "linux-headers", "foo"]
        );
    }

    #[test]
    fn cache_and_desc() {
        assert_eq!(
            parse_cache_file("tree-2.1.1-1-aarch64.pkg.tar.zst"),
            Some(("tree".into(), "2.1.1-1".into()))
        );
        assert_eq!(
            parse_cache_file("lib32-glibc-2.40+r16-1-x86_64.pkg.tar.zst").map(|p| p.0),
            Some("lib32-glibc".into())
        );
        assert_eq!(parse_cache_file("tree-2.1.1-1-aarch64.pkg.tar.zst.sig"), None);
        assert_eq!(parse_cache_file("tree-2.1.1-1-aarch64.pkg.tar.zst.part"), None);
        assert_eq!(parse_cache_file("random.txt"), None);
        assert_eq!(desc_size("%NAME%\ntree\n\n%SIZE%\n104857\n\n"), Some(104857));
    }
}
