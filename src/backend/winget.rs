//! Windows: winget. Packages are identified by their winget Id; the
//! display name becomes the description.

use super::{Backend, Cmd, Package, VersionChoice, col, fixed_table, run};
use anyhow::Result;

pub struct Winget;

const QUIET: [&str; 2] = ["--accept-source-agreements", "--disable-interactivity"];

fn query(args: &[&str]) -> Result<Vec<Package>> {
    let mut a = args.to_vec();
    a.extend(QUIET);
    Ok(parse(&run("winget", &a)?))
}

/// One command per package: winget takes a single package per call.
fn each(verb: &str, pkgs: &[Package]) -> Vec<Cmd> {
    pkgs.iter()
        .map(|p| Cmd::user("winget", &[verb, "--id", &p.name, "--exact"]))
        .collect()
}

impl Backend for Winget {
    /// Runs as the user: no command ever needs sudo.
    fn needs_root(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "winget"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        query(&["search", q.trim()])
    }

    fn installed(&self) -> Result<Vec<Package>> {
        query(&["list"])
    }

    fn updates(&self) -> Result<Vec<Package>> {
        query(&["upgrade"])
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        run("winget", &["show", "--id", &pkg.name, "--exact", QUIET[0]])
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        each("install", pkgs)
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        each("uninstall", pkgs)
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            vec![Cmd::user("winget", &["upgrade", "--all"])]
        } else {
            each("upgrade", pkgs)
        }
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds.into_iter()
            .map(|c| match c.args.first().map(String::as_str) {
                Some("install" | "upgrade") => c.strings(
                    [
                        "--accept-package-agreements",
                        "--accept-source-agreements",
                        "--disable-interactivity",
                    ]
                    .map(String::from),
                ),
                Some("uninstall") => {
                    c.strings(["--accept-source-agreements", "--disable-interactivity"].map(String::from))
                }
                _ => c,
            })
            .collect()
    }

    fn held(&self) -> Result<Vec<String>> {
        let rows = fixed_table(&run("winget", &["pin", "list", QUIET[0]])?);
        Ok(rows
            .iter()
            .map(|r| col(r, "Id").to_string())
            .filter(|i| !i.is_empty())
            .collect())
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        let verb = if on { "add" } else { "remove" };
        Ok(pkgs
            .iter()
            .map(|p| Cmd::user("winget", &["pin", verb, "--id", &p.name, "--exact"]))
            .collect())
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        // winget keeps no package cache or orphan list to clean.
        Ok(Vec::new())
    }

    fn versions(&self, pkg: &Package) -> Result<Vec<VersionChoice>> {
        let out = run(
            "winget",
            &["show", "--id", &pkg.name, "--exact", "--versions", QUIET[0]],
        )?;
        Ok(super::plain_versions(parse_versions(&out)))
    }

    fn installs_versions(&self) -> bool {
        true
    }

    fn install_version(&self, pkg: &Package, version: &str) -> Result<Vec<Cmd>> {
        super::validate_version(version)?;
        Ok(vec![Cmd::user(
            "winget",
            &["install", "--id", &pkg.name, "--exact", "--version", version],
        )])
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::user("winget", &["source", "update"])]
    }
}

/// `winget search/list/upgrade` tables: Name, Id, Version, [Available], Source.
pub(crate) fn parse(out: &str) -> Vec<Package> {
    fixed_table(out)
        .iter()
        // Summary lines ("2 upgrades available.") spill into the columns;
        // real rows always have an Id without spaces and a version.
        .filter(|r| {
            let id = col(r, "Id");
            !id.is_empty() && !id.contains(' ') && !col(r, "Version").is_empty()
        })
        .map(|r| {
            let p = Package::new(col(r, "Id"), col(r, "Version"))
                .desc(col(r, "Name"))
                .repo(col(r, "Source"));
            match col(r, "Available") {
                "" => p,
                new => p.upgrade_to(new),
            }
        })
        .collect()
}

/// `winget show --versions`: a "Version" header, a dashed rule, then one
/// version per line.
pub(crate) fn parse_versions(out: &str) -> Vec<String> {
    let mut lines = out.lines().map(|l| l.rsplit('\r').next().unwrap_or(l).trim());
    if lines
        .by_ref()
        .find(|l| l.chars().all(|c| c == '-') && l.len() > 2)
        .is_none()
    {
        return Vec::new();
    }
    lines.take_while(|l| !l.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_table() {
        let out = "Name               Id                Version  Available Source\n---------------------------------------------------------------\nGit                Git.Git           2.46.0   2.47.0    winget\nMicrosoft Edge     Microsoft.Edge    130.0    131.0     winget\n2 upgrades available.\n";
        let p = parse(out);
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0],
            Package::new("Git.Git", "2.46.0")
                .desc("Git")
                .repo("winget")
                .upgrade_to("2.47.0")
        );
    }

    #[test]
    fn versions() {
        assert_eq!(
            parse_versions("Found Git [Git.Git]\nVersion\n-------\n2.47.0\n2.46.2\n"),
            ["2.47.0", "2.46.2"]
        );
        assert!(parse_versions("No package found\n").is_empty());
    }
}
