//! Every package-manager output parser must survive arbitrary input.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pakdeck::backend::{Package, fuzz_parsers};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    fuzz_parsers(&text);
    // The history file may be torn or written by another version.
    for e in pakdeck::history::parse(&text) {
        let _ = e.not_undoable();
        let _ = pakdeck::history::undo_draft(&e);
    }

    // Search ranking over arbitrary names, first line as the query.
    let mut lines = text.lines();
    let query = lines.next().unwrap_or_default();
    let mut pkgs: Vec<Package> = lines.map(|l| Package::new(l, "")).collect();
    let n = pkgs.len();
    pakdeck::app::rank(&mut pkgs, query);
    assert_eq!(pkgs.len(), n);
});
