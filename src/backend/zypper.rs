//! openSUSE / SLES: zypper, plus rpm for local queries.

use super::{
    Backend, Cmd, Package, Section, VersionChoice, capture, col, field, fields, pipe_table, plain_versions, rpm,
};
use anyhow::Result;
use std::sync::Mutex;

pub struct Zypper;

/// zypper holds a system-wide lock, so concurrent zypper processes fail
/// ("System management is locked"). pakdeck's background queries take turns.
static ZYPPER: Mutex<()> = Mutex::new(());

/// Read-only zypper query: never prompt, never refresh repositories.
fn query(args: &[&str], ok_codes: &[i32]) -> Result<String> {
    let mut a = vec!["--non-interactive", "--quiet", "--no-refresh"];
    a.extend(args);
    let _turn = ZYPPER.lock().unwrap_or_else(|e| e.into_inner());
    capture("zypper", &a, ok_codes)
}

// zypper exits 104 when a search finds nothing.
const NOT_FOUND: i32 = 104;

impl Backend for Zypper {
    fn name(&self) -> &'static str {
        "zypper"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        Ok(parse_search(&query(
            &["search", "-t", "package", q.trim()],
            &[NOT_FOUND],
        )?))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        rpm::installed()
    }

    fn updates(&self) -> Result<Vec<Package>> {
        Ok(parse_updates(&query(&["list-updates"], &[])?))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        query(&["info", &pkg.name], &[])
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("zypper", &["install"]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("zypper", &["remove", "--clean-deps"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root("zypper", &["update"]).names(pkgs)]
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        let orphans = self.orphans()?;
        let mut v = Vec::new();
        if !orphans.is_empty() {
            v.push(Cmd::root("zypper", &["remove", "--clean-deps"]).names(&orphans));
        }
        v.push(Cmd::root("zypper", &["clean", "--all"]));
        Ok(v)
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        // A global option: it must come before the subcommand.
        cmds.into_iter().map(|c| c.flag_at(0, "--non-interactive")).collect()
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(parse_user_installed(&query(
            &["search", "-i", "-t", "package"],
            &[NOT_FOUND],
        )?))
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        Ok(parse_unneeded(&query(&["packages", "--unneeded"], &[])?))
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let name = pkg.name.as_str();
        match s {
            Section::Size => rpm::size(name).or_else(|_| {
                let f = fields(&query(&["info", name], &[])?);
                Ok(field(&f, "Installed Size").map(String::from).into_iter().collect())
            }),
            Section::Depends => rpm::requires(name).or_else(|_| {
                let f = fields(&query(&["info", "--requires", name], &[])?);
                Ok(field(&f, "Requires")
                    .map(|v| v.lines().filter(|l| !l.starts_with('[')).map(String::from).collect())
                    .unwrap_or_default())
            }),
            Section::RequiredBy => rpm::required_by(name),
            Section::Files => rpm::files(name),
        }
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(pipe_table(&query(&["locks"], &[])?)
            .iter()
            .map(|r| col(r, "Name").to_string())
            .filter(|n| !n.is_empty())
            .collect())
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("zypper", &[if on { "addlock" } else { "removelock" }]).names(pkgs),
        ])
    }

    fn versions(&self, pkg: &Package) -> Result<Vec<VersionChoice>> {
        let out = query(
            &["search", "-s", "--match-exact", "-t", "package", &pkg.name],
            &[NOT_FOUND],
        )?;
        Ok(plain_versions(
            pipe_table(&out)
                .iter()
                .map(|r| col(r, "Version").to_string())
                .filter(|v| !v.is_empty())
                .collect(),
        ))
    }

    fn installs_versions(&self) -> bool {
        true
    }

    fn install_version(&self, pkg: &Package, version: &str) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root("zypper", &["install", "--oldpackage"]).strings([format!("{}={version}", pkg.name)]),
        ])
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root("zypper", &["refresh"])]
    }
}

/// `zypper search`: "S | Name | Summary | Type"
pub(crate) fn parse_search(out: &str) -> Vec<Package> {
    pipe_table(out)
        .iter()
        .filter(|r| !col(r, "Name").is_empty())
        .map(|r| Package::new(col(r, "Name"), "").desc(col(r, "Summary")))
        .collect()
}

/// `zypper list-updates`: "S | Repository | Name | Current Version | Available Version | Arch"
pub(crate) fn parse_updates(out: &str) -> Vec<Package> {
    pipe_table(out)
        .iter()
        .filter(|r| !col(r, "Name").is_empty())
        .map(|r| {
            Package::new(col(r, "Name"), col(r, "Current Version"))
                .repo(col(r, "Repository"))
                .upgrade_to(col(r, "Available Version"))
        })
        .collect()
}

/// `zypper search -i`: status "i+" marks packages the user selected.
pub(crate) fn parse_user_installed(out: &str) -> Vec<String> {
    pipe_table(out)
        .iter()
        .filter(|r| col(r, "S") == "i+")
        .map(|r| col(r, "Name").to_string())
        .collect()
}

/// `zypper packages --unneeded`: "S | Repository | Name | Version | Arch"
pub(crate) fn parse_unneeded(out: &str) -> Vec<Package> {
    pipe_table(out)
        .iter()
        .filter(|r| !col(r, "Name").is_empty())
        .map(|r| Package::new(col(r, "Name"), col(r, "Version")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search() {
        let out = "S  | Name      | Summary                    | Type\n---+-----------+----------------------------+--------\ni+ | tree      | File listing as tree       | package\n   | tree-sitt | Parser generator           | package\n";
        let p = parse_search(out);
        assert_eq!(p[0], Package::new("tree", "").desc("File listing as tree"));
        assert_eq!(p.len(), 2);
        assert_eq!(parse_user_installed(out), ["tree"]);
    }

    #[test]
    fn updates_and_unneeded() {
        let out = "S | Repository | Name | Current Version | Available Version | Arch\n--+------------+------+-----------------+-------------------+-------\nv | Main       | curl | 8.9.1-1.1       | 8.10.1-1.1        | aarch64\n";
        assert_eq!(
            parse_updates(out),
            vec![Package::new("curl", "8.9.1-1.1").repo("Main").upgrade_to("8.10.1-1.1")]
        );
        let out = "S | Repository | Name   | Version | Arch\n--+------------+--------+---------+-----\ni | Main       | libfoo | 1.0-1.1 | aarch64\n";
        assert_eq!(parse_unneeded(out), vec![Package::new("libfoo", "1.0-1.1")]);
    }
}
