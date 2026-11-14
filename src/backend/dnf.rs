//! Fedora / RHEL / CentOS: dnf (4 and 5) or yum, plus rpm for queries.

use super::{
    Backend, Cmd, Package, Section, VersionChoice, capture, lines, plain_versions, rpm, run, unsupported, vercmp,
};
use anyhow::Result;
use std::collections::HashSet;

pub struct Dnf {
    bin: &'static str,
}

impl Dnf {
    /// `versionlock list`. dnf5 has versionlock built in; dnf4 needs the
    /// python3-dnf-plugin-versionlock package, without which dnf answers
    /// "No such command" (reported as unsupported, not as an error).
    fn versionlock_list(&self) -> Result<String> {
        match run(self.bin, &["versionlock", "list"]) {
            Err(e) if is_missing_command(&format!("{e:#}")) => unsupported(format!(
                "{}: holds need the versionlock plugin (install python3-dnf-plugin-versionlock)",
                self.bin
            )),
            other => other,
        }
    }

    pub fn new(name: &str) -> Self {
        Self {
            bin: if name == "yum" { "yum" } else { "dnf" },
        }
    }
}

impl Dnf {
    /// Package names from a repoquery, deduplicated.
    fn names(&self, args: &[&str]) -> Result<Vec<String>> {
        let mut a = vec!["repoquery", "-q", "--qf", "%{name}\n"];
        a.extend(args);
        Ok(lines(&run(self.bin, &a)?))
    }
}

impl Backend for Dnf {
    fn name(&self) -> &'static str {
        self.bin
    }

    fn search(&self, query: &str) -> Result<Vec<Package>> {
        let pattern = format!("*{}*", query.trim());
        let out = run(
            self.bin,
            &[
                "repoquery",
                "-q",
                "--latest-limit=1",
                "--qf",
                "%{name}\t%{version}-%{release}\t%{summary}\n",
                &pattern,
            ],
        )?;
        Ok(parse_tabbed(&out))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        rpm::installed()
    }

    fn updates(&self) -> Result<Vec<Package>> {
        // check-update exits with 100 when updates are available.
        let out = capture(self.bin, &["check-update", "-q"], &[100])?;
        Ok(parse_check_update(&out))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        run(self.bin, &["info", "-q", &pkg.name])
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root(self.bin, &["install"]).names(pkgs)]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root(self.bin, &["remove"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::root(self.bin, &["upgrade"]).names(pkgs)]
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![
            Cmd::root(self.bin, &["autoremove"]),
            Cmd::root(self.bin, &["clean", "all"]),
        ])
    }

    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds.into_iter().map(|c| c.flag_at(0, "-y")).collect()
    }

    fn explicit(&self) -> Result<Vec<String>> {
        self.names(&["--userinstalled"])
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        let qf = "%{name}\t%{version}-%{release}\t%{summary}\n";
        Ok(parse_tabbed(&run(
            self.bin,
            &["repoquery", "-q", "--unneeded", "--qf", qf],
        )?))
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let name = pkg.name.as_str();
        match s {
            Section::Size => rpm::size(name),
            // dnf4 resolves requirements to packages with --resolve; dnf5
            // replaced it with --providers-of=requires.
            Section::Depends => self
                .names(&["--requires", "--resolve", name])
                .or_else(|_| self.names(&["--providers-of=requires", name])),
            Section::RequiredBy => self.names(&["--installed", "--whatrequires", name]),
            Section::Files => {
                rpm::files(name).or_else(|_| Ok(lines(&run(self.bin, &["repoquery", "-q", "-l", name])?)))
            }
        }
    }

    fn held(&self) -> Result<Vec<String>> {
        Ok(parse_versionlock(&self.versionlock_list()?))
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        // Fails early (as unsupported) when the plugin is missing.
        self.versionlock_list()?;
        let verb = if on { "add" } else { "delete" };
        Ok(vec![Cmd::root(self.bin, &["versionlock", verb]).names(pkgs)])
    }

    fn versions(&self, pkg: &Package) -> Result<Vec<VersionChoice>> {
        let qf = "%{version}-%{release}\n";
        // dnf4 needs --showduplicates; dnf5 lists every version by default.
        let out = run(
            self.bin,
            &["repoquery", "-q", "--showduplicates", "--qf", qf, &pkg.name],
        )
        .or_else(|_| run(self.bin, &["repoquery", "-q", "--qf", qf, &pkg.name]))?;
        Ok(plain_versions(lines(&out)))
    }

    /// `downgrade` when going below the installed version, else `install`.
    fn installs_versions(&self) -> bool {
        true
    }

    fn install_version(&self, pkg: &Package, version: &str) -> Result<Vec<Cmd>> {
        // Only an installed package can be downgraded; `pkg.version` of a
        // search result is the repository's version.
        let older = pkg.installed && vercmp(version, &pkg.version) == std::cmp::Ordering::Less;
        let verb = if older { "downgrade" } else { "install" };
        Ok(vec![
            Cmd::root(self.bin, &[verb]).strings([format!("{}-{version}", pkg.name)]),
        ])
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![Cmd::root(self.bin, &["makecache"])]
    }
}

/// "name\tversion\tsummary" lines; deduplicated by name (multiple arches).
pub(crate) fn parse_tabbed(out: &str) -> Vec<Package> {
    let mut seen = HashSet::new();
    out.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (name, ver) = (f.next()?.trim(), f.next()?.trim());
            if name.is_empty() || !seen.insert(name.to_string()) {
                return None;
            }
            Some(Package::new(name, ver).desc(f.next().unwrap_or("").trim()))
        })
        .collect()
}

/// "name.arch   version   repo". dnf4 wraps long names onto their own line
/// when not writing to a terminal, so a lone name is carried to the next line.
pub(crate) fn parse_check_update(out: &str) -> Vec<Package> {
    let mut v = Vec::new();
    let mut pending: Option<String> = None;
    for line in out.lines() {
        if line.starts_with("Obsoleting") || line.starts_with("Security:") {
            break;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        let (name_arch, version, repo) = match (t.as_slice(), pending.take()) {
            ([only], _) if only.contains('.') => {
                pending = Some(only.to_string());
                continue;
            }
            ([n, ver, repo], _) => (n.to_string(), *ver, *repo),
            ([ver, repo], Some(n)) => (n, *ver, *repo),
            _ => continue,
        };
        // Skip messages ("Last metadata expiration check: ...") but keep
        // versions with an epoch ("1:2.0-1").
        if line.contains(':') && !version.contains(':') {
            continue;
        }
        let name = name_arch.rsplit_once('.').map(|(n, _)| n).unwrap_or(&name_arch);
        v.push(Package::new(name, "").repo(repo).upgrade_to(version));
    }
    v
}

/// `dnf versionlock list`: dnf5 prints "Package name: tree", dnf4 prints
/// NEVRA patterns ("tree-0:2.1.1-1.fc40.*").
pub(crate) fn parse_versionlock(out: &str) -> Vec<String> {
    let names: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter_map(|l| {
            if let Some(n) = l.strip_prefix("Package name:") {
                return Some(n.trim().to_string());
            }
            let (name_epoch, _) = l.split_once(':')?;
            let (name, epoch) = name_epoch.rsplit_once('-')?;
            (epoch.chars().all(|c| c.is_ascii_digit()) && !epoch.is_empty() && !name.contains(' '))
                .then(|| name.to_string())
        })
        .collect();
    lines(&names.join("\n"))
}

/// dnf's answer to an unknown subcommand (plugin not installed).
pub(crate) fn is_missing_command(err: &str) -> bool {
    err.contains("No such command") || err.contains("Unknown argument \"versionlock\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabbed() {
        let p = parse_tabbed("bash\t5.2.26-3.fc40\tThe GNU Bourne Again shell\nbash\t5.2.26-3.fc40\tdup\n\n");
        assert_eq!(
            p,
            vec![Package::new("bash", "5.2.26-3.fc40").desc("The GNU Bourne Again shell")]
        );
    }

    #[test]
    fn check_update() {
        let out = "\nkernel.x86_64    6.11.4-301.fc41   updates\nvery-long-package-name-here.noarch\n                 1:2.0-1.fc41      updates\n\nObsoleting Packages\nfoo.x86_64 1.0 updates\n";
        let p = parse_check_update(out);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "kernel");
        assert_eq!(p[0].new_version.as_deref(), Some("6.11.4-301.fc41"));
        assert_eq!(p[1].name, "very-long-package-name-here");
        assert_eq!(p[1].new_version.as_deref(), Some("1:2.0-1.fc41"));
    }

    #[test]
    fn versionlock() {
        assert_eq!(
            parse_versionlock(
                "# Added by 'versionlock add' command on 2026-11-06\nPackage name: tree\nevr = 2.1.1-1.fc45\n"
            ),
            ["tree"]
        );
        assert_eq!(
            parse_versionlock(
                "Last metadata expiration check: 0:01:02 ago\ntree-0:2.1.1-1.fc40.*\nkernel-core-0:6.11.4-301.fc41.*\n"
            ),
            ["tree", "kernel-core"]
        );
    }

    #[test]
    fn missing_versionlock_plugin() {
        assert!(is_missing_command(
            "dnf versionlock exited with code 1: No such command: versionlock. Please use /usr/bin/dnf --help"
        ));
        assert!(!is_missing_command(
            "dnf versionlock exited with code 1: Failed to download metadata"
        ));
    }

    #[test]
    fn version_commands() {
        let d = Dnf::new("dnf");
        let installed = Package {
            installed: true,
            ..Package::new("tree", "2.1.1-1.fc45")
        };
        assert_eq!(
            d.install_version(&installed, "2.0.0-3.fc45").unwrap()[0].display(None),
            "dnf downgrade tree-2.0.0-3.fc45"
        );
        assert_eq!(
            d.install_version(&installed, "2.2.0-1.fc45").unwrap()[0].display(None),
            "dnf install tree-2.2.0-1.fc45"
        );
        // Not installed (a search result): never "downgrade".
        let candidate = Package::new("tree", "2.1.1-1.fc45");
        assert_eq!(
            d.install_version(&candidate, "2.0.0-3.fc45").unwrap()[0].display(None),
            "dnf install tree-2.0.0-3.fc45"
        );
    }
}
