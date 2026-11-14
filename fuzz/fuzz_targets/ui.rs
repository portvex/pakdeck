//! Drive the real UI with arbitrary key, mouse and resize events on an
//! in-memory terminal (mock backend, inline loading, dry-run, sandboxed so
//! no process is ever started). Rendering and input handling must never
//! panic at any terminal size, including with an unsupported-everything
//! backend.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

#[derive(Arbitrary, Debug)]
enum Key {
    Enter,
    Esc,
    Backspace,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Arbitrary, Debug)]
enum Mouse {
    Left,
    Right,
    Drag,
    Release,
    ScrollUp,
    ScrollDown,
}

#[derive(Arbitrary, Debug)]
enum Event {
    Char(char),
    Ctrl(char),
    Key(Key),
    Mouse(Mouse, u8, u8),
    Resize(u8, u8),
    Log(String),
}

#[derive(Arbitrary, Debug)]
struct Input {
    unsupported: bool,
    multi: bool,
    width: u8,
    height: u8,
    events: Vec<Event>,
}

fn key(k: Key) -> KeyCode {
    match k {
        Key::Enter => KeyCode::Enter,
        Key::Esc => KeyCode::Esc,
        Key::Backspace => KeyCode::Backspace,
        Key::Tab => KeyCode::Tab,
        Key::BackTab => KeyCode::BackTab,
        Key::Up => KeyCode::Up,
        Key::Down => KeyCode::Down,
        Key::Left => KeyCode::Left,
        Key::Right => KeyCode::Right,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
    }
}

fuzz_target!(|input: Input| {
    let size = |w: u8, h: u8| Rect::new(0, 0, u16::from(w).max(1), u16::from(h).max(1));
    let area = size(input.width, input.height);
    let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
    // One backend, or two at once (source column, routing, source filter).
    let mock = |name| pakdeck::testing::Mock {
        unsupported: input.unsupported,
        ..pakdeck::testing::Mock::named(name)
    };
    let mut app = if input.multi {
        pakdeck::testing::mock_app_multi(vec![mock("one"), mock("two")])
    } else {
        pakdeck::testing::mock_app_multi(vec![mock("mock")])
    };

    for ev in input.events.into_iter().take(256) {
        app.tick();
        term.draw(|f| pakdeck::ui::draw(f, &mut app)).unwrap();
        match ev {
            Event::Char(c) => app.on_key(KeyEvent::from(KeyCode::Char(c))),
            Event::Ctrl(c) => app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
            Event::Key(k) => app.on_key(KeyEvent::from(key(k))),
            Event::Mouse(m, x, y) => {
                let kind = match m {
                    Mouse::Left => MouseEventKind::Down(MouseButton::Left),
                    Mouse::Right => MouseEventKind::Down(MouseButton::Right),
                    Mouse::Drag => MouseEventKind::Drag(MouseButton::Left),
                    Mouse::Release => MouseEventKind::Up(MouseButton::Left),
                    Mouse::ScrollUp => MouseEventKind::ScrollUp,
                    Mouse::ScrollDown => MouseEventKind::ScrollDown,
                };
                let modifiers = KeyModifiers::NONE;
                app.on_mouse(MouseEvent {
                    kind,
                    column: x.into(),
                    row: y.into(),
                    modifiers,
                });
            }
            Event::Resize(w, h) => {
                let r = size(w, h);
                term.backend_mut().resize(r.width, r.height);
                term.resize(r).unwrap();
            }
            Event::Log(s) => app.log(s),
        }
        // Dry-run can be toggled off by the input; never execute anything.
        if app.run_request.take().is_some() {
            app.after_run(true);
        }
        if app.quit {
            break;
        }
    }
    app.tick();
    term.draw(|f| pakdeck::ui::draw(f, &mut app)).unwrap();
});
