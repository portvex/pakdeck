//! Rendering. Nothing here may panic: no indexing, saturating arithmetic.

use crate::app::{Action, App, Hits, LoadErr, Mode, SectionState, SortKey, Tab};
use crate::backend::Section;
use crate::term::JobState;
use crate::theme::Colors;
use ratatui::Frame;
#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Flex, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

fn key_name(c: char) -> String {
    match c {
        ' ' => "space".into(),
        crate::config::UNBOUND => "(unbound)".into(),
        c => c.to_string(),
    }
}

/// The theme's background and text colour.
fn base_style(c: crate::theme::Colors) -> Style {
    Style::new().bg(c.bg).fg(c.fg)
}

/// Blank `area` for a popup, in the theme's colours (a plain `Clear`
/// would show the terminal's background through a themed screen).
fn clear(f: &mut Frame, area: Rect, c: crate::theme::Colors) {
    f.render_widget(Clear, area);
    f.render_widget(Block::new().style(base_style(c)), area);
}

/// The selected row: highlighted with `selected_bg`, or reversed for
/// themes that use only the terminal's own colours (or none).
fn selected_style(c: crate::theme::Colors) -> Style {
    if c.reverse_selection {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new().bg(c.selected_bg).add_modifier(Modifier::BOLD)
    }
}

/// Display width of a short string, saturating into u16.
fn width(s: &str) -> u16 {
    u16::try_from(s.chars().count()).unwrap_or(u16::MAX)
}

fn len16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let c = app.colors;
    app.hits = Hits::default();
    // The theme's background and text colour under everything (Reset for
    // themes that keep the terminal's).
    f.render_widget(Block::new().style(base_style(c)), f.area());
    let [top, input, main, log, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(6),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // Status (right) is sized to its content; tabs get the rest and drop
    // detail (counts, then full names) until all four fit, so every tab
    // stays visible and clickable on an 80-column terminal.
    let mode = match app.run_mode {
        crate::config::RunMode::Embedded => "embedded",
        crate::config::RunMode::Suspend => "suspend",
    };
    let sources = format!("[{}]", app.backend.names().join("+"));
    let mode_label = format!("[{mode}]");
    let dry = if app.dry_run { " DRY RUN " } else { "" };
    let status_w = width(&sources)
        .saturating_add(width(&mode_label))
        .saturating_add(width(dry))
        .saturating_add(3);
    let [tabs_area, status_area] = Layout::horizontal([Constraint::Min(0), Constraint::Length(status_w)]).areas(top);
    let label = |t: Tab, level: u8| -> String {
        let p = app.panes.get(t);
        let count = if p.loading {
            "...".into()
        } else if p.unsupported.is_some() {
            "-".into()
        } else if t == Tab::Installed && app.explicit_only {
            format!("{}/{}", p.view.len(), p.items.len())
        } else {
            p.items.len().to_string()
        };
        match level {
            0 => format!(" {} ({count}) ", t.title()),
            1 => format!(" {} ", t.title()),
            _ => format!(" {} ", t.title().chars().take(4).collect::<String>()),
        }
    };
    let fits = |level: u8| {
        Tab::ALL.iter().fold(0u16, |w, t| {
            w.saturating_add(width(&label(*t, level))).saturating_add(1)
        }) <= tabs_area.width
    };
    let level = (0..2u8).find(|l| fits(*l)).unwrap_or(2);
    let mut spans = Vec::new();
    let mut x = tabs_area.x;
    for t in Tab::ALL {
        let title = label(t, level);
        let w = width(&title);
        app.hits
            .tabs
            .push((Rect::new(x, tabs_area.y, w, 1).intersection(tabs_area), t));
        let style = if t == app.tab && c.accent == Color::Reset {
            // No colours (monochrome): the active tab is reversed.
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else if t == app.tab {
            Style::new().fg(c.on_accent).bg(c.accent).bold()
        } else {
            Style::new()
        };
        spans.push(Span::styled(title, style));
        spans.push(Span::raw("|").fg(c.dim));
        x = x.saturating_add(w).saturating_add(1);
    }
    spans.pop();
    f.render_widget(Line::from(spans), tabs_area);
    // " [apt+flatpak] [embedded] DRY RUN": the first two are clickable.
    let sx = status_area.x.saturating_add(1);
    app.hits.status_source = Rect::new(sx, top.y, width(&sources), 1).intersection(status_area);
    let mx = sx.saturating_add(width(&sources)).saturating_add(1);
    app.hits.status_mode = Rect::new(mx, top.y, width(&mode_label), 1).intersection(status_area);
    let mut status = vec![
        Span::raw(" "),
        Span::raw(sources).fg(c.accent),
        Span::raw(" "),
        Span::raw(mode_label).fg(c.accent),
        Span::raw(" "),
    ];
    if app.dry_run {
        // The safety signal: whichever of black or white reads best.
        status.push(
            Span::raw(dry)
                .fg(crate::theme::text_on(c.marked, c.on_accent))
                .bg(c.marked)
                .bold(),
        );
    }
    f.render_widget(Line::from(status), status_area);

    // Search / filter box.
    let editing = matches!(app.mode, Mode::Input);
    let (label, text, hint) = if app.tab == Tab::Search {
        (
            " Search ",
            app.query.as_str(),
            " Enter: run, Up/Down: earlier searches ",
        )
    } else {
        (" Filter ", app.pane().filter.as_str(), " live filter, Esc: clear ")
    };
    let border = if editing {
        Style::new().fg(c.accent)
    } else {
        Style::new().fg(c.dim)
    };
    let shown = if text.is_empty() && !editing {
        Span::raw(format!(
            "press {} to {}",
            key_name(app.cfg.keys.search),
            label.trim().to_lowercase()
        ))
        .fg(c.dim)
    } else {
        Span::raw(text.to_string())
    };
    // A repository filter hides packages too: say so where filters live.
    let mut shown = vec![shown];
    if app.tab == Tab::Installed
        && let Some(repo) = &app.repo_filter
    {
        let kind = crate::backend::Backend::repositories(app.backend.as_ref())
            .map_or("repository".to_string(), |r| crate::app::singular(r.kind()));
        shown.push(Span::raw(format!("   {kind}: {} (Esc clears)", repo.name)).fg(c.accent));
    }
    f.render_widget(
        Paragraph::new(Line::from(shown)).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(border)
                .title(label)
                .title_bottom(Line::from(hint).right_aligned()),
        ),
        input,
    );
    let text_len = width(text);
    app.hits.input = input;
    if editing {
        let x = input.x.saturating_add(1).saturating_add(text_len);
        f.set_cursor_position((x.min(input.right().saturating_sub(2)), input.y.saturating_add(1)));
    }

    // Package list and details.
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main);
    draw_list(f, app, list_area);
    draw_details(f, app, detail_area);

    // Log. `log_scroll` counts lines up from the newest entry.
    let height = usize::from(log.height.saturating_sub(2));
    let max_scroll = app.log.len().saturating_sub(height);
    app.log_scroll = app.log_scroll.min(max_scroll);
    app.log_max = max_scroll;
    let start = max_scroll.saturating_sub(app.log_scroll);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(start)
        .take(height)
        .map(|l| {
            let style = if l.starts_with("error") || l.contains("failed") {
                Style::new().fg(c.error)
            } else if l.starts_with("warning") {
                Style::new().fg(c.marked).bold()
            } else if l.starts_with("[dry-run]") {
                Style::new().fg(c.marked)
            } else {
                Style::new()
            };
            Line::styled(l.clone(), style)
        })
        .collect();
    let log_title = if app.log_scroll > 0 {
        format!(" Log (+{} newer) ", app.log_scroll)
    } else {
        " Log ".into()
    };
    // A clickable "clear" label on the right of the title (its key too).
    let clear_label = format!(" {}: clear ", key_name(app.cfg.keys.clear_log));
    let clear_w = width(&clear_label);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(log_title)
                .title(Line::from(clear_label).right_aligned())
                .border_style(Style::new().fg(c.dim)),
        ),
        log,
    );
    app.hits.log = log;
    app.hits.log_clear =
        Rect::new(log.right().saturating_sub(clear_w.saturating_add(1)), log.y, clear_w, 1).intersection(log);
    app.hits.log_bar = scrollbar(f, log, app.log.len(), height, start, c);

    // Footer.
    let k = app.cfg.keys;
    let hints = [
        (k.search, "search", Action::Search),
        (k.mark, "mark", Action::Mark),
        (k.install, "install", Action::Install),
        (k.delete, "delete", Action::Delete),
        (k.update, "update", Action::Update),
        (k.update_all, "update all", Action::UpdateAll),
        (k.cleanup, "cleanup", Action::Cleanup),
        (k.sync, "sync", Action::Sync),
        (k.hold, "hold", Action::Hold),
        (k.explicit, "explicit", Action::Explicit),
        (k.expand, "expand", Action::Expand),
        (k.dry_run, "dry-run", Action::DryRun),
        // Shown only on wide terminals (the status bar labels also click).
        (k.source, "source", Action::Source),
        (k.export, "export", Action::Export),
        (k.import, "import", Action::Import),
        (k.run_mode, "run mode", Action::RunMode),
        (k.sort, "sort", Action::Sort),
        (k.versions, "versions", Action::Versions),
        (k.theme, "theme", Action::Theme),
        (k.tree, "tree", Action::Tree),
        (k.disk, "disk", Action::Disk),
        (k.repos, "repos", Action::Repos),
        (k.history, "history", Action::History),
        (k.clear_log, "clear log", Action::ClearLog),
        (k.reload, "reload", Action::Reload),
    ];
    // Help and quit are always shown; other hints only while they fit.
    let tail = [(k.help, "help", Action::Help), (k.quit, "quit", Action::Quit)];
    let hint_width = |key: char, what: &str| width(&key_name(key)).saturating_add(width(what)).saturating_add(4);
    let tail_width = tail
        .iter()
        .fold(0u16, |w, (key, what, _)| w.saturating_add(hint_width(*key, what)));
    let budget = footer.width.saturating_sub(tail_width);
    let mut used = 0u16;
    let mut shown = Vec::new();
    for h in hints.into_iter().filter(|h| h.0 != crate::config::UNBOUND) {
        let w = hint_width(h.0, h.1);
        if used.saturating_add(w) <= budget {
            used = used.saturating_add(w);
            shown.push(h);
        }
    }
    shown.extend(tail);
    let mut spans = Vec::new();
    let mut x = footer.x;
    for (key, what, action) in shown {
        let w = hint_width(key, what);
        app.hits
            .footer
            .push((Rect::new(x, footer.y, w, 1).intersection(footer), action));
        x = x.saturating_add(w);
        spans.push(Span::raw(format!(" {} ", key_name(key))).fg(c.on_accent).bg(c.accent));
        spans.push(Span::raw(format!(" {what} ")));
    }
    f.render_widget(Line::from(spans), footer);

    match std::mem::replace(&mut app.mode, Mode::Normal) {
        Mode::Confirm(p) => {
            draw_confirm(f, app, &p);
            app.mode = Mode::Confirm(p);
        }
        Mode::Help => {
            draw_help(f, app);
            app.mode = Mode::Help;
        }
        Mode::Versions(pick) => {
            draw_versions(f, app, &pick);
            app.mode = Mode::Versions(pick);
        }
        Mode::Repos(v) => {
            draw_repos(f, app, &v);
            app.mode = Mode::Repos(v);
        }
        Mode::Tree(t) => {
            draw_tree(f, app, &t);
            app.mode = Mode::Tree(t);
        }
        Mode::Disk(d) => {
            draw_disk(f, app, &d);
            app.mode = Mode::Disk(d);
        }
        Mode::Themes(t) => {
            draw_themes(f, app, &t);
            app.mode = Mode::Themes(t);
        }
        Mode::History(v) => {
            draw_history(f, app, &v);
            app.mode = Mode::History(v);
        }
        other => app.mode = other,
    }
    if app.job.is_some() {
        draw_job(f, app);
    }
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let c = app.colors;
    let tab = app.tab;
    let multi = app.backend.is_multi();
    let pane = app.panes.get(tab);
    let held = &app.held;
    let sort = pane.sort;
    let none: Vec<usize> = Vec::new();
    let selected_row = pane.state.selected();
    let items: Vec<ListItem> = pane
        .view
        .iter()
        .enumerate()
        .filter_map(|(row, &i)| {
            pane.items
                .get(i)
                .map(|p| (row, p, pane.highlights.get(row).unwrap_or(&none)))
        })
        .map(|(row, p, hits)| {
            let marked = pane.marked.contains(&p.key());
            let mut spans = vec![
                if marked {
                    Span::raw("[x] ").fg(c.marked).bold()
                } else {
                    Span::raw("[ ] ").fg(c.dim)
                },
                if held.contains(&p.id()) {
                    Span::raw("H ").fg(c.marked).bold()
                } else if p.installed && tab == Tab::Search {
                    Span::raw("i ").fg(c.installed).bold()
                } else {
                    Span::raw("  ")
                },
            ];
            spans.extend(highlighted(&p.name, hits, c.accent));
            if multi {
                spans.insert(2, Span::raw(format!("[{}] ", p.source)).fg(c.accent));
            }
            if !p.version.is_empty() {
                spans.push(Span::raw(format!(" {}", p.version)).fg(c.dim));
            }
            if let Some(nv) = &p.new_version {
                spans.push(Span::raw(format!(" -> {nv}")).fg(c.update));
            }
            // "(cask)", "(updates itself)" or both in one bracket.
            let tags: Vec<&str> = (p.repo == "cask")
                .then_some("cask")
                .into_iter()
                .chain(p.note.as_deref())
                .collect();
            if !tags.is_empty() {
                spans.push(Span::raw(format!(" ({})", tags.join(", "))).fg(c.dim));
            }
            // Show the value the list is sorted by.
            match sort {
                SortKey::Size => {
                    let v = p.size.map_or_else(|| "?".into(), crate::backend::human_size);
                    spans.push(Span::raw(format!("  {v}")).fg(c.accent));
                }
                SortKey::Date => {
                    let v = p.installed_at.map_or_else(|| "?".into(), crate::util::date);
                    spans.push(Span::raw(format!("  {v}")).fg(c.accent));
                }
                _ => {}
            }
            // On the selected row dim text takes the normal colour: dim on
            // the selection background is too faint in several themes.
            if selected_row == Some(row) {
                for s in &mut spans {
                    if s.style.fg == Some(c.dim) {
                        s.style.fg = None;
                    }
                }
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut title = format!(" {} ", tab.title());
    // The count first: on a narrow list the tags after it may be cut off.
    // While Updates may refresh a catalogue, the title stays short and the
    // explanation goes in the (empty) list body, which wraps.
    let updates_note = (tab == Tab::Updates && pane.loading)
        .then(|| crate::backend::Backend::updates_note(app.backend.as_ref()))
        .flatten();
    if pane.loading {
        title.push_str(if updates_note.is_some() {
            "checking... "
        } else {
            "loading... "
        });
    } else if pane.view.len() != pane.items.len() {
        title.push_str(&format!("{}/{} ", pane.view.len(), pane.items.len()));
    }
    if sort != SortKey::Default {
        title.push_str(&format!("(by {}) ", sort.label()));
    }
    if tab == Tab::Installed && app.explicit_only {
        title.push_str("(explicit) ");
    }
    if let Some(src) = &app.source_filter {
        title.push_str(&format!("[{src} only] "));
    }
    if tab == Tab::Installed
        && let Some(repo) = &app.repo_filter
    {
        title.push_str(&format!("[from {}] ", repo.name));
    }
    if !pane.marked.is_empty() {
        let visible = pane
            .view
            .iter()
            .filter_map(|&i| pane.items.get(i))
            .filter(|p| pane.marked.contains(&p.key()))
            .count();
        let hidden = pane.marked.len().saturating_sub(visible);
        if hidden > 0 {
            title.push_str(&format!("[{visible} marked, {hidden} hidden] "));
        } else {
            title.push_str(&format!("[{visible} marked] "));
        }
    }
    let empty = pane.view.is_empty() && (!pane.loading || updates_note.is_some());
    app.hits.list = area.inner(Margin::new(1, 1));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::new().fg(c.accent));
    if empty {
        let msg = match (&pane.unsupported, tab) {
            _ if updates_note.is_some() => updates_note.clone().unwrap_or_default(),
            (Some(m), _) => format!("Not available: {m}."),
            (None, Tab::Search) if app.query.is_empty() => {
                format!("Type {} and a package name to search.", key_name(app.cfg.keys.search))
            }
            (None, Tab::Updates) => format!(
                "No updates found. Press {} to sync repositories.",
                key_name(app.cfg.keys.sync)
            ),
            (None, Tab::Orphans) => "No orphaned packages.".into(),
            (None, _) => "No packages.".into(),
        };
        f.render_widget(
            Paragraph::new(msg).fg(c.dim).block(block).wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(selected_style(c))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.panes.get_mut(tab).state);
    let pane = app.panes.get(tab);
    let (total, offset) = (pane.view.len(), pane.state.offset());
    app.hits.list_bar = scrollbar(f, area, total, usize::from(area.height.saturating_sub(2)), offset, c);
}

/// Draw a vertical scrollbar over the right border of a bordered `area` when
/// `total` rows do not fit in `viewport` rows. Returns the bar's column.
fn scrollbar(f: &mut Frame, area: Rect, total: usize, viewport: usize, offset: usize, colors: Colors) -> Rect {
    if total <= viewport || area.height < 3 || area.width < 2 {
        return Rect::default();
    }
    let track = area.inner(Margin::new(0, 1));
    // content_length counts scroll positions, so the thumb reaches the
    // bottom exactly when the last row is visible.
    let mut state = ScrollbarState::new(total.saturating_sub(viewport).saturating_add(1))
        .position(offset)
        .viewport_content_length(viewport);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("\u{2502}"))
        .track_style(Style::new().fg(colors.dim))
        .thumb_symbol("\u{2588}")
        .thumb_style(Style::new().fg(colors.accent));
    f.render_stateful_widget(bar, track, &mut state);
    Rect::new(track.right().saturating_sub(1), track.y, 1, track.height)
}

fn draw_details(f: &mut Frame, app: &mut App, area: Rect) {
    app.hits.details = area;
    let c = app.colors;
    let k = app.cfg.keys;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" Details (J/K scroll, {} expand) ", key_name(k.expand)))
        .border_style(Style::new().fg(c.dim));
    let Some(p) = app.selected().cloned() else {
        f.render_widget(block, area);
        return;
    };
    let key = p.key();
    let field = |k: &str, v: String, style: Style| {
        Line::from(vec![Span::raw(format!("{k:<10}")).fg(c.dim), Span::styled(v, style)])
    };
    let mut lines = vec![field("Name", p.name.clone(), Style::new().bold().fg(c.accent))];
    if !p.version.is_empty() {
        lines.push(field("Version", p.version.clone(), Style::new()));
    }
    if let Some(nv) = &p.new_version {
        lines.push(field("Available", nv.clone(), Style::new().fg(c.update)));
    }
    if let Some(note) = &p.note {
        lines.push(field("Note", note.clone(), Style::new()));
    }
    if !p.repo.is_empty() {
        lines.push(field("Repo", p.repo.clone(), Style::new()));
    }
    let installed = app.is_installed(&p);
    lines.push(field(
        "Status",
        if installed {
            "installed".into()
        } else {
            "not installed".into()
        },
        Style::new().fg(if installed { c.installed } else { c.muted }),
    ));
    if !p.description.is_empty() {
        lines.push(field("Summary", p.description.clone(), Style::new()));
    }
    if app.backend.is_multi() {
        lines.push(field("Source", p.source.clone(), Style::new().fg(c.accent)));
    }
    if app.is_held(&p) {
        lines.push(field(
            "Held",
            "yes: excluded from updates".into(),
            Style::new().fg(c.marked),
        ));
    }
    let size = match app.sections.get(&(key.clone(), Section::Size)) {
        Some(SectionState::Ready(v)) => v.first().cloned(),
        Some(SectionState::Loading) | None => Some("...".into()),
        Some(SectionState::Error(_)) => None,
    };
    if let Some(size) = size.filter(|s| !s.is_empty()) {
        lines.push(field("Size", size, Style::new()));
    }

    // Collapsible sections; remember which line each header is on.
    let mut headers: Vec<(usize, Section)> = Vec::new();
    for s in [Section::Depends, Section::RequiredBy, Section::Files] {
        lines.push(Line::raw(""));
        headers.push((lines.len(), s));
        let open = app.expanded.contains(&s);
        let marker = if open { "[-]" } else { "[+]" };
        let state = app.sections.get(&(key.clone(), s));
        let suffix = match state {
            Some(SectionState::Ready(v)) => format!(" ({})", v.len()),
            Some(SectionState::Loading) => " (loading...)".into(),
            Some(SectionState::Error(LoadErr::Unsupported(_))) => ": not available".into(),
            Some(SectionState::Error(LoadErr::Failed(_))) => ": error".into(),
            None => " (click to load)".into(),
        };
        lines.push(Line::from(vec![
            Span::raw(format!("{marker} ")).fg(c.accent),
            Span::raw(s.title()).bold(),
            Span::raw(suffix).fg(c.dim),
        ]));
        if !open {
            continue;
        }
        match state {
            Some(SectionState::Ready(v)) if v.is_empty() => lines.push(Line::raw("    (none)").fg(c.dim)),
            Some(SectionState::Ready(v)) => lines.extend(v.iter().map(|l| Line::raw(format!("    {l}")))),
            Some(SectionState::Error(LoadErr::Unsupported(m) | LoadErr::Failed(m))) => {
                lines.push(Line::raw(format!("    {m}")).fg(c.dim))
            }
            _ => {}
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::raw("Full info").bold());
    match app.details.get(&key) {
        Some(info) => lines.extend(info.lines().map(|l| Line::raw(l.to_string()))),
        None => lines.push(Line::raw("loading details...").fg(c.dim)),
    }
    let inner = block.inner(area);
    // Screen row of each header after wrapping, for click targets.
    let header_rows: Vec<(usize, Section)> = headers
        .iter()
        .map(|&(i, s)| {
            let before: Vec<Line> = lines.iter().take(i).cloned().collect();
            (
                Paragraph::new(before)
                    .wrap(Wrap { trim: false })
                    .line_count(inner.width),
                s,
            )
        })
        .collect();
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = para.line_count(inner.width);
    let viewport = usize::from(inner.height);
    let max = len16(total.saturating_sub(viewport));
    app.detail_scroll = app.detail_scroll.min(max);
    app.detail_max = max;
    let scroll = usize::from(app.detail_scroll);
    for (row, s) in header_rows {
        if let Some(visible) = row.checked_sub(scroll).filter(|r| *r < viewport) {
            let y = inner.y.saturating_add(len16(visible));
            app.hits.sections.push((Rect::new(inner.x, y, inner.width, 1), s));
        }
    }
    f.render_widget(para.block(block).scroll((app.detail_scroll, 0)), area);
    app.hits.details_bar = scrollbar(f, area, total, viewport, scroll, app.colors);
}

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(v);
    h
}

/// A scrollable text popup with a border and an optional bottom row; the
/// height follows the *wrapped* text, so nothing is clipped unseen.
/// Returns (popup area, bottom row area).
fn scroll_popup(f: &mut Frame, app: &mut App, title: &str, lines: Vec<Line<'static>>, bottom: bool) -> (Rect, Rect) {
    let c = app.colors;
    let screen = f.area();
    let max_w = screen.width.saturating_sub(4).max(1);
    let content_w = lines
        .iter()
        .map(|l| len16(l.width()))
        .max()
        .unwrap_or(40)
        .max(40)
        .min(max_w);
    let outer_w = content_w.saturating_add(4);
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let text_h = len16(para.line_count(content_w));
    let extra = if bottom { 2 } else { 0 }; // blank line + button row
    let want_h = text_h.saturating_add(extra).saturating_add(2);
    let area = popup(screen, outer_w, want_h.min(screen.height));
    clear(f, area, c);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.to_string())
        .border_style(Style::new().fg(c.accent))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [text_area, _, row] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if bottom { 1 } else { 0 }),
        Constraint::Length(if bottom { 1 } else { 0 }),
    ])
    .areas(inner);
    let visible = text_area.height;
    app.popup_max = text_h.saturating_sub(visible);
    app.popup_scroll = app.popup_scroll.min(app.popup_max);
    f.render_widget(para.scroll((app.popup_scroll, 0)), text_area);
    scrollbar(
        f,
        area,
        usize::from(text_h),
        usize::from(visible),
        usize::from(app.popup_scroll),
        c,
    );
    (area, row)
}

fn draw_confirm(f: &mut Frame, app: &mut App, p: &crate::app::Pending) {
    let c = app.colors;
    let mut lines: Vec<Line<'static>> = vec![Line::raw(p.title.clone()).bold(), Line::raw("")];
    for n in &p.notes {
        lines.push(Line::raw(n.clone()).fg(c.marked));
    }
    if !p.notes.is_empty() {
        lines.push(Line::raw(""));
    }
    for cmd in &p.cmds {
        lines.push(Line::raw(format!("$ {}", cmd.display(app.sudo.as_deref()))).fg(c.accent));
    }
    lines.push(Line::raw(""));
    let how = match (&p.action, app.run_mode) {
        (crate::app::PendingAction::WriteExport { .. }, _) => "The file is replaced atomically.",
        (_, crate::config::RunMode::Embedded) => "Output appears in a terminal pane inside pakdeck.",
        (_, crate::config::RunMode::Suspend) => "The TUI will pause while the command runs.",
    };
    lines.push(if app.dry_run {
        Line::raw("DRY RUN: commands will only be logged.").fg(c.marked)
    } else {
        Line::raw(how).fg(c.dim)
    });
    let (area, buttons) = scroll_popup(f, app, " Confirm (j/k scroll) ", lines, true);
    let (yes, no) = (" [ y/Enter: run ] ", " [ n/Esc: cancel ] ");
    let [yes_area, _, no_area] = Layout::horizontal([
        Constraint::Length(width_of(yes)),
        Constraint::Length(2),
        Constraint::Length(width_of(no)),
    ])
    .areas(buttons);
    f.render_widget(Span::raw(yes).fg(c.on_accent).bg(c.accent).bold(), yes_area);
    f.render_widget(Span::raw(no).fg(c.on_accent).bg(c.muted), no_area);
    app.hits.popup = area;
    app.hits.confirm_yes = yes_area;
    app.hits.confirm_no = no_area;
}

/// `text` with the characters `pattern` fuzzy-matched shown in `color`.
fn highlighted(text: &str, positions: &[usize], color: Color) -> Vec<Span<'static>> {
    if positions.is_empty() {
        return vec![Span::raw(text.to_string()).bold()];
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (i, ch) in text.chars().enumerate() {
        let hit = positions.contains(&i);
        if hit != run_hit && !run.is_empty() {
            spans.push(styled_run(std::mem::take(&mut run), run_hit, color));
        }
        run_hit = hit;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(styled_run(run, run_hit, color));
    }
    spans
}

fn styled_run(text: String, hit: bool, color: Color) -> Span<'static> {
    if hit {
        Span::raw(text).fg(color).bold().underlined()
    } else {
        Span::raw(text).bold()
    }
}

/// A popup with a header, a scrollable list of rows (one selected) and a
/// hint in the bottom border. Rows are clickable (`hits.popup_rows`).
/// Cut `s` to `max` characters, ending in "..." when cut.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{keep}...")
}

/// A popup with header lines and a list of selectable rows: the selected
/// row is marked "> " and highlighted (dim text on it switches to the
/// normal colour so it stays readable), kept in view, with a scrollbar when
/// the rows overflow. `fixed_w` sets the content width; otherwise it fits
/// the content.
#[allow(clippy::too_many_arguments)]
fn list_popup(
    f: &mut Frame,
    app: &mut App,
    title: &str,
    header: Vec<Line<'static>>,
    rows: Vec<Line<'static>>,
    selected: usize,
    hint: &str,
    fixed_w: Option<u16>,
) {
    let c = app.colors;
    let screen = f.area();
    let fitted = header
        .iter()
        .map(|l| len16(l.width()))
        .chain(rows.iter().map(|l| len16(l.width()).saturating_add(2)))
        .max()
        .unwrap_or(40)
        .max(50);
    let content_w = fixed_w
        .unwrap_or(fitted)
        // The hint says how to close the popup: keep it whole where it fits.
        .max(len16(hint.chars().count()).saturating_sub(2))
        .min(screen.width.saturating_sub(4));
    let want_h = len16(header.len())
        .saturating_add(len16(rows.len()).max(1))
        .saturating_add(2);
    let area = popup(screen, content_w.saturating_add(4), want_h.min(screen.height));
    clear(f, area, c);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.to_string())
        .title_bottom(Line::from(hint.to_string()).right_aligned())
        .border_style(Style::new().fg(c.accent))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [head, list] = Layout::vertical([Constraint::Length(len16(header.len())), Constraint::Min(1)]).areas(inner);
    f.render_widget(Paragraph::new(header), head);
    let visible = usize::from(list.height.max(1));
    let offset = selected.saturating_sub(visible.saturating_sub(1));
    let total = rows.len();
    for (row, (i, line)) in rows.into_iter().enumerate().skip(offset).take(visible).enumerate() {
        let r = Rect::new(list.x, list.y.saturating_add(len16(row)), list.width, 1);
        let chosen = i == selected;
        let spans = std::iter::once(Span::raw(if chosen { "> " } else { "  " }))
            .chain(line.spans.into_iter().map(|mut s| {
                if chosen && s.style.fg == Some(c.dim) {
                    s.style.fg = None;
                }
                s
            }))
            .collect::<Vec<_>>();
        let style = if chosen { selected_style(c) } else { Style::new() };
        f.render_widget(Paragraph::new(Line::from(spans)).style(style), r);
        app.hits.popup_rows.push((r, i));
    }
    // Scrollbar beside the rows when they do not all fit.
    let track = Rect::new(
        area.x,
        list.y.saturating_sub(1),
        area.width,
        list.height.saturating_add(2),
    );
    scrollbar(f, track, total, visible, offset, c);
    app.hits.popup = area;
}

fn draw_repos(f: &mut Frame, app: &mut App, v: &crate::app::RepoView) {
    let c = app.colors;
    let mut header = match &v.adding {
        Some(text) => vec![Line::from(vec![
            Span::raw(match v.add_target() {
                "" => "Add: ".to_string(),
                target => format!("Add to {target}: "),
            })
            .bold(),
            Span::raw(format!("{text}_")).fg(c.accent),
        ])],
        None => vec![Line::raw(format!("{} {}", v.repos.len(), v.kind.to_lowercase())).fg(c.dim)],
    };
    match (&v.error, &v.adding) {
        (Some(e), _) => header.push(Line::raw(e.clone()).fg(c.error)),
        (None, Some(_)) => {
            header.push(Line::raw(v.add_hint).fg(c.dim));
        }
        (None, None) => {}
    }
    header.push(Line::raw(""));
    // Narrow screens: a shorter name column keeps the counts and the
    // official tag visible (the remote URL is cut first).
    let cap = if f.area().width < 100 { 28 } else { 40 };
    let name_w = v
        .repos
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(cap);
    let rows: Vec<Line<'static>> = v
        .repos
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let installed = v.installed.get(i).copied().unwrap_or(0);
            Line::from(vec![
                Span::raw(format!("{:<name_w$}", ellipsize(&r.name, name_w))).bold(),
                Span::raw(if r.official { "  official" } else { "          " }).fg(c.installed),
                Span::raw(format!("  {installed:>4} installed")),
                Span::raw(format!("  {:>6} available", r.packages)).fg(c.dim),
                Span::raw(if r.remote.is_empty() {
                    String::new()
                } else {
                    format!("  {}", r.remote)
                })
                .fg(c.dim),
            ])
        })
        .collect();
    let k = app.cfg.keys;
    let hint = if v.adding.is_some() {
        " Enter: add, Esc: cancel ".to_string()
    } else {
        format!(
            " {}: add, {}: remove, {}/Enter: show installed, Esc: close ",
            key_name(k.add),
            key_name(k.delete),
            key_name(k.source)
        )
    };
    list_popup(f, app, &format!(" {} ", v.kind), header, rows, v.selected, &hint, None);
}

fn draw_tree(f: &mut Frame, app: &mut App, t: &crate::app::DepTree) {
    let c = app.colors;
    let depends = t.direction == crate::backend::Section::Depends;
    let what = if depends {
        format!("Dependencies of {}", t.root.name)
    } else {
        format!("Packages that need {}", t.root.name)
    };
    let mut header = vec![Line::raw(what).bold()];
    let root = t.nodes.first();
    if root.is_some_and(|n| n.error.is_none() && n.children.as_ref().is_some_and(Vec::is_empty)) {
        header.push(
            Line::raw(if depends {
                "It has no dependencies."
            } else {
                "No installed package needs it."
            })
            .fg(c.dim),
        );
    }
    header.push(Line::raw(""));
    let rows: Vec<Line<'static>> = t
        .visible()
        .into_iter()
        .filter_map(|i| t.nodes.get(i))
        .map(|n| {
            let marker = if n.cycle {
                "   "
            } else if n.loading {
                "[.]"
            } else if n.children.as_ref().is_some_and(|k| k.is_empty()) {
                "[ ]"
            } else if n.expanded {
                "[-]"
            } else {
                "[+]"
            };
            let mut spans = vec![
                Span::raw("    ".repeat(n.depth)),
                Span::raw(format!("{marker} ")).fg(c.accent),
                Span::raw(n.name.clone()),
            ];
            if n.loading {
                spans.push(Span::raw("  loading...").fg(c.dim));
            }
            if n.cycle {
                spans.push(Span::raw("  (already above)").fg(c.dim));
            }
            if let Some(e) = &n.error {
                spans.push(Span::raw(format!("  {e}")).fg(c.error));
            }
            Line::from(spans)
        })
        .collect();
    let flip = key_name(app.cfg.keys.tree);
    let hint = if depends {
        format!(" Enter: expand, h: collapse, g: go to, {flip}: who needs it, Esc: close ")
    } else {
        format!(" Enter: expand, h: collapse, g: go to, {flip}: its dependencies, Esc: close ")
    };
    list_popup(f, app, " Dependency tree ", header, rows, t.selected_row(), &hint, None);
}

fn draw_disk(f: &mut Frame, app: &mut App, d: &crate::app::DiskView) {
    let c = app.colors;
    // A fixed width: the popup does not jump when measured sizes arrive.
    let content_w: u16 = 72u16.min(f.area().width.saturating_sub(4));
    let biggest = d.totals.iter().map(|t| t.bytes).max().unwrap_or(0).max(1);
    let mut header: Vec<Line<'static>> = vec![Line::raw("Installed size").bold()];
    for t in &d.totals {
        let measuring = d.measuring.contains(&t.source);
        // Not measured yet because the list is reloading: unknown, not 0 B.
        let unknown = t.sized == 0 && d.postponed.contains(&t.source);
        let bar = usize::try_from(t.bytes.saturating_mul(20).checked_div(biggest).unwrap_or(0)).unwrap_or(0);
        let size = if measuring {
            "measuring...".to_string()
        } else if unknown {
            "-".to_string()
        } else {
            crate::backend::human_size(t.bytes)
        };
        let note = if t.incomplete() && !measuring {
            format!("  {} of {} sized", t.sized, t.packages)
        } else {
            format!("  {} packages", t.packages)
        };
        header.push(Line::from(vec![
            Span::raw(format!("{:<8} ", ellipsize(&t.source, 8))),
            Span::raw(format!("{size:>12} ")).bold(),
            Span::raw(format!("{:<20}", "#".repeat(if measuring { 0 } else { bar }))).fg(c.accent),
            Span::raw(note).fg(c.dim),
        ]));
    }
    let disk_key = key_name(app.cfg.keys.disk);
    if !d.postponed.is_empty() {
        header.push(
            Line::raw(format!(
                "The list is reloading: press {disk_key} again to measure the missing sizes."
            ))
            .fg(c.dim),
        );
    }
    header.push(Line::raw(""));
    header.push(
        Line::raw(
            match (d.largest.is_empty(), d.measuring.is_empty(), d.postponed.is_empty()) {
                (false, _, _) => "Largest packages".to_string(),
                (true, false, _) => "Largest packages: waiting for sizes...".to_string(),
                (true, true, false) => format!("Largest packages: press {disk_key} again after the reload"),
                (true, true, true) => "No package sizes are available for these package managers.".to_string(),
            },
        )
        .bold(),
    );
    let source_w = d
        .largest
        .iter()
        .map(|p| p.source.chars().count())
        .max()
        .unwrap_or(4)
        .min(10);
    // "NN. " + name + " " + size(10) + "  " + source, inside the marker.
    let name_w = usize::from(content_w)
        .saturating_sub(4 + 1 + 10 + 2 + 2)
        .saturating_sub(source_w);
    let rows: Vec<Line<'static>> = d
        .largest
        .iter()
        .enumerate()
        .map(|(i, p)| {
            Line::from(vec![
                Span::raw(format!("{:>2}. ", i.saturating_add(1))),
                Span::raw(format!("{:<name_w$} ", ellipsize(&p.name, name_w))).bold(),
                Span::raw(format!(
                    "{:>10}",
                    p.size.map(crate::backend::human_size).unwrap_or_default()
                )),
                Span::raw(format!("  {}", ellipsize(&p.source, source_w))).fg(c.dim),
            ])
        })
        .collect();
    list_popup(
        f,
        app,
        " Disk usage ",
        header,
        rows,
        d.selected,
        " Enter/g: go to package, Esc: close ",
        Some(content_w),
    );
}

/// `text` broken into lines of at most `width` characters, at spaces.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty()
            && line
                .chars()
                .count()
                .saturating_add(1)
                .saturating_add(word.chars().count())
                > width
        {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn draw_history(f: &mut Frame, app: &mut App, v: &crate::app::HistoryView) {
    let c = app.colors;
    let mut header = vec![Line::raw("What pakdeck ran, newest first (times in UTC).").fg(c.dim)];
    if !app.cfg.history {
        header.push(Line::raw("Recording is off (history = false); older entries are shown.").fg(c.dim));
    }
    // The reason an undo is refused: wrapped, the popup does not wrap.
    if let Some(e) = &v.error {
        let width = usize::from(f.area().width.saturating_sub(10)).clamp(20, 76);
        header.extend(wrap_words(e, width).into_iter().map(|l| Line::raw(l).fg(c.error)));
    }
    header.push(Line::raw(""));
    let undone_or_not = |i: usize| v.why_not.get(i).cloned().flatten();
    let rows: Vec<Line<'static>> = v
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            // Status before the title: a long title cannot push it away.
            let (status, color) = match undone_or_not(i) {
                _ if !e.ok => ("failed ", c.error),
                Some(w) if w.contains("undone already") => ("undone ", c.dim),
                Some(_) => ("no undo", c.dim),
                None => ("       ", c.dim),
            };
            Line::from(vec![
                Span::raw(format!("{}  ", crate::util::date_time(e.time))).fg(c.dim),
                Span::raw(format!("{status}  ")).fg(color),
                Span::raw(e.title.clone()),
            ])
        })
        .collect();
    list_popup(
        f,
        app,
        " History ",
        header,
        rows,
        v.selected,
        " Enter: undo (asks first), Esc: close ",
        None,
    );
}

fn draw_themes(f: &mut Frame, app: &mut App, t: &crate::app::ThemePick) {
    let c = app.colors;
    let mut header = vec![Line::raw("Moving the selection previews a theme on the whole screen.").fg(c.dim)];
    if app.cfg.theme.has_overrides() {
        header.push(Line::raw("Your [theme] colour overrides apply on top of each theme.").fg(c.dim));
    }
    header.push(Line::raw(""));
    let label = |name: &str| -> String {
        if name == "auto" && app.auto_known {
            format!("auto ({})", app.auto_theme)
        } else if name == "auto" {
            "auto (next start)".to_string()
        } else {
            name.to_string()
        }
    };
    let name_w = t
        .themes
        .iter()
        .map(|(n, _)| label(n).chars().count())
        .max()
        .unwrap_or(10)
        .min(26);
    let rows: Vec<Line<'static>> = t
        .themes
        .iter()
        .map(|(name, kind)| {
            let mut spans = vec![
                Span::raw(format!("{:<name_w$}", ellipsize(&label(name), name_w))),
                Span::raw(format!("  {:<8} ", kind.label())).fg(c.dim),
            ];
            // A swatch of the theme's own colours: accent, installed,
            // update, marked, error. The gaps between blocks stay
            // uncoloured, so they never cut into the selection bar.
            match app.theme_colors(name) {
                Some(tc) if tc.accent == ratatui::style::Color::Reset => {
                    spans.push(Span::raw("(no colours)").fg(c.dim));
                }
                Some(tc) => {
                    for col in [tc.accent, tc.installed, tc.update, tc.marked, tc.error] {
                        spans.push(Span::raw("  ").bg(col));
                        spans.push(Span::raw(" "));
                    }
                }
                None => {}
            }
            if *name == t.original {
                spans.push(Span::raw(" (in use)").fg(c.dim));
            }
            Line::from(spans)
        })
        .collect();
    list_popup(
        f,
        app,
        " Themes ",
        header,
        rows,
        t.selected,
        " Enter: use and save, Esc: back to the old one ",
        None,
    );
}

fn draw_versions(f: &mut Frame, app: &mut App, pick: &crate::app::VersionPick) {
    let c = app.colors;
    let screen = f.area();
    let rows = len16(pick.versions.len());
    let width = pick
        .versions
        .iter()
        .map(|v| width(&v.label()))
        .max()
        .unwrap_or(10)
        .saturating_add(18)
        .max(48)
        .min(screen.width.saturating_sub(4));
    // Title row, versions, blank, hold toggle, buttons hint, borders.
    let area = popup(screen, width, rows.saturating_add(6).min(screen.height));
    clear(f, area, c);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" Versions of {} ", pick.pkg.name))
        .title_bottom(
            Line::from(if pick.can_hold {
                " Enter: install, Space: hold, Esc: cancel "
            } else {
                " Enter: install, Esc: cancel "
            })
            .right_aligned(),
        )
        .border_style(Style::new().fg(c.accent))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [list_area, _, toggle] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    let visible = usize::from(list_area.height.max(1));
    // Keep the selection in view.
    let offset = pick.selected.saturating_sub(visible.saturating_sub(1));
    for (row, (i, v)) in pick.versions.iter().enumerate().skip(offset).take(visible).enumerate() {
        let y = list_area.y.saturating_add(len16(row));
        let r = Rect::new(list_area.x, y, list_area.width, 1);
        let installed = pick.pkg.installed && v.version == pick.pkg.version;
        let mut text = format!("{} {}", if i == pick.selected { ">" } else { " " }, v.label());
        if installed {
            text.push_str("  (installed)");
        }
        let style = if i == pick.selected {
            selected_style(c)
        } else if installed {
            Style::new().fg(c.installed)
        } else {
            Style::new()
        };
        f.render_widget(Span::styled(text, style), r);
        app.hits.popup_rows.push((r, i));
    }
    scrollbar(f, area, pick.versions.len(), visible, offset, c);
    let label = if !pick.can_hold {
        "    hold: not available for this package manager".to_string()
    } else {
        let mark = if pick.hold { "[x]" } else { "[ ]" };
        let release = if pick.held_now {
            " (held now: released first)"
        } else {
            ""
        };
        format!("{mark} hold at this version afterwards{release}")
    };
    let style = if pick.can_hold {
        Style::new().fg(c.marked)
    } else {
        Style::new().fg(c.dim)
    };
    f.render_widget(Span::styled(label.clone(), style), toggle);
    app.hits.hold_toggle = Rect::new(toggle.x, toggle.y, width_of(&label), 1).intersection(toggle);
    app.hits.popup = area;
}

fn width_of(s: &str) -> u16 {
    width(s)
}

/// A terminal cell colour; the program's "default" is the theme's.
fn vt_color(c: vt100::Color, default: Color) -> Color {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The embedded terminal pane, drawn over the main UI.
fn draw_job(f: &mut Frame, app: &mut App) {
    let c = app.colors;
    let screen_area = f.area();
    let area = screen_area.inner(Margin::new(2, 1));
    let Some(job) = app.job.as_mut() else { return };
    let (title, border, hint) = match job.state {
        JobState::Running => (
            format!(
                " Running: {} ({}/{}) ",
                job.title,
                job.index.saturating_add(1),
                job.total()
            ),
            c.accent,
            " keys go to the command | Shift+PgUp/PgDn, wheel: scroll | Ctrl-\\: stop ",
        ),
        JobState::Finished { ok: true } => (
            format!(" Finished: {} ", job.title),
            c.installed,
            " PgUp/PgDn, wheel: scroll | Enter/Esc: close ",
        ),
        JobState::Finished { ok: false } => (
            format!(" Failed: {} ", job.title),
            c.error,
            " PgUp/PgDn, wheel: scroll | Enter/Esc: close ",
        ),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .title_bottom(Line::from(hint).right_aligned())
        .border_style(Style::new().fg(border));
    let inner = block.inner(area);
    clear(f, area, c);
    f.render_widget(block, area);
    job.resize(inner.height, inner.width);

    let parser = job.screen();
    let screen = parser.screen();
    let buf = f.buffer_mut();
    for row in 0..inner.height {
        for col in 0..inner.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let pos = Position::new(inner.x.saturating_add(col), inner.y.saturating_add(row));
            let Some(out) = buf.cell_mut(pos) else { continue };
            let text = cell.contents();
            out.set_symbol(if text.is_empty() { " " } else { text });
            let mut style = Style::new()
                .fg(vt_color(cell.fgcolor(), c.fg))
                .bg(vt_color(cell.bgcolor(), c.bg));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            out.set_style(style);
        }
    }
    let scrolled = screen.scrollback();
    let show_cursor = job.state == JobState::Running && scrolled == 0 && !screen.hide_cursor();
    let (crow, ccol) = screen.cursor_position();
    drop(parser);
    if scrolled > 0 {
        let note = format!(" scrolled back {scrolled} lines ");
        let w = width(&note);
        let x = area.right().saturating_sub(w).saturating_sub(2);
        let at = Rect::new(x, area.y, w, 1).intersection(area);
        f.render_widget(
            Span::raw(note)
                .fg(crate::theme::text_on(c.marked, c.on_accent))
                .bg(c.marked),
            at,
        );
    }
    if show_cursor && crow < inner.height && ccol < inner.width {
        f.set_cursor_position((inner.x.saturating_add(ccol), inner.y.saturating_add(crow)));
    }
    // Scrollbar over the history: position counts from the top.
    if let Some(job) = app.job.as_ref() {
        let depth = job.scroll_max();
        let rows = usize::from(inner.height);
        scrollbar(
            f,
            area,
            depth.saturating_add(rows),
            rows,
            depth.saturating_sub(scrolled),
            c,
        );
    }
    app.hits.job = area;
    app.hits.job_term = inner;
}

fn draw_help(f: &mut Frame, app: &mut App) {
    let k = app.cfg.keys;
    let rows: Vec<(String, &str)> = vec![
        (
            "Tab / h l / 1-4".into(),
            "switch tab (Search, Installed, Orphans, Updates)",
        ),
        ("j k / arrows".into(), "move selection"),
        ("g G / PgUp PgDn".into(), "jump"),
        ("J K".into(), "scroll details"),
        (key_name(k.search), "search (Search tab) or filter (other tabs)"),
        (key_name(k.mark), "mark / unmark for batch operations"),
        ("Esc".into(), "clear marks, then filter, then repository filter"),
        (key_name(k.install), "install marked or selected"),
        (key_name(k.delete), "remove marked or selected"),
        (key_name(k.update), "update marked or selected"),
        (key_name(k.update_all), "update everything"),
        (key_name(k.cleanup), "remove orphans and clean cache"),
        (key_name(k.sync), "sync repository metadata"),
        (key_name(k.explicit), "Installed: only packages you asked for"),
        (key_name(k.hold), "hold / release marked or selected (H marker)"),
        (key_name(k.source), "filter by package manager (several active)"),
        (key_name(k.export), "export your package list to the export file"),
        (key_name(k.import), "install what the export file lists"),
        (key_name(k.expand), "expand / collapse all detail sections"),
        (key_name(k.reload), "reload current list"),
        (key_name(k.sort), "sort: name, size, install date, source"),
        (key_name(k.versions), "pick a version to install (downgrade)"),
        (key_name(k.theme), "theme picker (live preview, Enter saves)"),
        (key_name(k.tree), "dependency tree (inside: again for who needs it)"),
        (key_name(k.disk), "disk usage per package manager, largest packages"),
        (key_name(k.repos), "repositories (Homebrew taps): add, remove, filter"),
        (
            key_name(k.history),
            "history of what pakdeck ran; Enter undoes an entry",
        ),
        (
            key_name(k.clear_log),
            "clear the Log pane (or click \"clear\" in its title)",
        ),
        ("Up/Down".into(), "in the search box: earlier searches"),
        (key_name(k.run_mode), "run commands embedded / suspended"),
        (key_name(k.dry_run), "toggle dry-run"),
        ("Ctrl-\\".into(), "terminal pane: interrupt the command (twice: kill)"),
        (key_name(k.quit), "quit"),
    ];
    let accent = app.colors.accent;
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::raw(format!("{key:>16}  ")).fg(accent).bold(),
                Span::raw(what),
            ])
        })
        .collect();
    let (area, _) = scroll_popup(f, app, " Keys (j/k scroll, any other key closes) ", lines, false);
    app.hits.popup = area;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use std::time::Duration;

    struct Harness {
        app: App,
        term: Terminal<TestBackend>,
    }

    impl Harness {
        fn new() -> Self {
            let mut h = Self {
                app: crate::testing::mock_app(),
                term: Terminal::new(TestBackend::new(120, 40)).unwrap(),
            };
            h.settle();
            h
        }
        /// Let background loads finish, then redraw (which records hit regions).
        fn settle(&mut self) {
            for _ in 0..50 {
                self.app.tick();
                if Tab::ALL.iter().all(|t| !self.app.panes.get(*t).loading) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            self.draw();
        }
        fn draw(&mut self) {
            let app = &mut self.app;
            self.term.draw(|f| draw(f, app)).unwrap();
        }
        fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
            self.app.on_mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
            self.draw();
        }
        fn click(&mut self, r: Rect) {
            self.mouse(MouseEventKind::Down(MouseButton::Left), r.x + 1, r.y);
        }
        fn screen(&self) -> String {
            let buf = self.term.backend().buffer();
            (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
                .collect()
        }
    }

    #[test]
    fn click_tabs() {
        let mut h = Harness::new();
        let installed = h.app.hits.tabs[1].0;
        h.click(installed);
        assert_eq!(h.app.tab, Tab::Installed);
        let updates = h.app.hits.tabs[3].0;
        h.click(updates);
        assert_eq!(h.app.tab, Tab::Updates);
    }

    #[test]
    fn click_rows_marks_and_wheel() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        let list = h.app.hits.list;
        // Click the name of the 4th row: selects, does not mark.
        h.mouse(MouseEventKind::Down(MouseButton::Left), list.x + 12, list.y + 3);
        assert_eq!(h.app.pane().state.selected(), Some(3));
        assert!(h.app.pane().marked.is_empty());
        // Click the checkbox of row 6: selects and marks.
        h.mouse(MouseEventKind::Down(MouseButton::Left), list.x + 3, list.y + 5);
        assert_eq!(h.app.selected().unwrap().name, "pkg05");
        assert!(h.app.pane().marked.contains(&h.app.selected().unwrap().key()));
        // Right-click marks another row.
        h.mouse(MouseEventKind::Down(MouseButton::Right), list.x + 12, list.y + 7);
        assert_eq!(h.app.pane().marked.len(), 2);
        assert!(h.screen().contains("[2 marked]"));
        // Clicking below the last item does nothing.
        h.click(h.app.hits.tabs[3].0);
        let before = h.app.pane().state.selected();
        h.mouse(MouseEventKind::Down(MouseButton::Left), list.x + 12, list.y + 20);
        assert_eq!(h.app.pane().state.selected(), before);
        // Wheel over the list moves the selection, and the list scrolls.
        h.click(h.app.hits.tabs[1].0);
        for _ in 0..15 {
            h.mouse(MouseEventKind::ScrollDown, list.x + 5, list.y + 2);
        }
        assert_eq!(h.app.selected().unwrap().name, "pkg49");
        assert!(h.app.pane().state.offset() > 0);
        // After scrolling, clicking the top row maps through the offset.
        let offset = h.app.pane().state.offset();
        h.mouse(MouseEventKind::Down(MouseButton::Left), list.x + 12, list.y);
        assert_eq!(h.app.pane().state.selected(), Some(offset));
        // Wheel over details scrolls details, not the list.
        let details = h.app.hits.details;
        let key = h.app.selected().unwrap().key();
        h.app.details.insert(key, "x\n".repeat(100));
        h.mouse(MouseEventKind::ScrollDown, details.x + 5, details.y + 5);
        assert_eq!(h.app.detail_scroll, 3);
        assert_eq!(h.app.pane().state.selected(), Some(offset));
    }

    #[test]
    fn click_search_box_footer_and_confirm() {
        let mut h = Harness::new();
        h.click(h.app.hits.input);
        assert!(matches!(h.app.mode, Mode::Input));
        for c in "tree".chars() {
            h.app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        h.app.on_key(KeyEvent::from(KeyCode::Enter));
        h.settle();
        assert_eq!(h.app.pane().items.len(), 30);

        // Footer "install" button opens the confirm popup.
        let install = h.app.hits.footer.iter().find(|(_, a)| *a == Action::Install).unwrap().0;
        h.click(install);
        assert!(matches!(h.app.mode, Mode::Confirm(_)));
        assert!(h.screen().contains("y/Enter: run"));
        // Cancel button.
        h.click(h.app.hits.confirm_no);
        assert!(matches!(h.app.mode, Mode::Normal));
        assert_eq!(h.app.log.back().unwrap(), "cancelled");
        // Run button (dry-run): command is logged.
        h.click(install);
        h.click(h.app.hits.confirm_yes);
        assert!(matches!(h.app.mode, Mode::Normal));
        assert_eq!(h.app.log.back().unwrap(), "[dry-run] $ mock install tree0");
        // Clicking outside the popup cancels.
        h.click(install);
        h.mouse(MouseEventKind::Down(MouseButton::Left), 0, 39);
        assert!(matches!(h.app.mode, Mode::Normal));

        // Help button, then any click closes it.
        let help = h.app.hits.footer.iter().find(|(_, a)| *a == Action::Help).unwrap().0;
        h.click(help);
        assert!(matches!(h.app.mode, Mode::Help));
        h.mouse(MouseEventKind::Down(MouseButton::Left), 50, 20);
        assert!(matches!(h.app.mode, Mode::Normal));
    }

    #[test]
    fn click_outside_input_stops_typing() {
        let mut h = Harness::new();
        h.click(h.app.hits.input);
        assert!(matches!(h.app.mode, Mode::Input));
        h.mouse(
            MouseEventKind::Down(MouseButton::Left),
            h.app.hits.details.x + 3,
            h.app.hits.details.y + 3,
        );
        assert!(matches!(h.app.mode, Mode::Normal));
    }

    /// Characters drawn in a one-column rect, top to bottom.
    fn column(h: &Harness, r: Rect) -> String {
        let buf = h.term.backend().buffer();
        (r.y..r.bottom()).map(|y| buf[(r.x, y)].symbol().to_string()).collect()
    }

    #[test]
    fn scrollbars_only_when_overflowing() {
        let mut h = Harness::new();
        // Updates has 1 item: no list scrollbar. Installed has 50: scrollbar.
        h.click(h.app.hits.tabs[3].0);
        assert_eq!(h.app.hits.list_bar, Rect::default());
        h.click(h.app.hits.tabs[1].0);
        let bar = h.app.hits.list_bar;
        assert!(bar.height > 0);
        let col = column(&h, bar);
        assert!(col.starts_with('\u{2588}'), "thumb at top: {col}");
        assert!(col.ends_with('\u{2502}'));
        // Short details and log do not get a bar.
        assert_eq!(h.app.hits.details_bar, Rect::default());
        assert_eq!(h.app.hits.log_bar, Rect::default());

        // Jump to the end: thumb touches the bottom.
        h.app.on_key(KeyEvent::from(KeyCode::Char('G')));
        h.draw();
        let col = column(&h, h.app.hits.list_bar);
        assert!(
            col.ends_with('\u{2588}') && col.starts_with('\u{2502}'),
            "thumb at bottom: {col}"
        );
    }

    #[test]
    fn drag_list_scrollbar() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        let bar = h.app.hits.list_bar;
        h.mouse(MouseEventKind::Down(MouseButton::Left), bar.x, bar.bottom() - 1);
        assert_eq!(h.app.selected().unwrap().name, "pkg49");
        // Dragging keeps tracking even when the pointer leaves the column.
        h.mouse(MouseEventKind::Drag(MouseButton::Left), bar.x - 20, bar.y);
        assert_eq!(h.app.selected().unwrap().name, "pkg00");
        h.mouse(MouseEventKind::Up(MouseButton::Left), bar.x - 20, bar.y);
        h.mouse(MouseEventKind::Drag(MouseButton::Left), bar.x, bar.bottom() - 1);
        assert_eq!(h.app.selected().unwrap().name, "pkg00", "no drag after release");
    }

    #[test]
    fn details_and_log_scrollbars() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        let key = h.app.selected().unwrap().key();
        h.app
            .details
            .insert(key, (0..100).map(|i| format!("line {i}\n")).collect());
        h.draw();
        let bar = h.app.hits.details_bar;
        assert!(bar.height > 0);
        // Scrolling is clamped so the last line stays at the bottom.
        for _ in 0..100 {
            h.app.on_key(KeyEvent::from(KeyCode::Char('J')));
        }
        h.draw();
        assert_eq!(h.app.detail_scroll, h.app.detail_max);
        assert!(h.screen().contains("line 99"));
        // Click the top of the track: back to the start.
        h.mouse(MouseEventKind::Down(MouseButton::Left), bar.x, bar.y);
        assert_eq!(h.app.detail_scroll, 0);

        for i in 0..30 {
            h.app.log(format!("entry {i}"));
        }
        h.draw();
        assert!(h.app.hits.log_bar.height > 0);
        let log = h.app.hits.log;
        h.mouse(MouseEventKind::ScrollUp, log.x + 5, log.y + 2);
        h.mouse(MouseEventKind::ScrollUp, log.x + 5, log.y + 2);
        assert_eq!(h.app.log_scroll, 2);
        assert!(h.screen().contains("Log (+2 newer)"));
        assert!(!h.screen().contains("entry 29"));
        // New entries do not move the view while scrolled up.
        h.app.log("entry 30");
        h.draw();
        assert_eq!(h.app.log_scroll, 3);
        h.mouse(
            MouseEventKind::Down(MouseButton::Left),
            h.app.hits.log_bar.x,
            h.app.hits.log_bar.bottom() - 1,
        );
        assert_eq!(h.app.log_scroll, 0);
        assert!(h.screen().contains("entry 30"));
    }

    /// Regression (found by the `ui` fuzz target): opening the confirm popup
    /// on a terminal narrower than 44 columns panicked in `clamp`.
    #[test]
    fn popups_on_tiny_terminals() {
        for (w, h) in [(1, 1), (5, 3), (37, 47), (43, 10), (44, 10), (120, 2)] {
            let mut app = crate::testing::mock_app();
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            for key in ['c', 'q', '?'] {
                app.tick();
                term.draw(|f| draw(f, &mut app)).unwrap();
                app.on_key(KeyEvent::from(KeyCode::Char(key)));
                term.draw(|f| draw(f, &mut app)).unwrap();
            }
        }
    }

    #[test]
    fn section_headers_toggle_and_lazy_files() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        h.settle();
        let screen = h.screen();
        assert!(screen.contains("[-] Depends on (2)"), "{screen}");
        assert!(screen.contains("libmock"));
        assert!(screen.contains("[+] Files (click to load)"));
        assert!(screen.contains("Size      1.0 MiB"));
        // Click the Files header: expands, loads, lists files.
        let files = h
            .app
            .hits
            .sections
            .iter()
            .find(|(_, s)| *s == Section::Files)
            .map(|(r, _)| *r)
            .unwrap();
        h.click(files);
        h.settle();
        h.settle();
        assert!(h.screen().contains("[-] Files (40)"), "{}", h.screen());
        // Collapse Depends by clicking its header.
        let deps = h
            .app
            .hits
            .sections
            .iter()
            .find(|(_, s)| *s == Section::Depends)
            .map(|(r, _)| *r)
            .unwrap();
        h.click(deps);
        assert!(h.screen().contains("[+] Depends on (2)"));
        assert!(!h.screen().contains("    libmock"));
    }

    #[test]
    fn orphans_tab_and_unsupported_message() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[2].0);
        assert_eq!(h.app.tab, Tab::Orphans);
        assert!(h.screen().contains("Orphans (2)"));
        assert!(h.screen().contains("pkg48"));

        let mut app = crate::testing::mock_app_with(crate::testing::Mock {
            unsupported: true,
            ..Default::default()
        });
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        app.tick();
        app.on_key(KeyEvent::from(KeyCode::Char('3')));
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert!(text.contains("Not available: mock: no orphans"), "{text}");
        assert!(text.contains("Orphans (-)"));
    }

    #[test]
    fn explicit_toggle_in_tab_title() {
        let mut h = Harness::new();
        h.app.on_key(KeyEvent::from(KeyCode::Char('e')));
        h.settle();
        assert!(h.screen().contains("Installed (25/50)"), "{}", h.screen());
        assert!(h.screen().contains("(explicit)"));
    }

    #[cfg(unix)]
    #[test]
    fn embedded_terminal_pane_runs_and_closes() {
        let mut h = Harness::new();
        h.app.sandbox = false;
        h.draw();
        let cmds = vec![crate::backend::Cmd::user(
            "sh",
            &[
                "-c",
                "printf '\\033[32mgreen\\033[0m '; printf 'Proceed? [Y/n] '; read a; echo got-$a",
            ],
        )];
        h.app.start_job("demo".into(), cmds);
        let wait = |h: &mut Harness, what: &str| {
            for _ in 0..300 {
                h.app.tick();
                h.draw();
                if h.screen().contains(what) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting for {what:?}\n{}", h.screen());
        };
        // The echoed command line also contains the prompt text; this
        // sequence only appears in the program's real output.
        wait(&mut h, "green Proceed? [Y/n]");
        assert!(h.screen().contains("Running: demo (1/1)"));
        // Colours from the program reach the buffer.
        let buf = h.term.backend().buffer();
        let green = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .any(|p| buf[p].symbol() == "g" && buf[p].fg == Color::Indexed(2));
        let fgs: Vec<String> = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|p| buf[*p].symbol() == "g")
            .map(|p| format!("{:?}@{:?}", buf[p].fg, p))
            .collect();
        assert!(green, "green text not rendered in colour: {fgs:?}");
        // Keys go to the program while it runs (q does not quit pakdeck).
        for c in ['q', '\r'] {
            let code = if c == '\r' { KeyCode::Enter } else { KeyCode::Char(c) };
            h.app.on_key(KeyEvent::from(code));
        }
        wait(&mut h, "got-q");
        wait(&mut h, "Finished: demo");
        assert!(!h.app.quit);
        // Enter closes the pane and refreshes the lists.
        h.app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(h.app.job.is_none());
        assert_eq!(h.app.log.back().map(String::as_str), Some("done"));
    }

    #[test]
    fn eighty_columns_all_tabs_clickable() {
        let mut app = crate::testing::mock_app_multi(vec![
            crate::testing::Mock::named("one"),
            crate::testing::Mock::named("two"),
        ]);
        app.tick();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(app.hits.tabs.len(), 4);
        for (r, t) in app.hits.tabs.clone() {
            assert!(r.width > 0, "tab {t:?} has no room at 80 columns");
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: r.x,
                row: r.y,
                modifiers: KeyModifiers::NONE,
            });
            assert_eq!(app.tab, t);
        }
        // The status labels are clickable too: source filter and run mode.
        let src = app.hits.status_source;
        assert!(src.width > 0);
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: src.x,
            row: src.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.source_filter.as_deref(), Some("one"));
    }

    #[test]
    fn help_scrolls_on_small_terminals() {
        let mut app = crate::testing::mock_app();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(app.popup_max > 0, "help should overflow 24 rows");
        for _ in 0..40 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        assert!(matches!(app.mode, Mode::Help), "j must scroll, not close");
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert!(text.contains("quit"), "last help row not reachable");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn long_confirm_scrolls_instead_of_clipping() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        // Mark everything: one very long install line.
        for _ in 0..50 {
            h.app.on_key(KeyEvent::from(KeyCode::Char(' ')));
        }
        h.app.on_key(KeyEvent::from(KeyCode::Char('d')));
        h.draw();
        assert!(matches!(h.app.mode, Mode::Confirm(_)));
        let max = h.app.popup_max;
        let screen = h.screen();
        assert!(
            screen.contains("DRY RUN") || max > 0,
            "footer line clipped without scrolling"
        );
        for _ in 0..100 {
            h.app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        h.draw();
        assert!(h.screen().contains("DRY RUN"), "{}", h.screen());
        assert!(h.screen().contains("pkg49"));
    }

    #[test]
    fn dates() {
        assert_eq!(crate::util::date(0), "1970-01-01");
        assert_eq!(crate::util::date(1_730_851_200), "2024-11-06");
        assert_eq!(crate::util::date(951_782_400), "2000-02-29");
        assert_eq!(crate::util::date(u64::MAX), "?");
    }

    #[test]
    fn match_highlighting() {
        let pos = crate::fuzzy::fuzzy_match("rg", "ripgrep")
            .map(|m| m.positions)
            .unwrap_or_default();
        let spans = highlighted("ripgrep", &pos, Color::Cyan);
        let text: Vec<String> = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, ["r", "ip", "g", "rep"]);
        assert_eq!(highlighted("tree", &[], Color::Cyan).len(), 1);
    }

    #[test]
    fn version_picker_renders_and_clicks() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        h.app.on_key(KeyEvent::from(KeyCode::Char('v')));
        h.settle();
        assert!(h.screen().contains("Versions of pkg00"), "{}", h.screen());
        assert!(h.screen().contains("1.0  (installed)"));
        assert!(h.screen().contains("[x] hold at this version afterwards"));
        // Click the hold toggle, then click 2.0 twice (select, then install).
        let toggle = h.app.hits.hold_toggle;
        h.click(toggle);
        assert!(h.screen().contains("[ ] hold"));
        let row = h
            .app
            .hits
            .popup_rows
            .iter()
            .find(|(_, i)| *i == 0)
            .map(|(r, _)| *r)
            .unwrap();
        h.click(row);
        h.click(row);
        h.settle();
        assert!(matches!(h.app.mode, Mode::Confirm(_)), "{}", h.screen());
        assert!(h.screen().contains("Install pkg00 2.0"));
    }

    #[test]
    fn sorted_column_and_light_theme_render() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        h.app.on_key(KeyEvent::from(KeyCode::Char('o')));
        h.app.on_key(KeyEvent::from(KeyCode::Char('o')));
        h.draw();
        assert!(h.screen().contains("(by size)"), "{}", h.screen());
        assert!(h.screen().contains("48.8 KiB"));
        h.app.on_key(KeyEvent::from(KeyCode::Char('T')));
        h.draw();
        let buf = h.term.backend().buffer();
        let accent = h.app.colors.accent;
        assert!(
            (0..buf.area.width).any(|x| buf[(x, 0)].bg == accent),
            "light accent not used"
        );
    }

    #[test]
    fn filtering_a_scrolled_list_shows_the_top_matches() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        // Scroll far down a long list, then filter it.
        h.app.on_key(KeyEvent::from(KeyCode::Char('G')));
        h.draw();
        assert!(h.app.pane().state.offset() > 0);
        // Type the filter as a user would.
        h.app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "pkg1".chars() {
            h.app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        h.draw();
        // pkg10 ranks first and must be on screen, not scrolled away.
        assert!(h.screen().contains("pkg10"), "{}", h.screen());
        assert_eq!(h.app.pane().state.offset(), 0);
    }

    #[test]
    fn tree_disk_and_repos_popups_render_and_click() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[1].0);
        h.app.on_key(KeyEvent::from(KeyCode::Char('t')));
        h.settle();
        assert!(h.screen().contains("Dependencies of pkg00"), "{}", h.screen());
        assert!(h.screen().contains("libmock"));
        // Click libmock twice: select, then expand.
        let row = h
            .app
            .hits
            .popup_rows
            .iter()
            .find(|(_, i)| *i == 1)
            .map(|(r, _)| *r)
            .unwrap();
        h.click(row);
        h.click(row);
        h.settle();
        assert!(h.screen().contains("libbase"), "{}", h.screen());
        h.app.on_key(KeyEvent::from(KeyCode::Esc));

        h.app.on_key(KeyEvent::from(KeyCode::Char('z')));
        h.draw();
        assert!(h.screen().contains("Disk usage"));
        assert!(h.screen().contains("Largest packages"));
        assert!(h.screen().contains("48.8 KiB"));
        h.app.on_key(KeyEvent::from(KeyCode::Esc));

        h.app.on_key(KeyEvent::from(KeyCode::Char('R')));
        h.settle();
        assert!(h.screen().contains("mock/extra"), "{}", h.screen());
        // While typing a name, moves and clicks inside keep the text.
        h.app.on_key(KeyEvent::from(KeyCode::Char('a')));
        h.app.on_key(KeyEvent::from(KeyCode::Char('x')));
        h.draw();
        h.mouse(MouseEventKind::Moved, 0, 39);
        let row = h.app.hits.popup_rows.first().map(|(r, _)| *r).unwrap();
        h.click(row);
        assert!(matches!(&h.app.mode, Mode::Repos(v) if v.adding.as_deref() == Some("x")));
        assert!(h.screen().contains("Add to mock: x_"), "{}", h.screen());
        assert!(h.screen().contains("Enter: add, Esc: cancel"), "{}", h.screen());
        // Clicking outside closes it.
        h.mouse(MouseEventKind::Down(MouseButton::Left), 0, 39);
        assert!(matches!(h.app.mode, Mode::Normal));
        // Filtering by a repository says so in the filter box and the title.
        h.app.on_key(KeyEvent::from(KeyCode::Char('R')));
        h.settle();
        h.app.on_key(KeyEvent::from(KeyCode::Char('f')));
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("repository: mock/extra (Esc clears)"), "{screen}");
        assert!(screen.contains("Installed 25/50 [from mock/extra]"), "{screen}");
    }

    #[test]
    fn theme_picker_fits_80x24_and_previews() {
        let mut h = Harness::new();
        h.term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        h.app.on_key(KeyEvent::from(KeyCode::Char('T')));
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("Themes"), "{screen}");
        assert!(
            screen.contains("Enter: use and save, Esc: back to the old one"),
            "{screen}"
        );
        assert!(screen.contains("(in use)"), "{screen}");
        // Preview a palette with its own background: the whole screen,
        // popups included, is painted in it.
        let Mode::Themes(t) = &h.app.mode else {
            panic!("no picker")
        };
        let nord = t.themes.iter().position(|(n, _)| n == "nord").unwrap();
        for _ in t.selected..nord {
            h.app.on_key(KeyEvent::from(KeyCode::Down));
        }
        h.draw();
        let bg = crate::theme::builtin("nord").unwrap().colors.bg;
        let buf = h.term.backend().buffer().clone();
        // The filter box's corner: plain screen, not a coloured chip.
        assert_eq!(buf.cell((0, 1)).map(|c| c.bg), Some(bg), "screen");
        let popup = h.app.hits.popup;
        assert_eq!(
            buf.cell((popup.x + 2, popup.y + 1)).map(|c| c.bg),
            Some(bg),
            "inside the popup"
        );
        h.app.on_key(KeyEvent::from(KeyCode::Esc));
        h.draw();
        let buf = h.term.backend().buffer().clone();
        assert_eq!(buf.cell((0, 1)).map(|c| c.bg), Some(ratatui::style::Color::Reset));
    }

    #[test]
    fn warnings_are_whole_and_coloured_at_80_columns() {
        let mut h = Harness::new();
        h.term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        h.app.warn_no_elevation();
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("commands that need root will fail"), "{screen}");
        assert!(screen.contains("install one, or set sudo"), "{screen}");
        let buf = h.term.backend().buffer().clone();
        let marked = h.app.colors.marked;
        let row = (0..24u16)
            .find(|y| {
                (0..80u16)
                    .filter_map(|x| buf.cell((x, *y)))
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .contains("warning: no sudo")
            })
            .unwrap();
        assert!((0..80u16).filter_map(|x| buf.cell((x, row))).any(|c| c.fg == marked));
    }

    #[test]
    fn update_notes_show_in_the_list_and_details() {
        let mut h = Harness::new();
        h.click(h.app.hits.tabs[3].0);
        if let Some(p) = h.app.panes.updates.items.first_mut() {
            p.note = Some("updates itself".into());
        }
        h.settle();
        let screen = h.screen();
        assert!(screen.contains("pkg01 1.0 -> 2.0 (updates itself)"), "{screen}");
        // A cask that updates itself: one bracket, not two.
        if let Some(p) = h.app.panes.updates.items.first_mut() {
            p.repo = "cask".into();
        }
        h.draw();
        assert!(h.screen().contains("(cask, updates itself)"), "{}", h.screen());
        assert!(screen.contains("Note"), "{screen}");
    }

    #[test]
    fn updates_loading_explains_the_wait_at_80_columns() {
        let mut h = Harness::new();
        h.app = crate::testing::mock_app_with(crate::testing::Mock {
            slow_updates: true,
            ..crate::testing::Mock::named("brewish")
        });
        h.term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        h.settle();
        h.click(h.app.hits.tabs[3].0);
        h.app.panes.updates.loading = true;
        h.app.panes.updates.items.clear();
        h.app.panes.updates.view.clear();
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("Updates checking..."), "{screen}");
        assert!(screen.contains("Checking slowly."), "{screen}");
    }

    #[test]
    fn history_popup_fits_80x24() {
        let dir = std::env::temp_dir().join(format!("pakdeck-hist-ui-{}", std::process::id()));
        let path = dir.join("history.jsonl");
        let entry = crate::history::Entry {
            time: 1_763_030_700,
            id: 1,
            undoes: None,
            op: crate::history::Op::Upgrade,
            title: "Update stats".into(),
            items: Vec::new(),
            commands: vec!["brew upgrade --cask --greedy stats".into()],
            ok: false,
        };
        crate::history::append(&path, &entry).unwrap();
        let mut h = Harness::new();
        h.term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        h.app.history_path = Some(path);
        h.app.on_key(KeyEvent::from(KeyCode::Char('H')));
        h.draw();
        let screen = h.screen();
        assert!(screen.contains(" History "), "{screen}");
        assert!(screen.contains("2025-11-13 10:45  failed   Update stats"), "{screen}");
        assert!(screen.contains("Enter: undo (asks first), Esc: close"), "{screen}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_marks_and_explains_what_cannot_be_undone_at_80x24() {
        let dir = std::env::temp_dir().join(format!("pakdeck-hist-why-{}", std::process::id()));
        let path = dir.join("history.jsonl");
        let other = crate::history::Entry {
            time: 1_763_030_700,
            id: 7,
            undoes: None,
            op: crate::history::Op::Other,
            title: "Clean up unused packages and cache".into(),
            items: Vec::new(),
            commands: Vec::new(),
            ok: true,
        };
        crate::history::append(&path, &other).unwrap();
        let mut h = Harness::new();
        h.term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        h.app.history_path = Some(path);
        h.app.cfg.history = false;
        h.app.on_key(KeyEvent::from(KeyCode::Char('H')));
        h.app.on_key(KeyEvent::from(KeyCode::Enter));
        h.draw();
        let screen = h.screen();
        assert!(
            screen.contains("no undo  Clean up unused packages and cache"),
            "{screen}"
        );
        assert!(screen.contains("Recording is off"), "{screen}");
        // The whole reason is readable: no line of it is cut.
        let joined: String = screen.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            joined.contains("cannot undo: only installs, removals, updates and holds can"),
            "{screen}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_pane_clears_by_key_and_click() {
        let mut h = Harness::new();
        h.term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        h.app.log("something happened");
        h.draw();
        assert!(h.screen().contains("something happened"));
        assert!(h.screen().contains("L: clear"), "{}", h.screen());
        h.app.on_key(KeyEvent::from(KeyCode::Char('L')));
        h.draw();
        assert!(!h.screen().contains("something happened"), "{}", h.screen());
        assert!(h.app.log.is_empty());
        // The title label clears too.
        h.app.log("again");
        h.draw();
        let at = h.app.hits.log_clear;
        assert!(at.width > 0);
        h.mouse(MouseEventKind::Down(MouseButton::Left), at.x + 1, at.y);
        assert!(h.app.log.is_empty());
        // A rebound key shows in the label.
        h.app.cfg.keys.clear_log = 'Y';
        h.draw();
        assert!(h.screen().contains("Y: clear"), "{}", h.screen());
    }

    #[test]
    fn auto_entry_says_what_it_is() {
        let mut h = Harness::new();
        h.app.auto_theme = "light".into();
        h.app.on_key(KeyEvent::from(KeyCode::Char('T')));
        h.draw();
        assert!(h.screen().contains("auto (light)"), "{}", h.screen());
        h.app.on_key(KeyEvent::from(KeyCode::Esc));
        // Not asked at startup (another theme was set): decided next time.
        h.app.auto_known = false;
        h.app.on_key(KeyEvent::from(KeyCode::Char('T')));
        h.draw();
        assert!(h.screen().contains("auto (next start)"), "{}", h.screen());
    }

    #[test]
    fn monochrome_shows_the_selection_reversed() {
        let mut h = Harness::new();
        assert!(h.app.set_theme("monochrome"));
        h.click(h.app.hits.tabs[1].0);
        h.draw();
        let buf = h.term.backend().buffer().clone();
        let list = h.app.hits.list;
        let cell = buf.cell((list.x + 4, list.y)).unwrap();
        assert!(cell.modifier.contains(Modifier::REVERSED), "selected row not reversed");
        assert_eq!(
            (cell.fg, cell.bg),
            (ratatui::style::Color::Reset, ratatui::style::Color::Reset)
        );
    }

    /// The popups at 80x24: the selected row is marked, dim text on it stays
    /// readable, and the hint saying how to close is whole.
    #[test]
    fn popups_fit_80x24_and_mark_the_selection() {
        let mut h = Harness::new();
        h.term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        h.click(h.app.hits.tabs[1].0);
        h.app.on_key(KeyEvent::from(KeyCode::Char('z')));
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("Enter/g: go to package, Esc: close"), "{screen}");
        assert!(screen.contains(">  1. pkg00"), "{screen}");
        // The selected row's dim source column is drawn in the normal colour.
        let buf = h.term.backend().buffer().clone();
        let row = h.app.hits.popup_rows.first().map(|(r, _)| *r).unwrap();
        let dim = h.app.colors.dim;
        assert!(
            (row.x..row.right()).all(|x| buf.cell((x, row.y)).is_none_or(|c| c.fg != dim)),
            "dim text on the selected row"
        );
        h.app.on_key(KeyEvent::from(KeyCode::Esc));
        h.app.on_key(KeyEvent::from(KeyCode::Char('t')));
        h.settle();
        let screen = h.screen();
        assert!(screen.contains("t: who needs it, Esc: close"), "{screen}");
        // Flip to "required by": the mock has none, and the popup says so.
        h.app.on_key(KeyEvent::from(KeyCode::Char('t')));
        h.settle();
        let screen = h.screen();
        assert!(screen.contains("Packages that need pkg00"), "{screen}");
        assert!(screen.contains("No installed package needs it."), "{screen}");
        h.app.on_key(KeyEvent::from(KeyCode::Esc));
        h.app.on_key(KeyEvent::from(KeyCode::Char('R')));
        h.settle();
        let screen = h.screen();
        assert!(
            screen.contains("a: add, d: remove, f/Enter: show installed, Esc: close"),
            "{screen}"
        );
        assert!(screen.contains("25 installed"), "{screen}");
    }

    #[test]
    fn disk_during_a_reload_shows_unknown_not_zero() {
        let mut h = Harness::new();
        h.app = crate::testing::mock_app_with(crate::testing::Mock {
            sizeless: true,
            ..crate::testing::Mock::named("brewish")
        });
        h.settle();
        h.app.panes.installed.loading = true;
        h.app.on_key(KeyEvent::from(KeyCode::Char('z')));
        h.draw();
        let screen = h.screen();
        assert!(
            screen.contains("press z again to measure the missing sizes"),
            "{screen}"
        );
        assert!(
            screen.contains("Largest packages: press z again after the reload"),
            "{screen}"
        );
        assert!(!screen.contains("0 B"), "{screen}");
        assert!(!screen.contains("No package sizes are available"), "{screen}");
    }

    #[test]
    fn popup_hints_name_the_bound_keys() {
        let mut h = Harness::new();
        h.app.cfg.keys.add = 'n';
        h.app.cfg.keys.delete = 'x';
        h.app.cfg.keys.tree = 'Y';
        h.click(h.app.hits.tabs[1].0);
        h.app.on_key(KeyEvent::from(KeyCode::Char('R')));
        h.settle();
        assert!(
            h.screen().contains("n: add, x: remove, f/Enter: show installed"),
            "{}",
            h.screen()
        );
        h.app.on_key(KeyEvent::from(KeyCode::Esc));
        h.app.on_key(KeyEvent::from(KeyCode::Char('Y')));
        h.settle();
        assert!(h.screen().contains("Y: who needs it"), "{}", h.screen());
    }

    #[test]
    fn disk_shows_measuring_instead_of_zero() {
        let mut h = Harness::new();
        h.app = crate::testing::mock_app_with(crate::testing::Mock {
            sizeless: true,
            ..crate::testing::Mock::named("brewish")
        });
        h.settle();
        // Measuring has started but its result is not handled yet.
        h.app.on_key(KeyEvent::from(KeyCode::Char('z')));
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("measuring..."), "{screen}");
        assert!(screen.contains("Largest packages: waiting for sizes..."), "{screen}");
        assert!(!screen.contains("No package sizes are available"), "{screen}");
        h.settle();
        assert!(h.screen().contains("4.8 KiB"), "{}", h.screen());
    }
}
