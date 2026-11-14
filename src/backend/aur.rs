//! The Arch User Repository through an AUR helper (paru or yay, whichever
//! is installed). Opt-in: add "aur" to `backends` next to "pacman".
//!
//! Installed AUR packages are pacman's "foreign" packages (`pacman -Qm`);
//! this backend `overrides` pacman so they are listed once, as aur.
//! Helpers refuse to run as root and call sudo themselves, so every command
//! runs as the user.

use super::{Backend, Cmd, Package, Section, capture, has, lines, unsupported};
use anyhow::{Result, bail};

pub struct Aur {
    helper: &'static str,
}

impl Aur {
    /// The installed helper: paru, else yay.
    pub fn detect() -> Result<Self> {
        match ["paru", "yay"].into_iter().find(|h| has(h)) {
            Some(helper) => Ok(Self { helper }),
            None => bail!("aur: no AUR helper found (install paru or yay)"),
        }
    }

    #[cfg(test)]
    fn with(helper: &'static str) -> Self {
        Self { helper }
    }
}

impl Backend for Aur {
    /// Runs as the user: no command ever needs sudo.
    fn needs_root(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "aur"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        let mut args = vec!["-Ss", "--aur", "--"];
        args.extend(q.split_whitespace());
        // Exits 1 when nothing matches, like pacman.
        Ok(super::pacman::parse_search(&capture(self.helper, &args, &[1])?)
            .into_iter()
            .map(|p| p.repo("aur"))
            .collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(super::pacman::parse_qd(&capture("pacman", &["-Qm"], &[1])?))
    }

    fn updates(&self) -> Result<Vec<Package>> {
        // Exits 1 when there is nothing to update.
        Ok(super::pacman::parse_updates(&capture(self.helper, &["-Qua"], &[1])?))
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        capture(self.helper, &["-Si", "--aur", "--", &pkg.name], &[])
            .or_else(|_| capture("pacman", &["-Qi", "--", &pkg.name], &[]))
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::user(self.helper, &["-S", "--needed", "--aur"]).strings(aur_targets(pkgs))]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![Cmd::user(self.helper, &["-Rns"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            vec![Cmd::user(self.helper, &["-Sua"])]
        } else {
            vec![Cmd::user(self.helper, &["-S", "--aur"]).strings(aur_targets(pkgs))]
        }
    }

    /// The helper's own build cache (pacman's backend cleans its cache).
    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![Cmd::user(self.helper, &["-Sc", "--aur"])])
    }

    /// Unattended: also skip the helper's PKGBUILD review and diff prompts.
    fn assume_yes(&self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        let extra: &[&str] = match self.helper {
            "paru" => &["--noconfirm", "--skipreview"],
            _ => &["--noconfirm", "--answerdiff", "None", "--answerclean", "None"],
        };
        cmds.into_iter()
            .map(|c| {
                if c.program == self.helper {
                    c.strings(extra.iter().map(|s| s.to_string()))
                } else {
                    c
                }
            })
            .collect()
    }

    fn owns(&self, cmd: &Cmd) -> bool {
        cmd.program == self.helper
    }

    fn overrides(&self) -> &'static [&'static str] {
        &["pacman"]
    }

    fn explicit(&self) -> Result<Vec<String>> {
        Ok(lines(&capture("pacman", &["-Qqem"], &[1])?))
    }

    fn orphans(&self) -> Result<Vec<Package>> {
        Ok(super::pacman::parse_qd(&capture("pacman", &["-Qtdm"], &[1])?))
    }

    /// Installed AUR packages are in pacman's local database (asked directly:
    /// a package named on the command line carries no installed flag).
    /// Others only have the AUR's metadata, which lists dependencies. A
    /// dependency tree rooted at an AUR package also asks about repository
    /// packages (glibc) here, so pacman's sync database is the fallback.
    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let pacman = super::pacman::Pacman;
        if pkg.installed || capture("pacman", &["-Q", "--", &pkg.name], &[]).is_ok() {
            return pacman.section(pkg, s);
        }
        match s {
            Section::Depends => capture(self.helper, &["-Si", "--aur", "--", &pkg.name], &[])
                .map(|info| super::pacman::resolve_deps(&aur_depends(&info)))
                .or_else(|_| pacman.section(pkg, s)),
            _ => pacman
                .section(pkg, s)
                .or_else(|_| unsupported("aur: available once the package is installed")),
        }
    }

    /// pacman's IgnorePkg also stops the helpers from upgrading; the
    /// foreign packages in it are the held AUR packages (pacman's backend
    /// reports the same names, and Multi keeps them under aur).
    fn held(&self) -> Result<Vec<String>> {
        let ignored = super::pacman::Pacman.held()?;
        let foreign = lines(&capture("pacman", &["-Qqm"], &[1])?);
        Ok(ignored.into_iter().filter(|n| foreign.contains(n)).collect())
    }

    fn hold(&self, pkgs: &[Package], on: bool) -> Result<Vec<Cmd>> {
        super::pacman::Pacman.hold(pkgs, on)
    }

    fn sync(&self) -> Vec<Cmd> {
        // The helper queries the AUR live; pacman's backend syncs the repos.
        Vec::new()
    }
}

/// Run-time dependencies from `paru/yay -Si`, without version constraints.
pub(crate) fn aur_depends(info: &str) -> Vec<String> {
    let f = super::fields(info);
    super::field(&f, "Depends On")
        .filter(|v| *v != "None")
        .map(|v| {
            v.split_whitespace()
                .map(|d| super::pacman::dep_name(d).to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// `aur/NAME`: without the prefix the helpers resolve a name that a repo
/// package provides to that package (`pfetch` became `pfetch-rs`), even
/// with `--aur`.
fn aur_targets(pkgs: &[Package]) -> Vec<String> {
    pkgs.iter().map(|p| format!("aur/{}", p.name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_output_from_helpers() {
        let yay =
            "aur/pfetch 1.11.0-1 (+142 0.53) (Installed)\n    A pretty system information tool written in POSIX sh\n";
        let p = super::super::pacman::parse_search(yay);
        assert_eq!((p[0].name.as_str(), p[0].version.as_str()), ("pfetch", "1.11.0-1"));
        assert_eq!(p[0].description, "A pretty system information tool written in POSIX sh");
        let paru = "aur/pfetch 1.11.0-1 [+142 ~0.53] [Installed]\n    A pretty system information tool\n";
        assert_eq!(super::super::pacman::parse_search(paru)[0].name, "pfetch");
    }

    #[test]
    fn depends_from_helper_info() {
        let yay = "Repository      : aur\nName            : pfetch\nVersion         : 1.11.0-1\n\
                   Depends On      : sh  glibc>=2.38  libfoo=1.2\nMake Deps       : None\n";
        assert_eq!(aur_depends(yay), ["sh", "glibc", "libfoo"]);
        assert!(aur_depends("Name : x\nDepends On : None\n").is_empty());
        assert!(aur_depends("").is_empty());
    }

    #[test]
    fn commands() {
        // The helpers run as the user and call sudo themselves.
        let all = [Aur::with("paru"), Aur::with("yay")];
        for a in &all {
            let p = [Package::new("pfetch", "")];
            let mut cmds = a.install(&p);
            cmds.extend(a.remove(&p));
            cmds.extend(a.upgrade(&p));
            cmds.extend(a.cleanup().unwrap());
            assert!(!a.needs_root() && cmds.iter().all(|c| !c.root));
        }
        let p = [Package::new("pfetch", "")];
        let a = Aur::with("paru");
        assert_eq!(
            a.install(&p)[0].display(Some("sudo")),
            "paru -S --needed --aur aur/pfetch"
        );
        let y: Vec<String> = Aur::with("yay")
            .assume_yes(Aur::with("yay").install(&p))
            .iter()
            .map(|c| c.display(None))
            .collect();
        assert_eq!(
            y,
            ["yay -S --needed --aur aur/pfetch --noconfirm --answerdiff None --answerclean None"]
        );
        assert_eq!(a.upgrade(&p)[0].display(None), "paru -S --aur aur/pfetch");
        assert_eq!(a.upgrade(&[])[0].display(None), "paru -Sua");
        assert!(a.owns(&Cmd::user("paru", &[])) && !a.owns(&Cmd::user("pacman", &[])));
        assert_eq!(a.overrides(), ["pacman"]);
    }
}
