use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, Focus};
use crate::buffer::{self, Buffer};
use crate::highlight::UiPalette;
use crate::media::{self, Media};

const TREE_WIDTH: u16 = 32;

// A fixed accent that reads as intentional UI on any dark theme. Text, dim and
// selection are derived from the active theme instead (see UiPalette), so the
// chrome tracks the code; no background is ever painted, so the terminal's own
// background and any transparency show through, the status bar included.
const ACCENT: Color = Color::Rgb(97, 175, 239);

// Semantic status foregrounds: success, warning (unsaved / disk conflict) and
// error. Mid-tones chosen to stay legible on any dark background.
const OK: Color = Color::Rgb(152, 195, 121);
const WARN: Color = Color::Rgb(209, 154, 102);
const ERR: Color = Color::Rgb(224, 108, 117);

const IS_MAC: bool = cfg!(target_os = "macos");

/// A run of status-bar text with its own style. The bar is assembled from a
/// list of these so each part (mode badge, path, warning) is colored on its own.
type Seg = (String, Style);

/// ASCII wordmark shown on the empty-state welcome screen, centered as a block.
const LOGO: &[&str] = &[
    r"                        _       ",
    r"  ___    ___  ___    __| |  ___ ",
    r" / _ \  / __|/ _ \  / _` | / _ \",
    r"| (_) || (__| (_) || (_| ||  __/",
    r" \___/  \___|\___/  \__,_| \___|",
];

// Built with concat! so the leading indentation is part of each literal —
// a `\`-continuation would let Rust strip the spaces and flatten the preview.
const PREVIEW: &str = concat!(
    "// ocode — preview of this style\n",
    "use std::collections::HashMap;\n",
    "\n",
    "/// Greet a user by name.\n",
    "pub fn greet(name: &str, count: u32) -> String {\n",
    "    let mut seen: HashMap<&str, u32> = HashMap::new();\n",
    "    seen.insert(name, count);\n",
    "\n",
    "    if count > 0 {\n",
    "        format!(\"hi, {name}! x{count}\")\n",
    "    } else {\n",
    "        String::from(\"hello, world\")\n",
    "    }\n",
    "}\n",
);

pub fn render(frame: &mut Frame, app: &mut App) {
    let root = frame.area();

    if let Some(sel) = app.picker {
        let pal = app.highlighter.ui_palette_for(sel);

        render_picker(frame, app, root, pal);

        return;
    }

    let pal = app.highlighter.ui_palette();

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(root);

    let main = chunks[0];

    let status_area = chunks[1];

    let editor_area = if app.tree_visible {
        let cols = Layout::horizontal([Constraint::Length(TREE_WIDTH), Constraint::Min(0)]).split(main);

        render_tree(frame, app, cols[0], pal);

        cols[1]
    } else {
        app.tree_area = None;

        main
    };

    render_editor(frame, app, editor_area, pal);

    render_status(frame, app, status_area, pal);
}

fn render_picker(frame: &mut Frame, app: &App, area: Rect, pal: UiPalette) {
    let sel = app.picker.unwrap_or(0);

    let count = app.highlighter.theme_count();

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(area);

    let title = Line::from(Span::styled(
        format!("  ocode — choose a style ({count} available)"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(Paragraph::new(title), rows[0]);

    let cols = Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).split(rows[1]);

    render_theme_list(frame, app, cols[0], sel, count, pal);

    render_preview(frame, app, cols[1], sel, pal);

    render_picker_footer(frame, rows[2], count, pal);
}

fn render_theme_list(frame: &mut Frame, app: &App, area: Rect, sel: usize, count: usize, pal: UiPalette) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(format!(" Styles ({count}) "));

    let inner = block.inner(area);

    frame.render_widget(block, area);

    let visible = inner.height as usize;

    if visible == 0 {
        return;
    }

    let offset = if sel >= visible { sel + 1 - visible } else { 0 };

    let names = app.highlighter.theme_names();

    let end = (offset + visible).min(names.len());

    let lines: Vec<Line> = names[offset..end]
        .iter()
        .enumerate()
        .map(|(row, name)| {
            let i = offset + row;

            let marker = if i == sel { "› " } else { "  " };

            let mut style = Style::default().fg(pal.fg);

            if i == sel {
                style = style.bg(pal.selection).add_modifier(Modifier::BOLD);
            }

            Line::from(Span::styled(format!("{marker}{name}"), style))
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_preview(frame: &mut Frame, app: &App, area: Rect, sel: usize, pal: UiPalette) {
    let names = app.highlighter.theme_names();

    let tag = if app.highlighter.theme_is_dark(sel) { "dark" } else { "light" };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(pal.dim))
        .title(format!(" Preview · {} · {tag} ", names.get(sel).copied().unwrap_or("")));

    let inner = block.inner(area);

    frame.render_widget(block, area);

    let blocks = app.highlighter.highlight_block(PREVIEW, "rs", sel);

    let num_w = blocks.len().to_string().len().max(2);

    let lines: Vec<Line> = blocks
        .iter()
        .enumerate()
        .map(|(i, spans)| {
            let mut out = vec![Span::styled(
                format!("{:>num_w$} ", i + 1),
                Style::default().fg(pal.dim),
            )];

            out.extend(spans.iter().map(|(style, text)| Span::styled(text.clone(), *style)));

            Line::from(out)
        })
        .collect();

    // No background: only the syntax foreground colors are drawn, so the user's
    // own terminal background is preserved exactly as in the editor.
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_picker_footer(frame: &mut Frame, area: Rect, count: usize, pal: UiPalette) {
    let word_jump = if IS_MAC { "⌥+←/→" } else { "Ctrl+←/→" };

    let keys = Line::from(Span::styled(
        "  ↑/↓ select     Enter apply & save     Esc quit",
        Style::default().fg(pal.fg),
    ));

    let info = Line::from(Span::styled(
        format!(
            "  {count} styles · word-jump {word_jump} · add .tmTheme in {} · change later: ocode --style",
            crate::config::themes_dir_display()
        ),
        Style::default().fg(pal.dim),
    ));

    frame.render_widget(Paragraph::new(vec![keys, info]), area);
}

fn render_tree(frame: &mut Frame, app: &mut App, area: Rect, pal: UiPalette) {
    let title = app
        .tree
        .root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".")
        .to_string();

    let focused = app.focus == Focus::Tree;

    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(pal.dim)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(format!(" {title} "));

    let inner = block.inner(area);

    frame.render_widget(block, area);

    app.tree_area = Some((inner.x, inner.y, inner.width, inner.height));

    let visible = inner.height as usize;

    if visible == 0 {
        return;
    }

    // Never leave the window past the end (the list shrinks when a directory
    // collapses or files disappear).
    app.tree.scroll = app.tree.scroll.min(app.tree.nodes.len().saturating_sub(visible));

    // While the wheel is driving the list, stop chasing the selection.
    if !app.tree_scroll_free {
        if app.tree.selected < app.tree.scroll {
            app.tree.scroll = app.tree.selected;
        } else if app.tree.selected >= app.tree.scroll + visible {
            app.tree.scroll = app.tree.selected + 1 - visible;
        }
    }

    let mut lines: Vec<Line> = Vec::with_capacity(visible);

    let end = (app.tree.scroll + visible).min(app.tree.nodes.len());

    for idx in app.tree.scroll..end {
        let node = &app.tree.nodes[idx];

        let indent = "  ".repeat(node.depth);

        let marker = if node.is_dir {
            if node.expanded {
                "▾ "
            } else {
                "▸ "
            }
        } else {
            "  "
        };

        let fg = if node.is_dir { ACCENT } else { pal.fg };

        let mut style = Style::default().fg(fg);

        if idx == app.tree.selected {
            style = style.bg(pal.selection).add_modifier(Modifier::BOLD);
        }

        let text = format!("{indent}{marker}{}", node.name);

        lines.push(Line::from(Span::styled(text, style)));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Centered logo + tagline shown when no file is open yet.
fn render_welcome(frame: &mut Frame, area: Rect, pal: UiPalette) {
    let width = area.width as usize;

    let logo_w = LOGO.iter().map(|l| l.chars().count()).max().unwrap_or(0);

    let logo_pad = " ".repeat(width.saturating_sub(logo_w) / 2);

    let nav = if IS_MAC { "⌥/Shift+arrows" } else { "Ctrl/Shift+arrows" };

    let mut content: Vec<Line> = LOGO
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                format!("{logo_pad}{l}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();

    content.push(Line::from(""));

    content.push(centered(width, "a fast terminal code reader & editor", Style::default().fg(pal.fg)));

    content.push(Line::from(""));

    content.push(centered(width, "Enter or Ctrl+B  browse files        Ctrl+Q  quit", Style::default().fg(pal.dim)));

    content.push(centered(width, &format!("move with {nav}  ·  Ctrl+S save  ·  Ctrl+Z undo"), Style::default().fg(pal.dim)));

    let top = (area.height as usize).saturating_sub(content.len()) / 2;

    let mut lines: Vec<Line> = vec![Line::from(""); top];

    lines.extend(content);

    frame.render_widget(Paragraph::new(lines), area);
}

fn centered(width: usize, text: &str, style: Style) -> Line<'static> {
    let pad = " ".repeat(width.saturating_sub(text.chars().count()) / 2);

    Line::from(Span::styled(format!("{pad}{text}"), style))
}

fn render_editor(frame: &mut Frame, app: &mut App, area: Rect, pal: UiPalette) {
    if app.media.is_some() {
        app.editor_area = None;

        render_media(frame, app, area, pal);

        return;
    }

    if app.buffer.is_none() {
        app.editor_area = None;

        render_welcome(frame, area, pal);

        return;
    }

    // Read before the buffer is borrowed mutably. Matches are recomputed per
    // visible line each frame rather than cached, so an edit, an undo or a
    // reload from disk can never leave stale positions painted on screen.
    let query = app
        .search
        .as_ref()
        .map(|s| s.query.clone())
        .filter(|q| !q.is_empty())
        .unwrap_or_default();

    let Some(buf) = app.buffer.as_mut() else {
        return;
    };

    let height = area.height as usize;

    let width = area.width as usize;

    if height == 0 || width == 0 {
        return;
    }

    app.page_rows = height;

    let total = buf.last_line() + 1;

    let num_w = total.to_string().len().max(3);

    let gutter_w = num_w + 1;

    let text_width = width.saturating_sub(gutter_w);

    app.editor_area = Some((area.x, area.y, area.width, area.height));

    app.gutter_w = gutter_w as u16;

    // While the wheel has scrolled away from the caret the view stays put; any
    // keystroke clears the flag and the caret pulls it back.
    if !app.scroll_free {
        scroll_into_view(buf, height, text_width);
    }

    let target = buf.scroll_row + height;

    app.highlighter.ensure(&mut buf.hl, &buf.rope, target);

    let mut lines: Vec<Line> = Vec::with_capacity(height);

    let last = buf.last_line();

    for row in 0..height {
        let li = buf.scroll_row + row;

        if li > last {
            break;
        }

        let num_style = if li == buf.cursor_line {
            Style::default().fg(pal.fg)
        } else {
            Style::default().fg(pal.dim)
        };

        let gutter = Span::styled(format!("{:>w$} ", li + 1, w = num_w), num_style);

        let mut spans = vec![gutter];

        let sel = buf.selection_for_line(li);

        let hits = if query.is_empty() {
            Vec::new()
        } else {
            // Only scan as far as the rightmost visible column: a match that
            // starts past it cannot be seen, and this keeps a minified
            // one-megabyte line from being walked on every frame.
            let limit = buf.scroll_col + text_width;

            let text: String = buf
                .rope
                .line(li)
                .chars()
                .take_while(|c| *c != '\n')
                .take(limit)
                .collect();

            buffer::match_columns(&text, &query)
        };

        if let Some(cached) = buf.hl.line(li) {
            spans.extend(slice_spans(
                cached,
                buf.scroll_col,
                text_width,
                sel,
                pal.selection,
                &hits,
                pal.search_match,
            ));
        }

        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), area);

    // A wheel scroll can leave the caret off screen; placing it then would
    // underflow, so the terminal cursor is simply hidden until it is back.
    let caret_visible = buf.cursor_line >= buf.scroll_row
        && buf.cursor_line < buf.scroll_row + height
        && buf.cursor_col >= buf.scroll_col
        && buf.cursor_col - buf.scroll_col < text_width.max(1);

    if app.focus == Focus::Editor && app.search.is_none() && caret_visible {
        let cx = area.x + gutter_w as u16 + (buf.cursor_col - buf.scroll_col) as u16;

        let cy = area.y + (buf.cursor_line - buf.scroll_row) as u16;

        frame.set_cursor_position((cx, cy));
    }
}

fn render_media(frame: &mut Frame, app: &mut App, area: Rect, pal: UiPalette) {
    let showing_picture = matches!(app.media, Some(Media::Image(_))) && !app.media_info;

    if showing_picture {
        // Leave the area blank; the run loop paints the image into it. This is
        // the editor pane, so it already sits clear of the sidebar and the
        // image is simply scaled into whatever width is left.
        app.image_cells = (area.width > 0 && area.height > 0)
            .then_some((area.x, area.y, area.width, area.height));

        return;
    }

    app.image_cells = None;

    render_inspector(frame, app, area, pal);
}

/// Metadata groups followed by the whole file in hex, as one scrollable page.
/// Only the visible rows are built, and for a binary only those bytes are read,
/// so opening a huge file costs the same as a small one.
fn render_inspector(frame: &mut Frame, app: &mut App, area: Rect, pal: UiPalette) {
    let height = area.height as usize;

    let width = area.width as usize;

    if height == 0 || width == 0 {
        return;
    }

    app.media_rows = height;

    let (mut meta, hex_rows) = match &app.media {
        Some(Media::Binary(doc)) => (media::meta_rows(&doc.meta), doc.hex_rows()),

        Some(Media::Image(doc)) => (media::meta_rows(&doc.meta), 0),

        None => return,
    };

    // Breathing room before the dump starts.
    if hex_rows > 0 && !meta.is_empty() {
        meta.push(media::Row::Blank);
    }

    let total = meta.len() as u64 + hex_rows;

    // Keep the last screenful reachable but never scroll past the end.
    let max_scroll = total.saturating_sub(height as u64);

    app.media_scroll = app.media_scroll.min(max_scroll);

    let first = app.media_scroll;

    // Pull only the bytes the visible hex rows need.
    if let Some(Media::Binary(doc)) = app.media.as_mut() {
        let hex_first = first.saturating_sub(meta.len() as u64);

        doc.ensure_window(hex_first, height + 1);
    }

    let label = Style::default().fg(pal.dim);

    let value = Style::default().fg(pal.fg);

    let title = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);

    // Line up the values in a column, but never let a long label crowd them out.
    let label_w = meta
        .iter()
        .filter_map(|r| match r {
            media::Row::Field(l, _) => Some(l.chars().count()),

            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(18);

    let mut lines: Vec<Line> = Vec::with_capacity(height);

    for i in 0..height as u64 {
        let row = first + i;

        if row >= total {
            break;
        }

        let line = if let Some(meta_row) = meta.get(row as usize) {
            match meta_row {
                media::Row::Title(text) => Line::from(Span::styled(format!("  {text}"), title)),

                media::Row::Field(l, v) => Line::from(vec![
                    Span::styled(format!("    {l:<label_w$}  "), label),
                    Span::styled(v.clone(), value),
                ]),

                media::Row::Blank => Line::from(""),
            }
        } else {
            let hex_row = row - meta.len() as u64;

            match &app.media {
                Some(Media::Binary(doc)) => {
                    let bytes = doc.hex_row(hex_row);

                    let (hex, ascii) = media::hex_line(hex_row * media::HEX_COLS as u64, bytes);

                    Line::from(vec![
                        Span::styled(hex, label),
                        Span::styled(ascii, value),
                    ])
                }

                _ => Line::from(""),
            }
        };

        lines.push(line);
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect, pal: UiPalette) {
    let width = area.width as usize;

    let badge = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);

    let text = Style::default().fg(pal.fg);

    let dim = Style::default().fg(pal.dim);

    let warn = Style::default().fg(WARN).add_modifier(Modifier::BOLD);

    let conflict = app.search.is_none() && app.buffer.as_ref().is_some_and(|b| b.disk_changed);

    let (left, right): (Vec<Seg>, Vec<Seg>) = if let Some(search) = &app.search {
        (
            vec![(" Find: ".to_string(), badge), (search.query.clone(), text)],
            vec![(" Enter: next match  Esc: close ".to_string(), dim)],
        )
    } else if conflict {
        let buf = app.buffer.as_ref().unwrap();

        (
            vec![(
                format!(" ⚠ {} changed on disk — Ctrl+R reload · Ctrl+S overwrite", buf.file_name()),
                warn,
            )],
            vec![(format!("Ln {}, Col {} ", buf.cursor_line + 1, buf.cursor_col + 1), warn)],
        )
    } else if let Some(buf) = &app.buffer {
        let focus = if app.focus == Focus::Tree { "TREE" } else { "EDIT" };

        let mut left = vec![
            (format!(" [{focus}] "), badge),
            (buf.path.display().to_string(), text),
        ];

        if buf.modified {
            left.push(("*".to_string(), warn));
        }

        if !app.status.is_empty() {
            left.push((format!("  {}", app.status), flash_style(app.status_ok)));
        }

        (
            left,
            vec![(format!("Ln {}, Col {} ", buf.cursor_line + 1, buf.cursor_col + 1), dim)],
        )
    } else if let Some(m) = &app.media {
        let (path, info) = match m {
            Media::Image(d) => (
                d.path.display().to_string(),
                format!("{} · {}×{} · {}", d.format, d.width, d.height, media::human_size(d.byte_len)),
            ),

            Media::Binary(d) => (
                d.path.display().to_string(),
                format!("{} · {}", d.format, media::human_size(d.byte_len)),
            ),
        };

        // An image can flip to its metadata; a binary is already showing it.
        let hint = match m {
            Media::Image(_) if app.media_info => "  i: picture",

            Media::Image(_) => "  i: info",

            Media::Binary(_) => "",
        };

        (
            vec![
                (" [VIEW] ".to_string(), badge),
                (path, text),
                (hint.to_string(), dim),
            ],
            vec![(format!("{info} "), dim)],
        )
    } else {
        let mut left = vec![(" [TREE] ".to_string(), badge)];

        if app.status.is_empty() {
            left.push(("ocode".to_string(), dim));
        } else {
            left.push((app.status.clone(), flash_style(app.status_ok)));
        }

        (left, Vec::new())
    };

    frame.render_widget(Paragraph::new(compose_bar(&left, &right, width)), area);
}

/// Green for a success flash, red for a lingering error (see `App::flash` /
/// `App::set_error`).
fn flash_style(ok: bool) -> Style {
    if ok {
        Style::default().fg(OK).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ERR)
    }
}

/// Lay out a status bar exactly `width` columns wide from styled segments: pin
/// `right` to the right edge and truncate `left` from its head (keeping the
/// tail, e.g. the file name) when the two would not fit. No background is drawn,
/// so the terminal's own background and any transparency show through.
fn compose_bar(left: &[Seg], right: &[Seg], width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }

    let right_w = seg_len(right).min(width);

    let avail = width - right_w;

    let left_len = seg_len(left);

    let mut spans: Vec<Span<'static>> = Vec::new();

    if left_len <= avail {
        for (t, s) in left {
            spans.push(Span::styled(t.clone(), *s));
        }

        spans.push(Span::raw(" ".repeat(avail - left_len)));
    } else if avail > 0 {
        spans.push(Span::raw("…"));

        spans.extend(tail_spans(left, avail - 1));
    }

    spans.extend(tail_spans(right, right_w));

    Line::from(spans)
}

fn seg_len(segs: &[Seg]) -> usize {
    segs.iter().map(|(t, _)| t.chars().count()).sum()
}

/// The last `keep` characters of a styled segment list, preserving each
/// segment's style and splitting the boundary segment.
fn tail_spans(segs: &[Seg], keep: usize) -> Vec<Span<'static>> {
    if keep == 0 {
        return Vec::new();
    }

    let skip = seg_len(segs).saturating_sub(keep);

    let mut out: Vec<Span<'static>> = Vec::new();

    let mut idx = 0;

    for (text, style) in segs {
        let seg_start = idx;

        idx += text.chars().count();

        if idx <= skip {
            continue;
        }

        let start_in_seg = skip.saturating_sub(seg_start);

        let kept: String = text.chars().skip(start_in_seg).collect();

        if !kept.is_empty() {
            out.push(Span::styled(kept, *style));
        }
    }

    out
}

fn scroll_into_view(buf: &mut Buffer, height: usize, text_width: usize) {
    if buf.cursor_line < buf.scroll_row {
        buf.scroll_row = buf.cursor_line;
    } else if buf.cursor_line >= buf.scroll_row + height {
        buf.scroll_row = buf.cursor_line + 1 - height;
    }

    if text_width == 0 {
        return;
    }

    if buf.cursor_col < buf.scroll_col {
        buf.scroll_col = buf.cursor_col;
    } else if buf.cursor_col >= buf.scroll_col + text_width {
        buf.scroll_col = buf.cursor_col + 1 - text_width;
    }
}

/// Take the slice of highlighted spans visible in `[start, start + width)`
/// display columns, expanding tabs to a single space so columns stay aligned
/// with the cursor (which counts one column per char). Columns inside `sel`
/// (line-relative char range) get the selection background.
fn slice_spans(
    spans: &[(Style, String)],
    start: usize,
    width: usize,
    sel: Option<(usize, usize)>,
    sel_bg: Color,
    hits: &[(usize, usize)],
    hit_bg: Color,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();

    if width == 0 {
        return out;
    }

    let stop = start + width;

    let mut col = 0usize;

    let mut chunk = String::new();

    let mut chunk_style: Option<Style> = None;

    'outer: for (style, text) in spans {
        for ch in text.chars() {
            if col >= stop {
                break 'outer;
            }

            if col >= start {
                // The selection marks the current match, so it wins over the
                // plain-match tint wherever both cover a cell.
                let cell_style = if sel.is_some_and(|(a, b)| col >= a && col < b) {
                    style.bg(sel_bg)
                } else if hits.iter().any(|(a, b)| col >= *a && col < *b) {
                    style.bg(hit_bg)
                } else {
                    *style
                };

                if chunk_style != Some(cell_style) {
                    flush_chunk(&mut out, &mut chunk, chunk_style);

                    chunk_style = Some(cell_style);
                }

                chunk.push(if ch == '\t' { ' ' } else { ch });
            }

            col += 1;
        }
    }

    flush_chunk(&mut out, &mut chunk, chunk_style);

    out
}

fn flush_chunk(out: &mut Vec<Span<'static>>, chunk: &mut String, style: Option<Style>) {
    if let Some(style) = style {
        if !chunk.is_empty() {
            out.push(Span::styled(std::mem::take(chunk), style));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use crate::app::App;

    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();

        terminal.draw(|frame| super::render(frame, app)).unwrap();

        format!("{}", terminal.backend())
    }

    #[test]
    fn editor_renders_code_with_line_numbers() {
        let path = std::env::temp_dir().join("ocode_render_test.py");

        fs::write(&path, "def greet(name):\n    return name\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        let screen = render_to_string(&mut app, 80, 24);

        assert!(screen.contains("def greet"), "code not rendered:\n{screen}");

        assert!(screen.contains("return name"), "second line missing");

        assert!(screen.contains("Ln 1, Col 1"), "status line missing");

        let _ = fs::remove_file(path);
    }

    /// Every occurrence is tinted while the find bar is open, and the current
    /// match (the selection) is tinted differently so it stands out among them.
    #[test]
    fn find_highlights_every_match_and_marks_the_current_one() {
        let path = std::env::temp_dir().join("ocode_find_render.txt");

        fs::write(&path, "foo and foo\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        for c in "foo".chars() {
            app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char(c)));
        }

        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter));

        let (w, h) = (40u16, 4u16);

        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();

        terminal.draw(|frame| super::render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();

        // Gutter is three digits plus a space, so text starts at column 4.
        let bg_at = |x: u16| buffer.cell((x, 0)).unwrap().bg;

        let current = bg_at(4); // first "foo", the hit Enter landed on

        let other = bg_at(12); // second "foo" at column 8 of the text

        let plain = bg_at(7); // the space after the first "foo"

        assert_ne!(current, Color::Reset, "the current match is tinted");

        assert_ne!(other, Color::Reset, "the other match is tinted too");

        assert_ne!(current, other, "and the current match is told apart from it");

        assert_eq!(plain, Color::Reset, "text outside a match keeps the terminal background");

        let _ = fs::remove_file(path);
    }

    /// Jumping to a match below the fold has to bring the view with it, or the
    /// caret lands somewhere the user cannot see.
    #[test]
    fn find_scrolls_the_view_to_a_match_off_screen() {
        let path = std::env::temp_dir().join("ocode_find_scroll.txt");

        let mut text: String = (0..60).map(|i| format!("line {i}\n")).collect();

        text.push_str("needle here\n");

        fs::write(&path, &text).unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();

        terminal.draw(|frame| super::render(frame, &mut app)).unwrap();

        assert_eq!(app.buffer.as_ref().unwrap().scroll_row, 0);

        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        for c in "needle".chars() {
            app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char(c)));
        }

        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter));

        terminal.draw(|frame| super::render(frame, &mut app)).unwrap();

        let buf = app.buffer.as_ref().unwrap();

        assert_eq!(buf.cursor_line, 60, "the caret is on the match");

        assert!(buf.scroll_row > 0, "and the view followed it down");

        let screen = format!("{}", terminal.backend());

        assert!(screen.contains("needle here"), "the match is on screen:\n{screen}");

        let _ = fs::remove_file(path);
    }

    /// Closing the find bar must take the tint with it.
    #[test]
    fn closing_find_clears_the_match_tint() {
        let path = std::env::temp_dir().join("ocode_find_clear.txt");

        fs::write(&path, "foo and foo\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        for c in "foo".chars() {
            app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char(c)));
        }

        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Esc));

        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();

        terminal.draw(|frame| super::render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();

        assert_eq!(
            buffer.cell((12, 0)).unwrap().bg,
            Color::Reset,
            "no tint remains once find is closed"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn status_bar_paints_no_opaque_background() {
        let path = std::env::temp_dir().join("ocode_status_bg.rs");

        fs::write(&path, "fn main() {}\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        let (w, h) = (80u16, 6u16);

        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();

        terminal.draw(|frame| super::render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();

        // The status bar is the last row; every cell must keep the terminal's
        // own background (Reset) so transparency is preserved.
        for x in 0..w {
            let bg = buffer.cell((x, h - 1)).unwrap().bg;

            assert_eq!(bg, Color::Reset, "status cell at x={x} paints an opaque background {bg:?}");
        }

        let _ = fs::remove_file(path);
    }

    #[test]
    fn render_records_the_editor_area_for_mouse_mapping() {
        let path = std::env::temp_dir().join("ocode_mouse_area.rs");

        fs::write(&path, "fn main() {}\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        let _ = render_to_string(&mut app, 80, 6);

        let area = app.editor_area.expect("editor area recorded for the mouse");

        assert_eq!(area, (0, 0, 80, 5), "status bar takes the last row");

        assert_eq!(app.gutter_w, 4, "3-digit line numbers plus a space");

        let _ = fs::remove_file(path);
    }

    /// A binary opens straight into the inspector: what the file is, then the
    /// whole thing in hex.
    #[test]
    fn binary_shows_metadata_then_hex() {
        let dir = std::env::temp_dir().join("ocode_inspect_bin");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("thing.bin");

        // A gzip header, so there is something to report beyond name and size.
        let mut bytes = vec![0x1f, 0x8b, 0x08, 0x00];

        bytes.extend((0..4000u32).map(|i| (i % 256) as u8));

        fs::write(&path, &bytes).unwrap();

        let mut app = App::new(path, false).unwrap();

        app.picker = None;

        // Wide enough that the status bar keeps its badge: a narrower terminal
        // legitimately truncates the left side to save the file name.
        let screen = render_to_string(&mut app, 130, 24);

        assert!(screen.contains("gzip"), "the format is named:\n{screen}");

        assert!(screen.contains("thing.bin"), "the file is named");

        assert!(screen.contains("00000000"), "the hex dump starts at the top of the file");

        assert!(screen.contains("[VIEW]"), "status bar marks the view");

        let _ = fs::remove_dir_all(dir);
    }

    /// Paging must reach the tail of a file too large to hold on one screen,
    /// and stop there rather than scrolling into blank space.
    #[test]
    fn inspector_scrolls_to_the_end_and_stops() {
        let dir = std::env::temp_dir().join("ocode_inspect_scroll");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("big.bin");

        let len = 40_000usize;

        fs::write(&path, (0..len).map(|i| (i % 251) as u8).collect::<Vec<u8>>()).unwrap();

        let mut app = App::new(path, false).unwrap();

        app.picker = None;

        let _ = render_to_string(&mut app, 90, 20);

        // End: the last row of the file has to be on screen.
        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::End));

        let screen = render_to_string(&mut app, 90, 20);

        let last_offset = format!("{:08x}", (len - 1) / 16 * 16);

        assert!(screen.contains(&last_offset), "the final row {last_offset} is shown:\n{screen}");

        let settled = app.media_scroll;

        // Pressing End again must not run past the end.
        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::End));

        let _ = render_to_string(&mut app, 90, 20);

        assert_eq!(app.media_scroll, settled, "already at the end, so nothing moves");

        // Home returns to the first row.
        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Home));

        let screen = render_to_string(&mut app, 90, 20);

        assert_eq!(app.media_scroll, 0);

        assert!(screen.contains("00000000"), "back at the start of the file");

        let _ = fs::remove_dir_all(dir);
    }

    /// An image shows the picture; `i` swaps it for what the file says about
    /// itself, and back.
    #[test]
    fn i_toggles_an_image_between_picture_and_metadata() {
        let dir = std::env::temp_dir().join("ocode_inspect_img");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("p.png");

        let mut img = image::RgbaImage::new(9, 4);

        for px in img.pixels_mut() {
            *px = image::Rgba([10, 20, 30, 255]);
        }

        image::DynamicImage::ImageRgba8(img).save(&path).unwrap();

        let mut app = App::new(path, false).unwrap();

        app.picker = None;

        let _ = render_to_string(&mut app, 80, 14);

        assert!(app.image_cells.is_some(), "the picture is painted by default");

        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('i')));

        let screen = render_to_string(&mut app, 80, 14);

        assert!(app.image_cells.is_none(), "the picture gives way to the metadata");

        assert!(screen.contains("9 x 4 px"), "real dimensions are reported:\n{screen}");

        assert!(screen.contains("truecolour"), "the colour type comes from the PNG header");

        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('i')));

        let _ = render_to_string(&mut app, 80, 14);

        assert!(app.image_cells.is_some(), "and back to the picture");

        let _ = fs::remove_dir_all(dir);
    }

    /// An image opened with the sidebar up must still be painted, in the editor
    /// pane beside the tree rather than suppressed.
    #[test]
    fn image_renders_beside_an_open_sidebar() {
        let dir = std::env::temp_dir().join("ocode_img_sidebar");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("p.png");

        let mut img = image::RgbaImage::new(20, 10);

        for px in img.pixels_mut() {
            *px = image::Rgba([10, 20, 30, 255]);
        }

        image::DynamicImage::ImageRgba8(img).save(&path).unwrap();

        let mut app = App::new(path, false).unwrap();

        app.picker = None;

        app.tree_visible = true;

        let _ = render_to_string(&mut app, 80, 10);

        let cells = app.image_cells.expect("image still painted with the tree open");

        assert_eq!(cells.0, 32, "placed to the right of the 32-column sidebar");

        assert_eq!(cells.2, 48, "and given the remaining width");

        let _ = fs::remove_dir_all(dir);
    }

    fn write_png(path: &std::path::Path, w: u32, h: u32) {
        let mut img = image::RgbaImage::new(w, h);

        for px in img.pixels_mut() {
            *px = image::Rgba([10, 20, 30, 255]);
        }

        image::DynamicImage::ImageRgba8(img).save(path).unwrap();
    }

    fn left_click(column: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// Two images opened one after the other land on the same pane box, so the
    /// placement has to differ by image or the run loop skips the repaint and
    /// leaves the first one on screen.
    #[test]
    fn switching_images_changes_the_placement() {
        let dir = std::env::temp_dir().join("ocode_img_switch");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        // Same size on purpose: identical cell box, so only the image differs.
        write_png(&dir.join("a.png"), 20, 10);

        write_png(&dir.join("b.png"), 20, 10);

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter));

        // Render once so the sidebar geometry clicks map against is real.
        let _ = render_to_string(&mut app, 80, 12);

        let (_, ty, _, _) = app.tree_area.expect("sidebar recorded");

        let row_of = |app: &App, name: &str| {
            let idx = app.tree.nodes.iter().position(|n| n.name == name).expect("row");

            ty + (idx - app.tree.scroll) as u16
        };

        // Two clicks to open: the first only selects.
        let ra = row_of(&app, "a.png");

        app.on_mouse(left_click(2, ra));

        app.on_mouse(left_click(2, ra));

        let _ = render_to_string(&mut app, 80, 12);

        let first = app.image_placement().expect("a.png placed");

        let rb = row_of(&app, "b.png");

        app.on_mouse(left_click(2, rb));

        app.on_mouse(left_click(2, rb));

        let _ = render_to_string(&mut app, 80, 12);

        let second = app.image_placement().expect("b.png placed");

        assert_eq!(first.1, second.1, "same geometry: the box alone cannot tell them apart");

        assert_ne!(first, second, "so the placement must differ by image");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn image_is_centered_in_the_pane() {
        let dir = std::env::temp_dir().join("ocode_img_center");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("wide.png");

        // 2:1 pixels means 4 cells wide per cell tall, so an 11-row pane fits
        // 44 columns of image inside 80, leaving 36 columns to split evenly.
        write_png(&path, 20, 10);

        let mut app = App::new(path, false).unwrap();

        app.picker = None;

        let _ = render_to_string(&mut app, 80, 12);

        let (_, (x, y, cols, rows)) = app.image_placement().expect("image placed");

        assert_eq!((cols, rows), (44, 11), "fitted to the pane, aspect preserved");

        assert_eq!(x, (80 - 44) / 2, "centered horizontally");

        assert_eq!(y, 0, "full height, so nothing to centre vertically");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tree_renders_directory() {
        let dir = std::env::temp_dir().join("ocode_tree_test");

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("readme.md"), "# hi").unwrap();

        let mut app = App::new(PathBuf::from(&dir), false).unwrap();

        app.picker = None;

        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter)); // open the browser

        let screen = render_to_string(&mut app, 80, 24);

        assert!(screen.contains("readme.md"), "tree entry missing:\n{screen}");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn welcome_screen_shows_logo_when_no_file() {
        let dir = std::env::temp_dir().join("ocode_welcome_test");

        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(dir.join("x.txt"), "x").unwrap();

        let mut app = App::new(PathBuf::from(&dir), false).unwrap();

        app.picker = None;

        assert!(app.buffer.is_none(), "directory launch should open no file");

        let screen = render_to_string(&mut app, 100, 26);

        assert!(screen.contains("terminal code reader"), "welcome tagline missing:\n{screen}");

        assert!(!screen.contains("no file open"), "old empty-state text should be gone");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn preview_keeps_indentation() {
        let path = std::env::temp_dir().join("ocode_indent.rs");

        std::fs::write(&path, "x").unwrap();

        let mut app = App::new(path.clone(), true).unwrap();

        let screen = render_to_string(&mut app, 110, 26);

        // The body lines must keep their leading whitespace in the preview.
        assert!(
            screen.contains("    seen.insert"),
            "preview lost indentation:\n{screen}"
        );

        assert!(
            screen.contains("        format!"),
            "preview lost nested indentation"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn picker_lists_styles_with_preview() {
        let path = std::env::temp_dir().join("ocode_picker_test.rs");

        fs::write(&path, "fn x() {}\n").unwrap();

        let mut app = App::new(path.clone(), true).unwrap();

        assert!(app.picker.is_some(), "picker should show when forced");

        let screen = render_to_string(&mut app, 110, 30);

        assert!(screen.contains("choose a style"), "title missing:\n{screen}");

        assert!(screen.contains("Dracula"), "bundled theme missing from list");

        assert!(screen.contains("Preview"), "preview pane missing");

        assert!(screen.contains("greet"), "preview snippet missing");

        let _ = fs::remove_file(path);
    }
}
