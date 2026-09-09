use super::*;

use crate::protocol::file_transfer::FILE_BROWSER_MAX_ENTRIES;
use crate::ui::{middle_elide, take_suffix_width};

const PROMPT_WIDTH: u16 = 60;
const PROMPT_HEIGHT: u16 = 9;
const BROWSER_WIDTH: u16 = 72;
const BROWSER_HEIGHT: u16 = 18;

pub(super) fn render_file_transfer_overlay(
    b: &mut Buffer,
    overlay: &ClientFileTransferOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    match overlay {
        ClientFileTransferOverlay::SendPath(input) => render_send_prompt(b, input, p),
        ClientFileTransferOverlay::Browse(browser) => render_browser(b, browser, p),
        ClientFileTransferOverlay::Progress(run) => render_progress(b, run, p),
    }
}

/// The browser's list body, in screen coordinates. The renderer writes the row
/// count back into the browser so key paging and mouse hit-testing use the same
/// window the user is looking at.
pub(crate) fn browser_visible_rows(area: Rect) -> usize {
    let Some(popup) = popup(area, BROWSER_WIDTH, BROWSER_HEIGHT) else {
        return 0;
    };
    popup.height.saturating_sub(2).saturating_sub(4) as usize
}

fn render_send_prompt(b: &mut Buffer, input: &str, p: &Palette) -> Option<OverlayRender> {
    let popup = popup(b.area, PROMPT_WIDTH, PROMPT_HEIGHT)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        " send file",
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        inner.x,
        inner.y + 2,
        inner.width,
        " path on this computer",
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    let field = Rect::new(inner.x, inner.y + 3, inner.width, 1);
    let field_style = Style::default().fg(p.text).bg(p.surface0);
    b.set_style(field, field_style);
    // The tail, not the head: a path longer than the field would otherwise run
    // off the right edge and leave the user typing blind.
    let shown = take_suffix_width(input, field.width.saturating_sub(2) as usize);
    put_text(
        b,
        field.x,
        field.y,
        field.width,
        &format!(" {shown}"),
        field_style,
    );
    put_text(
        b,
        inner.x,
        inner.y + 5,
        inner.width,
        " drop a file onto this window to fill the path",
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );

    let buttons = row(inner, &[11, 12], 2, inner.height.saturating_sub(1));
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    button(
        b,
        *primary,
        " ↵ start ",
        Style::default()
            .fg(contrast(p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    button(
        b,
        *cancel,
        " esc cancel ",
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
    );
    Some(OverlayRender {
        primary: *primary,
        cancel: *cancel,
        cursor: Some(crate::protocol::CursorState {
            x: (field.x + 1 + display_width(&shown)).min(field.right().saturating_sub(1)),
            y: field.y,
            visible: true,
            shape: 0,
        }),
        ..OverlayRender::default()
    })
}

fn render_progress(
    b: &mut Buffer,
    run: &ClientFileTransferRun,
    p: &Palette,
) -> Option<OverlayRender> {
    let popup = popup(b.area, PROMPT_WIDTH, PROMPT_HEIGHT)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        &format!(" {}", run.direction.title()),
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        inner.x,
        inner.y + 2,
        inner.width,
        &format!(" {}", run.name),
        Style::default().fg(p.text).bg(p.panel_bg),
    );

    let buttons = row(inner, &[12], 2, inner.height.saturating_sub(1));
    let [cancel] = buttons.as_slice() else {
        return None;
    };
    match run.outcome.as_ref() {
        Some(Err(error)) => {
            // Bounded by the button row rather than fixed at two lines: the
            // buttons are drawn after this, so anything reaching their row is
            // overprinted mid-sentence.
            let rows = cancel.y.saturating_sub(inner.y + 3).max(1);
            put_wrapped(
                b,
                Rect::new(
                    inner.x + 1,
                    inner.y + 3,
                    inner.width.saturating_sub(1),
                    rows,
                ),
                error,
                Style::default().fg(p.red).bg(p.panel_bg),
            );
        }
        outcome => {
            put_text(
                b,
                inner.x,
                inner.y + 3,
                inner.width,
                &format!(
                    " {}",
                    progress_bar(run.ratio(), inner.width.saturating_sub(2))
                ),
                Style::default().fg(p.accent).bg(p.panel_bg),
            );
            let status = if outcome.is_some() {
                format!(" done - {}", human_bytes(run.size))
            } else {
                format!(" {} / {}", human_bytes(run.done), human_bytes(run.size))
            };
            put_text(
                b,
                inner.x,
                inner.y + 4,
                inner.width,
                &status,
                Style::default().fg(p.overlay0).bg(p.panel_bg),
            );
        }
    }
    button(
        b,
        *cancel,
        if run.finished() {
            " esc close "
        } else {
            " esc cancel "
        },
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
    );
    Some(OverlayRender {
        cancel: *cancel,
        ..OverlayRender::default()
    })
}

fn render_browser(
    b: &mut Buffer,
    browser: &ClientFileBrowser,
    p: &Palette,
) -> Option<OverlayRender> {
    let popup = popup(b.area, BROWSER_WIDTH, BROWSER_HEIGHT)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    if inner.height < 6 {
        return None;
    }
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        " receive file",
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        inner.x,
        inner.y + 1,
        inner.width,
        &format!(
            " {}",
            middle_elide(&browser.dir, inner.width.saturating_sub(2) as usize)
        ),
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    let (filter, filter_style) = if browser.query.is_empty() {
        (
            format!(" → {}", browser.destination),
            Style::default().fg(p.surface_dim).bg(p.panel_bg),
        )
    } else {
        (
            format!(" filter: {}", browser.query),
            Style::default().fg(p.text).bg(p.panel_bg),
        )
    };
    put_text(b, inner.x, inner.y + 2, inner.width, &filter, filter_style);

    let body = Rect::new(
        inner.x,
        inner.y + 3,
        inner.width,
        inner.height.saturating_sub(4),
    );
    let mut rows = Vec::new();
    if let Some(error) = browser.error.as_deref() {
        put_wrapped(
            b,
            Rect::new(
                body.x + 1,
                body.y,
                body.width.saturating_sub(1),
                body.height,
            ),
            error,
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    } else {
        rows = render_rows(b, browser, body, p);
    }

    let footer = if browser.truncated {
        format!(" ↑↓ move   ↵ open/select   . hidden   esc cancel   (first {FILE_BROWSER_MAX_ENTRIES} shown)")
    } else {
        " ↑↓ move   ↵ open/select   . hidden   esc cancel".to_owned()
    };
    put_text(
        b,
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        &footer,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        file_transfer_rows: rows,
        ..OverlayRender::default()
    })
}

fn render_rows(
    b: &mut Buffer,
    browser: &ClientFileBrowser,
    body: Rect,
    p: &Palette,
) -> Vec<(Rect, usize)> {
    let indices = browser.filtered_indices();
    if indices.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            if browser.loading {
                " listing…"
            } else {
                " no matching entries"
            },
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
        return Vec::new();
    }

    let visible = body.height as usize;
    let start = browser.window_start(visible);
    let mut rows = Vec::new();
    for (row, entry_index) in indices.iter().skip(start).take(visible).enumerate() {
        let Some(entry) = browser.entries.get(*entry_index) else {
            continue;
        };
        let rect = Rect::new(body.x, body.y + row as u16, body.width, 1);
        rows.push((rect, *entry_index));
        let selected = *entry_index == browser.selected;
        let style = if selected {
            Style::default()
                .fg(contrast(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else if entry.is_dir {
            Style::default().fg(p.blue).bg(p.panel_bg)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);

        // Size is right-aligned in its own column so the names stay scannable.
        let size = entry.size.map(human_bytes).unwrap_or_else(|| {
            if entry.is_dir {
                String::new()
            } else {
                "?".into()
            }
        });
        let name = if entry.is_dir && !entry.is_parent {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        let marker = if selected { "▸" } else { " " };
        // Clip the name short of the size column so a long name cannot push the
        // size off the row.
        let name_width = body
            .width
            .saturating_sub(display_width(&size).saturating_add(2))
            .max(1);
        put_text(
            b,
            rect.x,
            rect.y,
            name_width,
            &format!(" {marker} {name}"),
            style,
        );
        put_right_text(b, rect, rect.y, &format!("{size} "), style);
    }
    rows
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn progress_bar(ratio: f64, width: u16) -> String {
    let width = width as usize;
    let filled = ((ratio * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Peer-supplied failure text is a sentence and routinely outruns one modal row.
fn put_wrapped(b: &mut Buffer, area: Rect, text: &str, style: Style) {
    use ratatui::widgets::{Paragraph, Widget, Wrap};

    Paragraph::new(text)
        .style(style)
        .wrap(Wrap { trim: true })
        .render(area, b);
}
