//! FreeBSD: pkg(8).

use super::{Backend, Cmd, Package, Section, capture, lines, run};
use anyhow::Result;

pub struct Pkg;

/// Query the local database, falling back to the remote catalogue.
fn query(fmt: &str, name: &str) -> Result<Vec<String>> {
    let local = lines(&capture("pkg", &["query", fmt, name], &[])?);
    if !local.is_empty() {
        return Ok(local);
    }
    Ok(lines(&run("pkg", &["rquery", fmt, name])?))
}

impl Backend for Pkg {
    fn name(&self) -> &'static str {
        "pkg"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        // Exits 70 when nothing matches.
        Ok(parse_search(&capture("pkg", &["search", q.trim()], &[70])?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(super::parse_sized(&run("pkg", &["query", "%n\t%v\t%sb\t%t\t%c"])?))
    }

    fn updates(&self) -> Result<Vec<Package>> {
        Ok(parse_updates(&run("pkg", &["version", "-vRL="])?))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        run("pkg", &["info", &pkg.name]).or_else(|_| run("pkg", &["search", "-f", &pkg.name]))
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("pkg", &["install"]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("pkg", &["delete"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("pkg", &["upgrade"]).names(pkgs)]
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![Cmd::root("pkg", &["autoremove"]), Cmd::root("pkg", &["clean"])])
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        // `-y` belongs to the subcommand; `pkg update` never prompts.
        cmds.into_iter()
            .map(|c| {
                match c.args.first().map(String::as_str) {
                    // `lock`/`unlock` already carry -y; `update` never prompts.
                    Some("update" | "lock" | "unlock") => c,
                    _ => c.flag_at(1, "-y"),
                }
            })
            .collect()
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(lines(&run("pkg", &["query", "-e", "%a = 0", "%n"])?))
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        let out = run("pkg", &["query", "-e", "%a = 1 && %#r = 0", "%n\t%v\t%c"])?;
        Ok(super::dnf::parse_tabbed(&out))
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let name = pkg.name.as_str();
        match s {
            Section::Size => query("%sh", name),
            Section::Depends => query("%dn", name),
            Section::RequiredBy => Ok(lines(&run("pkg", &["query", "%rn", name])?)),
            Section::Files => Ok(lines(&run("pkg", &["query", "%Fp", name])?)),
        }
    }

    fn held(&self) -> Result<Vec<String>> {
        // Exits 1 when no package is locked.
        Ok(parse_locked(&capture("pkg", &["lock", "-l"], &[1])?))
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("pkg", &[if on { "lock" } else { "unlock" }, "-y"]).names(pkgs),
        ])
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root("pkg", &["update"])]
    }
}

/// `pkg search`: "tree-2.1.3                    Display a tree-view of directories"
pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let (namever, desc) = l.split_once(char::is_whitespace).unwrap_or((l, ""));
            let (n, v) = namever.rsplit_once('-')?;
            (!n.is_empty()).then(|| Package::new(n, v).desc(desc.trim()))
        })
        .collect()
}

/// `pkg version -vRL=`: "curl-8.9.1   <   needs updating (remote has 8.10.1)"
pub(crate) fn parse_updates(out: &str) -> Vec<Package> {
    out.lines()
        .filter_map(|l| {
            let mut t = l.split_whitespace();
            let (n, v) = t.next()?.rsplit_once('-')?;
            (t.next()? == "<").then_some(())?;
            let new = l.split_once("remote has ")?.1.trim_end_matches(')').trim();
            Some(Package::new(n, v).upgrade_to(new))
        })
        .collect()
}

/// `pkg lock -l`: "Currently locked packages:" then "tree-2.1.3" lines.
pub(crate) fn parse_locked(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with(':'))
        .filter_map(|l| l.rsplit_once('-').map(|(n, _)| n.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        let s = parse_search(
            "tree-2.1.3                     Display a tree-view of directories\npy311-foo-bar-1.0_1  Foo\n",
        );
        assert_eq!(
            s[0],
            Package::new("tree", "2.1.3").desc("Display a tree-view of directories")
        );
        assert_eq!(s[1].name, "py311-foo-bar");
        let u = parse_updates("curl-8.9.1                         <   needs updating (remote has 8.10.1)\n");
        assert_eq!(u, vec![Package::new("curl", "8.9.1").upgrade_to("8.10.1")]);
    }

    #[test]
    fn locked() {
        assert_eq!(
            parse_locked("Currently locked packages:\ntree-2.1.3\npy311-foo-1.0_1\n"),
            ["tree", "py311-foo"]
        );
    }
}
