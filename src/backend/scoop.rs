//! Windows: scoop. `scoop` is a batch shim, started through `cmd /C`
//! (see `resolve_argv`).

use super::{Backend, Cmd, Package, Section, col, fixed_table, run, unsupported};
use anyhow::Result;

pub struct Scoop;

impl Backend for Scoop {
    /// Runs as the user: no command ever needs sudo.
    fn needs_root(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "scoop"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        Ok(parse_search(&run("scoop", &["search", q.trim()])?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(parse_list(&run("scoop", &["list"])?))
    }

    fn updates(&self) -> Result<Vec<Package>> {
        Ok(parse_status(&run("scoop", &["status"])?))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        run("scoop", &["info", &pkg.name])
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::user("scoop", &["install"]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::user("scoop", &["uninstall"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            vec![Cmd::user("scoop", &["update"]), Cmd::user("scoop", &["update", "*"])]
        } else {
            vec![Cmd::user("scoop", &["update"]).names(pkgs)]
        }
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::user("scoop", &["cleanup", "*"]),
            Cmd::user("scoop", &["cache", "rm", "*"]),
        ])
    }

    fn explicit(&self) -> Result<Vec<String>> {
        // scoop installs dependencies as ordinary apps; all are explicit.
        Ok(self.installed()?.into_iter().map(|p| p.name).collect())
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        match s {
            Section::Depends => {
                let rows = fixed_table(&run("scoop", &["depends", &pkg.name])?);
                Ok(rows
                    .iter()
                    .map(|r| col(r, "Name").to_string())
                    .filter(|n| n != &pkg.name)
                    .collect())
            }
            _ => unsupported(format!("scoop: '{}' is not available", s.title())),
        }
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(fixed_table(&run("scoop", &["list"])?)
            .iter()
            .filter(|r| col(r, "Info").contains("Held"))
            .map(|r| col(r, "Name").to_string())
            .collect())
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::user("scoop", &[if on { "hold" } else { "unhold" }]).names(pkgs),
        ])
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::user("scoop", &["update"])]
    }
}

/// `scoop search`: Name, Version, Source, Binaries
pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    fixed_table(out)
        .iter()
        .filter(|r| !col(r, "Name").is_empty())
        .map(|r| Package::new(col(r, "Name"), col(r, "Version")).repo(col(r, "Source")))
        .collect()
}

/// `scoop list`: Name, Version, Source, Updated, Info
pub(crate) fn parse_list(out: &str) -> Vec<Package> {
    parse_search(out)
}

/// `scoop status`: Name, Installed Version, Latest Version, Missing Dependencies, Info
pub(crate) fn parse_status(out: &str) -> Vec<Package> {
    fixed_table(out)
        .iter()
        .filter(|r| !col(r, "Name").is_empty() && !col(r, "Latest Version").is_empty())
        .map(|r| Package::new(col(r, "Name"), col(r, "Installed Version")).upgrade_to(col(r, "Latest Version")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables() {
        let list = "Installed apps:\n\nName  Version Source Updated             Info\n----  ------- ------ -------             ----\n7zip  24.08   main   2024-10-01 10:00:00\ngit   2.46.0  main   2024-09-01 10:00:00\n";
        assert_eq!(parse_list(list)[1], Package::new("git", "2.46.0").repo("main"));
        let status = "Name Installed Version Latest Version Missing Dependencies Info\n---- ----------------- -------------- -------------------- ----\ngit  2.46.0            2.47.0\n";
        assert_eq!(
            parse_status(status),
            vec![Package::new("git", "2.46.0").upgrade_to("2.47.0")]
        );
    }
}
