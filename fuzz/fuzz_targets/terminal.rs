//! The embedded terminal renders whatever bytes a package manager prints.
//! Feed arbitrary output through vt100 at arbitrary sizes and render it the
//! way the UI does; nothing may panic. Key encoding is fuzzed too.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Arbitrary, Debug)]
struct Input {
    rows: u8,
    cols: u8,
    resize: Option<(u8, u8)>,
    scroll: i16,
    output: Vec<u8>,
    keys: Vec<(char, u8)>,
}

fuzz_target!(|input: Input| {
    let nz = |v: u8| std::num::NonZeroU16::new(u16::from(v).max(1)).unwrap_or(std::num::NonZeroU16::MIN);
    let mut parser = vt100::Parser::new(nz(input.rows), nz(input.cols), 100);
    parser.process(&input.output);
    if let Some((r, c)) = input.resize {
        parser.set_size(nz(r), nz(c));
        parser.process(&input.output);
    }
    let cur = parser.screen().scrollback();
    parser
        .screen_mut()
        .set_scrollback(cur.saturating_add_signed(isize::from(input.scroll)));
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    for r in 0..rows.get() {
        for c in 0..cols.get() {
            if let Some(cell) = screen.cell(r, c) {
                let _ = (
                    cell.contents(),
                    cell.fgcolor(),
                    cell.bgcolor(),
                    cell.is_wide_continuation(),
                );
            }
        }
    }
    let _ = screen.cursor_position();
    for (ch, mods) in input.keys.into_iter().take(64) {
        let m = KeyModifiers::from_bits_truncate(mods);
        let _ = pakdeck::term::key_bytes(KeyEvent::new(KeyCode::Char(ch), m), mods & 1 == 1);
    }
});
