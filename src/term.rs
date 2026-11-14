//! Embedded terminal: runs package-manager commands in a pseudo-terminal
//! and keeps a vt100 screen of their output, so they can be shown inside
//! the TUI with colours, progress bars and interactive prompts (sudo
//! passwords, y/n questions) working as in a real terminal.

use crate::backend::{Cmd, resolve_argv};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const SCROLLBACK: usize = 5000;
/// After a command exits, wait this long at most for its last output.
const DRAIN: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Running,
    Finished { ok: bool },
}

pub struct Job {
    pub title: String,
    cmds: Vec<Cmd>,
    sudo: Option<String>,
    /// Index of the command currently (or last) running.
    pub index: usize,
    pub state: JobState,
    parser: Arc<Mutex<vt100::Parser>>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    size: (u16, u16),
    /// Set by the reader thread at end of output.
    eof: Arc<AtomicBool>,
    /// Exit seen, waiting for the remaining output: (when, success, code).
    exited: Option<(Instant, bool, u32)>,
    /// Ctrl-\ was pressed once (interrupt sent); the next press kills.
    interrupted: bool,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap_or(NonZeroU16::MIN)
}

impl Job {
    /// Start running `cmds` one after another in a `rows` x `cols` terminal.
    pub fn start(title: String, cmds: Vec<Cmd>, sudo: Option<String>, rows: u16, cols: u16) -> Self {
        let mut job = Self {
            title,
            cmds,
            sudo,
            index: 0,
            state: JobState::Running,
            parser: Arc::new(Mutex::new(vt100::Parser::new(nz(rows), nz(cols), SCROLLBACK))),
            master: None,
            writer: None,
            child: None,
            size: (rows.max(1), cols.max(1)),
            eof: Arc::new(AtomicBool::new(false)),
            exited: None,
            interrupted: false,
        };
        job.spawn_current();
        job
    }

    pub fn total(&self) -> usize {
        self.cmds.len()
    }

    /// The terminal screen; lock briefly while rendering.
    pub fn screen(&self) -> MutexGuard<'_, vt100::Parser> {
        self.parser.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn feed(&self, text: &str) {
        self.screen().process(text.replace('\n', "\r\n").as_bytes());
    }

    fn spawn_current(&mut self) {
        let Some(cmd) = self.cmds.get(self.index) else {
            return self.finish(false);
        };
        let line = cmd.display(self.sudo.as_deref());
        let prefix = if self.index == 0 { "" } else { "\n" };
        self.feed(&format!("{prefix}\x1b[1;36m$ {line}\x1b[0m\n"));
        if let Err(e) = self.try_spawn() {
            self.feed(&format!("\x1b[31mfailed to start: {e:#}\x1b[0m\n"));
            self.finish(false);
        }
    }

    fn try_spawn(&mut self) -> anyhow::Result<()> {
        let (rows, cols) = self.size;
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let argv = self
            .cmds
            .get(self.index)
            .map(|c| resolve_argv(c.argv(self.sudo.as_deref())))
            .unwrap_or_default();
        let Some((program, args)) = argv.split_first() else {
            anyhow::bail!("empty command");
        };
        let mut builder = CommandBuilder::new(program);
        builder.args(args);
        builder.env("TERM", "xterm-256color");
        // Wait for a package-manager lock held by a background query
        // instead of failing (zypper).
        builder.env("ZYPP_LOCK_TIMEOUT", "60");
        if let Ok(dir) = std::env::current_dir() {
            builder.cwd(dir);
        }
        let mut child = pair.slave.spawn_command(builder)?;
        // Close our copy of the slave so the reader sees EOF when the child exits.
        drop(pair.slave);
        // From here on, a setup failure must not leave the command running
        // unseen.
        let setup = (|| -> anyhow::Result<Box<dyn Write + Send>> {
            let mut reader = pair.master.try_clone_reader()?;
            let parser = self.parser.clone();
            let eof = Arc::new(AtomicBool::new(false));
            self.eof = eof.clone();
            std::thread::Builder::new().spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    let Some(data) = buf.get(..n).filter(|d| !d.is_empty()) else {
                        break;
                    };
                    parser.lock().unwrap_or_else(|e| e.into_inner()).process(data);
                }
                eof.store(true, Ordering::Release);
            })?;
            pair.master.take_writer()
        })();
        match setup {
            Ok(writer) => {
                self.writer = Some(writer);
                self.master = Some(pair.master);
                self.child = Some(child);
                Ok(())
            }
            Err(e) => {
                let _ = child.kill();
                Err(e)
            }
        }
    }

    fn finish(&mut self, ok: bool) {
        self.state = JobState::Finished { ok };
        self.writer = None;
        self.child = None;
        let msg = if ok {
            "\n\x1b[1;32mfinished\x1b[0m: press Enter to close\n"
        } else {
            "\n\x1b[1;31mfailed\x1b[0m: press Enter to close\n"
        };
        self.feed(msg);
    }

    /// Advance when the current command exits: start the next one, or
    /// finish. The command's last output is drained first (up to `DRAIN`),
    /// so it never lands after the footer or in the next command's section.
    pub fn poll(&mut self) {
        if self.state != JobState::Running {
            return;
        }
        if self.exited.is_none() {
            let Some(child) = self.child.as_mut() else { return };
            match child.try_wait() {
                Ok(Some(status)) => self.exited = Some((Instant::now(), status.success(), status.exit_code())),
                Ok(None) => return,
                Err(e) => {
                    self.feed(&format!("\n\x1b[31m{e}\x1b[0m"));
                    return self.finish(false);
                }
            }
        }
        let Some((at, ok, code)) = self.exited else { return };
        if !self.eof.load(Ordering::Acquire) && at.elapsed() < DRAIN {
            return;
        }
        self.exited = None;
        self.interrupted = false;
        if ok && self.index.saturating_add(1) < self.cmds.len() {
            self.index = self.index.saturating_add(1);
            self.spawn_current();
        } else {
            if !ok {
                self.feed(&format!("\n\x1b[31mexit status {code}\x1b[0m"));
            }
            self.finish(ok);
        }
    }

    /// Ctrl-\: first interrupt the command through the terminal (Ctrl-C;
    /// this also reaches commands running under sudo, which pakdeck may not
    /// signal directly); a second press kills it.
    pub fn stop(&mut self) {
        if self.interrupted {
            self.kill();
        } else {
            self.interrupted = true;
            self.send(&[0x03]);
            self.feed("\n\x1b[33m^C sent; press Ctrl-\\ again to kill\x1b[0m\n");
        }
    }

    pub fn kill(&mut self) {
        if let Some(c) = self.child.as_mut()
            && let Err(e) = c.kill()
        {
            self.feed(&format!("\n\x1b[31mcannot stop the command: {e}\x1b[0m\n"));
        }
    }

    /// How far the view can scroll back into the history.
    pub fn scroll_max(&self) -> usize {
        let mut p = self.screen();
        let cur = p.screen().scrollback();
        p.screen_mut().set_scrollback(usize::MAX);
        let max = p.screen().scrollback();
        p.screen_mut().set_scrollback(cur);
        max
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        self.screen().set_size(nz(rows), nz(cols));
        if let Some(m) = &self.master {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Scroll the view into the history (positive = older).
    pub fn scroll(&mut self, delta: isize) {
        let mut p = self.screen();
        let cur = p.screen().scrollback();
        p.screen_mut().set_scrollback(cur.saturating_add_signed(delta));
    }

    pub fn send(&mut self, bytes: &[u8]) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.write_all(bytes).and_then(|_| w.flush());
        }
    }

    /// Forward a key press to the running command.
    pub fn send_key(&mut self, key: KeyEvent) {
        let app_cursor = self.screen().screen().application_cursor();
        if let Some(bytes) = key_bytes(key, app_cursor) {
            // Typing returns the view to the live screen.
            self.screen().screen_mut().set_scrollback(0);
            self.send(&bytes);
        }
    }
}

/// Bytes a terminal would send for a key press.
pub fn key_bytes(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let arrow = |c: char| {
        let lead = if app_cursor { 'O' } else { '[' };
        format!("\x1b{lead}{c}").into_bytes()
    };
    let mut out = match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => vec![(c.to_ascii_lowercase() as u8) & 0x1f],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => arrow('A'),
        KeyCode::Down => arrow('B'),
        KeyCode::Right => arrow('C'),
        KeyCode::Left => arrow('D'),
        KeyCode::Home => arrow('H'),
        KeyCode::End => arrow('F'),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        _ => return None,
    };
    if alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let k = |code, m| key_bytes(KeyEvent::new(code, m), false).unwrap();
        assert_eq!(k(KeyCode::Char('y'), KeyModifiers::NONE), b"y");
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(key_bytes(KeyEvent::from(KeyCode::Up), true).unwrap(), b"\x1bOA");
        assert_eq!(k(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
    }

    fn wait(job: &mut Job, what: &str) -> String {
        for _ in 0..300 {
            job.poll();
            let text = job.screen().screen().contents();
            if text.contains(what) {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what:?}:\n{}", job.screen().screen().contents());
    }

    #[cfg(unix)]
    #[test]
    fn runs_commands_in_sequence_with_input() {
        let cmds = vec![
            Cmd::user("sh", &["-c", "printf 'Continue? [y/N] '; read a; echo answer=$a"]),
            Cmd::user("sh", &["-c", "echo second; test -t 0 && echo tty"]),
        ];
        let mut job = Job::start("test".into(), cmds, None, 20, 60);
        wait(&mut job, "Continue?");
        job.send_key(KeyEvent::from(KeyCode::Char('y')));
        job.send_key(KeyEvent::from(KeyCode::Enter));
        let text = wait(&mut job, "press Enter to close");
        assert!(text.contains("answer=y"), "{text}");
        assert!(text.contains("$ sh -c"), "{text}");
        assert!(text.contains("second") && text.contains("tty"), "{text}");
        assert_eq!(job.state, JobState::Finished { ok: true });
        assert_eq!(job.index, 1);
    }

    #[cfg(unix)]
    #[test]
    fn stops_at_first_failure() {
        let cmds = vec![
            Cmd::user("sh", &["-c", "exit 3"]),
            Cmd::user("sh", &["-c", "echo never"]),
        ];
        let mut job = Job::start("t".into(), cmds, None, 10, 40);
        let text = wait(&mut job, "failed");
        assert!(text.contains("exit status 3"), "{text}");
        assert!(!text.contains("never"));
        assert_eq!(job.state, JobState::Finished { ok: false });
    }

    #[test]
    fn missing_program_fails_cleanly() {
        let mut job = Job::start(
            "t".into(),
            vec![Cmd::user("pakdeck-no-such-program", &[])],
            None,
            10,
            40,
        );
        wait(&mut job, "press Enter to close");
        assert_eq!(job.state, JobState::Finished { ok: false });
    }

    #[cfg(unix)]
    #[test]
    fn kill_and_scrollback() {
        let mut job = Job::start(
            "t".into(),
            vec![Cmd::user("sh", &["-c", "seq 1 100; sleep 30"])],
            None,
            10,
            40,
        );
        // "100" alone would match the echoed command line.
        wait(&mut job, "99\n100");
        job.scroll(50);
        assert!(job.screen().screen().scrollback() > 0);
        job.scroll(-1000);
        assert_eq!(job.screen().screen().scrollback(), 0);
        job.kill();
        wait(&mut job, "press Enter to close");
        assert_eq!(job.state, JobState::Finished { ok: false });
    }
}
