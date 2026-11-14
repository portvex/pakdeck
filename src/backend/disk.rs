//! Disk usage: totals per package manager and the largest packages, shared
//! by `--disk` and the TUI's disk usage popup.

use super::Package;
use std::collections::HashMap;

/// Installed size of one package manager's packages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    pub source: String,
    /// Sum of the known sizes.
    pub bytes: u64,
    /// Packages with a known size.
    pub sized: usize,
    /// All installed packages.
    pub packages: usize,
}

impl DiskUsage {
    /// Some packages have no size (yet).
    pub fn incomplete(&self) -> bool {
        self.sized < self.packages
    }
}

/// Totals per source, largest first, and the `top` largest packages.
pub fn disk_summary(pkgs: &[Package], top: usize) -> (Vec<DiskUsage>, Vec<Package>) {
    let mut totals: Vec<DiskUsage> = Vec::new();
    for p in pkgs {
        if !totals.iter().any(|t| t.source == p.source) {
            totals.push(DiskUsage {
                source: p.source.clone(),
                ..DiskUsage::default()
            });
        }
        if let Some(t) = totals.iter_mut().find(|t| t.source == p.source) {
            t.packages = t.packages.saturating_add(1);
            if let Some(size) = p.size {
                t.bytes = t.bytes.saturating_add(size);
                t.sized = t.sized.saturating_add(1);
            }
        }
    }
    totals.sort_by_key(|t| std::cmp::Reverse(t.bytes));
    let mut largest: Vec<Package> = pkgs.iter().filter(|p| p.size.is_some()).cloned().collect();
    largest.sort_by_key(|p| std::cmp::Reverse(p.size));
    largest.truncate(top);
    (totals, largest)
}

/// Set `size` on packages that have none, from measured (`Package::key()`,
/// bytes) pairs.
pub fn apply_sizes(pkgs: &mut [Package], sizes: &HashMap<String, u64>) {
    for p in pkgs.iter_mut().filter(|p| p.size.is_none()) {
        p.size = sizes.get(&p.key()).copied();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_and_largest() {
        let pkgs = [
            Package::new("a", "").source("x").sized(Some(5)),
            Package::new("b", "").source("y").sized(Some(50)),
            Package::new("c", "").source("x"),
            Package::new("d", "").source("x").sized(Some(7)),
        ];
        let (totals, largest) = disk_summary(&pkgs, 2);
        let x = DiskUsage {
            source: "x".into(),
            bytes: 12,
            sized: 2,
            packages: 3,
        };
        assert_eq!(totals.get(1), Some(&x));
        assert!(x.incomplete());
        assert_eq!(totals.first().map(|t| (t.source.as_str(), t.bytes)), Some(("y", 50)));
        assert_eq!(largest.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["b", "d"]);
    }

    #[test]
    fn applies_by_key_and_keeps_known_sizes() {
        let mut pkgs = vec![
            Package::new("docker", "").source("brew").repo("formula"),
            Package::new("docker", "").source("brew").repo("cask"),
            Package::new("tree", "").source("brew").repo("formula").sized(Some(1)),
        ];
        let sizes = HashMap::from([
            ("brew/formula/docker".to_string(), 10),
            ("brew/cask/docker".to_string(), 30),
            ("brew/formula/tree".to_string(), 99),
        ]);
        apply_sizes(&mut pkgs, &sizes);
        assert_eq!(
            pkgs.iter().map(|p| p.size).collect::<Vec<_>>(),
            [Some(10), Some(30), Some(1)]
        );
    }
}
