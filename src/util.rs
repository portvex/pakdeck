//! Small helpers shared by several modules: atomic file writes and
//! dates without a time zone database.

use anyhow::{Context, Result};
use std::path::Path;

/// Write `text` to `path` atomically: a crash or full disk never leaves a
/// truncated file behind. A symlink is followed and the file it points to
/// is replaced (dotfile managers link config files; replacing the link
/// would quietly detach it), keeping that file's permissions. A read-only
/// file (one managed by another tool) is refused, not replaced.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    // A dangling link (its file not created yet) is followed too, so the
    // file is created where the link points.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| match std::fs::read_link(path) {
        Ok(to) => path.parent().map_or_else(|| to.clone(), |dir| dir.join(&to)),
        Err(_) => path.to_path_buf(),
    });
    let old = std::fs::metadata(&target).ok();
    if old.as_ref().is_some_and(|m| m.permissions().readonly()) {
        anyhow::bail!("{} is read-only (managed by another tool?)", target.display());
    }
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = target.with_file_name(format!(".{name}.pakdeck-tmp"));
    let written = std::fs::write(&tmp, text)
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| match &old {
            Some(m) => std::fs::set_permissions(&tmp, m.permissions())
                .with_context(|| format!("setting the permissions of {}", tmp.display())),
            None => Ok(()),
        })
        .and_then(|()| std::fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display())));
    if written.is_err() {
        // Never leave the temporary file behind.
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Seconds since the Unix epoch as YYYY-MM-DD (UTC); "?" if out of range.
pub fn date(secs: u64) -> String {
    civil(secs).map_or_else(|| "?".into(), |(y, m, d)| format!("{y:04}-{m:02}-{d:02}"))
}

/// "2026-11-13 10:05" (UTC; pakdeck has no time zone database).
pub fn date_time(secs: u64) -> String {
    let day = secs.checked_rem(86_400).unwrap_or(0);
    let (h, m) = (
        day.checked_div(3_600).unwrap_or(0),
        day.checked_rem(3_600).unwrap_or(0).checked_div(60).unwrap_or(0),
    );
    format!("{} {h:02}:{m:02}", date(secs))
}

/// Howard Hinnant's civil-from-days algorithm, with checked arithmetic.
fn civil(secs: u64) -> Option<(i64, i64, i64)> {
    let days = i64::try_from(secs.checked_div(86_400)?).ok()?;
    let z = days.checked_add(719_468)?;
    let era = z.checked_div_euclid(146_097)?;
    let doe = z.checked_rem_euclid(146_097)?;
    let yoe = doe
        .checked_sub(doe.checked_div(1_460)?)?
        .checked_add(doe.checked_div(36_524)?)?
        .checked_sub(doe.checked_div(146_096)?)?
        .checked_div(365)?;
    let doy = doe.checked_sub(
        yoe.checked_mul(365)?
            .checked_add(yoe.checked_div(4)?)?
            .checked_sub(yoe.checked_div(100)?)?,
    )?;
    let mp = doy.checked_mul(5)?.checked_add(2)?.checked_div(153)?;
    let day = doy
        .checked_sub(mp.checked_mul(153)?.checked_add(2)?.checked_div(5)?)?
        .checked_add(1)?;
    let month = if mp < 10 {
        mp.checked_add(3)?
    } else {
        mp.checked_sub(9)?
    };
    let year = yoe
        .checked_add(era.checked_mul(400)?)?
        .checked_add(i64::from(month <= 2))?;
    (0..=9999).contains(&year).then_some((year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces() {
        let dir = std::env::temp_dir().join(format!("pakdeck-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("list.toml");
        write_atomic(&f, "one").unwrap();
        write_atomic(&f, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "two");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "temp file left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_symlinks_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pakdeck-link-{}", std::process::id()));
        let real = dir.join("dotfiles").join("config.toml");
        let link = dir.join("config.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, "new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "link replaced"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
        // A read-only target (home-manager's store) is refused and left alone.
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o444)).unwrap();
        let err = write_atomic(&link, "x").unwrap_err();
        assert!(format!("{err:#}").contains("read-only"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        let leftovers = std::fs::read_dir(real.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temp file left behind");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        // A dangling link: the file is created where it points.
        let fresh = dir.join("dotfiles").join("new.toml");
        let link2 = dir.join("new.toml");
        std::os::unix::fs::symlink(&fresh, &link2).unwrap();
        write_atomic(&link2, "made").unwrap();
        assert!(
            std::fs::symlink_metadata(&link2).unwrap().file_type().is_symlink(),
            "dangling link replaced"
        );
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "made");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date_time(1_763_030_700), "2025-11-13 10:45");
    }
}
