//! The command line shown in the confirm popup and dry-run log must be a
//! faithful shell rendering: splitting it like a POSIX shell gives back
//! exactly the argv that will be executed.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pakdeck::backend::Cmd;

fuzz_target!(|input: (bool, Vec<String>)| {
    let (root, args) = input;
    // NUL cannot appear in a real argv.
    if args.is_empty() || args.iter().any(|a| a.contains('\0')) {
        return;
    }
    let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    let cmd = if root {
        Cmd::root(&args[0], &rest)
    } else {
        Cmd::user(&args[0], &rest)
    };
    for sudo in [None, Some("sudo")] {
        let line = cmd.display(sudo);
        let split = shlex::split(&line).unwrap_or_else(|| panic!("unparsable: {line:?}"));
        assert_eq!(split, cmd.argv(sudo), "line: {line:?}");
    }
});
