//! Nix: packages from the `nixpkgs` flake in the user's `nix profile`.
//! NixOS system configuration is declarative and out of scope.

use super::{Backend, Cmd, Package, Section, human_size, jget, lines, run, unsupported, walk_files};
use anyhow::{Context, Result};
use serde_json::Value;

pub struct Nix;

/// Enabled per call so pakdeck works without changing the user's nix.conf.
const FEATURES: [&str; 2] = ["--extra-experimental-features", "nix-command flakes"];

fn nix(args: &[&str]) -> Result<String> {
    let mut a: Vec<&str> = FEATURES.to_vec();
    a.extend(args);
    run("nix", &a)
}

fn cmd(args: &[&str]) -> Cmd {
    let mut a: Vec<&str> = FEATURES.to_vec();
    a.extend(args);
    Cmd::user("nix", &a)
}

fn flake_refs(pkgs: &[Package]) -> Vec<String> {
    pkgs.iter().map(|p| format!("nixpkgs#{}", p.name)).collect()
}

/// Installed profile entries with their store paths.
fn profile() -> Result<Vec<(Package, Vec<String>)>> {
    parse_profile(&nix(&["profile", "list", "--json"])?)
}

/// Store paths of every output of an installed profile element (a package
/// such as `tree` has a binary output and a separate `-man` output).
fn store_paths(name: &str) -> Result<Vec<String>> {
    match profile()?.into_iter().find(|(p, _)| p.name == name) {
        Some((_, paths)) if !paths.is_empty() => Ok(paths),
        _ => unsupported("details are only available for installed nix packages"),
    }
}

/// Store paths that are not packages: profile generations and environments.
fn is_profile_path(name: &str) -> bool {
    name == "profile" || name == "user-environment" || name.ends_with("-profile")
}

/// Sum of `narSize` over `nix path-info --json` output (a list in older nix,
/// an object keyed by store path in newer nix).
pub(crate) fn nar_total(json: &str) -> Result<u64> {
    let v: Value = serde_json::from_str(json).context("parsing nix path-info")?;
    let entries: Vec<&Value> = match &v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o.values().collect(),
        _ => Vec::new(),
    };
    Ok(entries
        .iter()
        .filter_map(|e| jget(e, "narSize").as_u64())
        .fold(0u64, u64::saturating_add))
}

impl Backend for Nix {
    /// Runs as the user: no command ever needs sudo.
    fn needs_root(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "nix"
    }

    fn search(&self, q: &str) -> Result<Vec<Package>> {
        // The first search evaluates nixpkgs, which can take a while.
        parse_search(&nix(&["search", "nixpkgs", q.trim(), "--json"])?)
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(profile()?.into_iter().map(|(p, _)| p).collect())
    }

    fn updates(&self) -> Result<Vec<Package>> {
        unsupported("nix cannot list pending updates; use Update all to upgrade the profile")
    }

    fn info(&self, pkg: &Package) -> Result<String> {
        let meta = nix(&["eval", "--json", &format!("nixpkgs#{}.meta", pkg.name)])?;
        parse_meta(&meta)
    }

    fn install(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![cmd(&["profile", "install"]).strings(flake_refs(pkgs))]
    }

    fn remove(&self, pkgs: &[Package]) -> Vec<Cmd> {
        vec![cmd(&["profile", "remove"]).names(pkgs)]
    }

    fn upgrade(&self, pkgs: &[Package]) -> Vec<Cmd> {
        if pkgs.is_empty() {
            vec![cmd(&["profile", "upgrade", "--all"])]
        } else {
            vec![cmd(&["profile", "upgrade"]).names(pkgs)]
        }
    }

    fn cleanup(&self) -> Result<Vec<Cmd>> {
        Ok(vec![cmd(&["store", "gc"])])
    }

    fn explicit(&self) -> Result<Vec<String>> {
        // Everything in a profile was installed on purpose.
        Ok(self.installed()?.into_iter().map(|p| p.name).collect())
    }

    fn section(&self, pkg: &Package, s: Section) -> Result<Vec<String>> {
        let paths = store_paths(&pkg.name)?;
        let own: Vec<&str> = paths.iter().map(String::as_str).collect();
        // Package names of related store paths, excluding the package's own
        // outputs and profile generations.
        let related = |flag: &str| -> Result<Vec<String>> {
            let mut args = vec!["-q", flag];
            args.extend(own.iter().copied());
            let names: Vec<String> = lines(&run("nix-store", &args)?)
                .iter()
                .filter(|p| !own.contains(&p.as_str()))
                .map(|p| store_name_version(p).0)
                .filter(|n| !is_profile_path(n) && *n != pkg.name)
                .collect();
            Ok(lines(&names.join("\n")))
        };
        Ok(match s {
            Section::Size => {
                let mut args = vec!["path-info", "--json"];
                args.extend(own.iter().copied());
                let size = nar_total(&nix(&args)?)?;
                args.insert(1, "--recursive");
                let closure = nar_total(&nix(&args)?)?;
                vec![format!(
                    "{} (with dependencies {})",
                    human_size(size),
                    human_size(closure)
                )]
            }
            Section::Depends => related("--references")?,
            Section::RequiredBy => related("--referrers")?,
            Section::Files => paths
                .iter()
                .flat_map(|p| walk_files(std::path::Path::new(p), 5000))
                .collect(),
        })
    }

    fn sync(&self) -> Vec<Cmd> {
        vec![cmd(&["flake", "metadata", "nixpkgs", "--refresh"])]
    }
}

/// "/nix/store/<hash>-hello-2.12.1" -> ("hello", "2.12.1"). The version
/// starts at the first "-" followed by a digit.
pub(crate) fn store_name_version(path: &str) -> (String, String) {
    let base = path.rsplit('/').next().unwrap_or(path);
    let rest = base.split_once('-').map(|(_, r)| r).unwrap_or(base);
    let split = rest
        .char_indices()
        .zip(rest.chars().skip(1))
        .find(|((_, c), next)| *c == '-' && next.is_ascii_digit())
        .map(|((i, _), _)| i);
    match split.and_then(|i| rest.split_at_checked(i)) {
        Some((name, ver)) => (name.to_string(), ver.get(1..).unwrap_or_default().to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// `nix search --json`: {"legacyPackages.<system>.hello": {"pname", "version", "description"}}
pub(crate) fn parse_search(json: &str) -> Result<Vec<Package>> {
    let v: Value = serde_json::from_str(json).context("parsing nix search JSON")?;
    let mut out: Vec<Package> = v
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, p)| {
            // The installable attribute is what follows "legacyPackages.<system>.".
            let attr = key.splitn(3, '.').nth(2).unwrap_or(key);
            Package::new(attr, jget(p, "version").as_str().unwrap_or_default())
                .desc(jget(p, "description").as_str().unwrap_or_default())
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `nix profile list --json`: elements as an object keyed by name (new) or a list (old).
pub(crate) fn parse_profile(json: &str) -> Result<Vec<(Package, Vec<String>)>> {
    let v: Value = serde_json::from_str(json).context("parsing nix profile JSON")?;
    let entry = |name: String, e: &Value| {
        let paths: Vec<String> = jget(e, "storePaths")
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(String::from))
            .collect();
        let version = paths.first().map(|p| store_name_version(p).1).unwrap_or_default();
        (Package::new(name, version), paths)
    };
    let mut out = Vec::new();
    match &jget(&v, "elements") {
        Value::Object(m) => out.extend(m.iter().map(|(k, e)| entry(k.clone(), e))),
        Value::Array(a) => out.extend(a.iter().map(|e| {
            let attr = jget(e, "attrPath").as_str().unwrap_or_default();
            let name = attr.rsplit('.').next().unwrap_or(attr).to_string();
            entry(name, e)
        })),
        _ => {}
    }
    Ok(out)
}

/// A readable summary of `nix eval nixpkgs#<attr>.meta --json`.
pub(crate) fn parse_meta(json: &str) -> Result<String> {
    let m: Value = serde_json::from_str(json).context("parsing nix meta JSON")?;
    let s = |k: &str| jget(&m, k).as_str().unwrap_or_default().to_string();
    let license = match &jget(&m, "license") {
        Value::Array(a) => a
            .iter()
            .map(|l| {
                jget(l, "spdxId")
                    .as_str()
                    .or(jget(l, "fullName").as_str())
                    .unwrap_or("?")
            })
            .collect::<Vec<_>>()
            .join(", "),
        l => jget(l, "spdxId")
            .as_str()
            .or(jget(l, "fullName").as_str())
            .unwrap_or_default()
            .to_string(),
    };
    let mut out = String::new();
    for (k, v) in [
        ("Description", s("description")),
        ("Homepage", s("homepage")),
        ("License", license),
        ("Main program", s("mainProgram")),
    ] {
        if !v.is_empty() {
            out.push_str(&format!("{k}: {v}\n"));
        }
    }
    let long = s("longDescription");
    if !long.is_empty() {
        out.push('\n');
        out.push_str(long.trim());
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_paths() {
        assert_eq!(
            store_name_version("/nix/store/abc123-hello-2.12.1"),
            ("hello".into(), "2.12.1".into())
        );
        assert_eq!(
            store_name_version("/nix/store/abc-python3.12-foo-1.0"),
            ("python3.12-foo".into(), "1.0".into())
        );
        assert_eq!(
            store_name_version("/nix/store/abc-glibc-locales"),
            ("glibc-locales".into(), String::new())
        );
    }

    #[test]
    fn search_and_profile() {
        let s = parse_search(
            r#"{"legacyPackages.aarch64-darwin.tree":{"pname":"tree","version":"2.1.3","description":"List dirs"}}"#,
        )
        .unwrap();
        assert_eq!(s, vec![Package::new("tree", "2.1.3").desc("List dirs")]);
        let new = parse_profile(r#"{"elements":{"tree":{"attrPath":"legacyPackages.x.tree","storePaths":["/nix/store/h-tree-2.1.3"]}},"version":3}"#).unwrap();
        assert_eq!(new[0].0, Package::new("tree", "2.1.3"));
        let old = parse_profile(r#"{"elements":[{"attrPath":"legacyPackages.x.hello","storePaths":["/nix/store/h-hello-2.12"]}],"version":2}"#).unwrap();
        assert_eq!(old[0].0, Package::new("hello", "2.12"));
    }

    #[test]
    fn path_info_sizes() {
        assert_eq!(
            nar_total(r#"[{"path":"/nix/store/a","narSize":100},{"path":"/nix/store/b","narSize":50}]"#).unwrap(),
            150
        );
        assert_eq!(
            nar_total(r#"{"/nix/store/a":{"narSize":7},"/nix/store/b":{"narSize":3}}"#).unwrap(),
            10
        );
        assert!(is_profile_path("profile") && !is_profile_path("glibc"));
    }

    #[test]
    fn meta() {
        let m = parse_meta(
            r#"{"description":"d","homepage":"https://h","license":{"spdxId":"MIT"},"longDescription":" long \n"}"#,
        )
        .unwrap();
        assert_eq!(m, "Description: d\nHomepage: https://h\nLicense: MIT\n\nlong\n");
    }
}
