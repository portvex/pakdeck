//! Queries against the local rpm database, shared by dnf/yum and zypper.

use super::{Package, capture, files_only, human_size, lines, run};
use anyhow::{Result, bail};

pub fn installed() -> Result<Vec<Package>> {
    let out = run(
        "rpm",
        &[
            "-qa",
            "--qf",
            "%{NAME}\t%{VERSION}-%{RELEASE}\t%{SIZE}\t%{INSTALLTIME}\t%{SUMMARY}\n",
        ],
    )?;
    let mut v = super::parse_sized(&out);
    v.retain(|p| p.name != "gpg-pubkey");
    Ok(v)
}

/// Installed size, for installed packages only.
pub fn size(name: &str) -> Result<Vec<String>> {
    let out = run("rpm", &["-q", "--qf", "%{SIZE}", name])?;
    match out.trim().parse::<u64>() {
        Ok(bytes) => Ok(vec![human_size(bytes)]),
        Err(_) => bail!("rpm: no size for {name}"),
    }
}

pub fn files(name: &str) -> Result<Vec<String>> {
    let mut v = lines(&run("rpm", &["-ql", name])?);
    v.retain(|l| l != "(contains no files)");
    Ok(files_only(v))
}

/// Packages providing what the installed package requires. Capabilities
/// no installed package provides are listed by their name only.
pub fn requires(name: &str) -> Result<Vec<String>> {
    let caps = parse_requires(&run("rpm", &["-qR", name])?);
    if caps.is_empty() {
        return Ok(caps);
    }
    let mut args = vec!["-q", "--whatprovides", "--qf", "%{NAME}\n"];
    args.extend(caps.iter().map(String::as_str));
    // Exits 1 if some capability is not provided; those lines say so.
    Ok(provider_names(&capture("rpm", &args, &[1])?, name))
}

/// `rpm -q --whatprovides CAP...` output as package names. A capability can
/// have several providers (several lines), so lines are not paired with
/// capabilities: the result is the set of names, in order, without `own`.
pub(crate) fn provider_names(out: &str, own: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let name = match line.strip_prefix("no package provides ") {
            Some(cap) => cap.to_string(),
            None => line.to_string(),
        };
        if name != own && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Installed packages that require `name` (exits 1 when there are none).
pub fn required_by(name: &str) -> Result<Vec<String>> {
    let out = capture("rpm", &["-q", "--whatrequires", "--qf", "%{NAME}\n", name], &[1])?;
    Ok(lines(&out)
        .into_iter()
        .filter(|l| !l.starts_with("no package"))
        .collect())
}

/// `rpm -qR`: the required capabilities without version constraints
/// ("libreadline8 >= 8.3" -> "libreadline8": `--whatprovides` takes one
/// capability per argument), deduplicated. Dropped: rpm's internal feature
/// markers, and rich (boolean) dependencies such as "(foo if bar)", which
/// are conditional and cannot be looked up as one capability.
pub(crate) fn parse_requires(out: &str) -> Vec<String> {
    let caps: Vec<String> = lines(out)
        .into_iter()
        .filter(|l| !l.starts_with("rpmlib(") && !l.starts_with('('))
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect();
    lines(&caps.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires() {
        let r = parse_requires("/bin/sh\nlibc.so.6()(64bit)\nrpmlib(CompressedFileNames) <= 3.0.4-1\n/bin/sh\n");
        assert_eq!(r, ["/bin/sh", "libc.so.6()(64bit)"]);
        // `rpm -qR bash` on openSUSE Tumbleweed.
        let r = parse_requires(
            "/bin/sh\nlibc.so.6()(64bit)\nlibc.so.6(GLIBC_2.38)(64bit)\nlibreadline.so.8()(64bit)\nlibreadline8 >= 8.3\n(glibc >= 2.38 if foo)\nrpmlib(PayloadIsZstd) <= 5.4.18-1\n",
        );
        assert_eq!(
            r,
            [
                "/bin/sh",
                "libc.so.6()(64bit)",
                "libc.so.6(GLIBC_2.38)(64bit)",
                "libreadline.so.8()(64bit)",
                "libreadline8"
            ]
        );
    }

    #[test]
    fn providers_are_a_set_of_names() {
        // Two providers for one capability, a duplicate, the package
        // itself, and a capability nothing installed provides.
        let out = "bash\nbusybox-sh\nglibc\nglibc\nlibreadline8\nlibreadline8\nno package provides weird-cap\n";
        assert_eq!(
            provider_names(out, "bash"),
            ["busybox-sh", "glibc", "libreadline8", "weird-cap"]
        );
    }
}
