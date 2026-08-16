use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::buffer::{self, Buffer, DiskEvent};
use crate::highlight::SyntaxHighlighter;
use crate::media::{self, Loaded, Media};
use crate::tree::FileTree;

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Focus {
    Tree,

    Editor,
}

pub struct Search {
    pub query: String,
}

/// Longest selection that seeds the find query when Ctrl+F opens it.
const SEARCH_SEED_MAX: usize = 100;

/// Lines the editor view moves per wheel event.
const EDITOR_SCROLL_STEP: isize = 3;

/// Rows the file tree scrolls per wheel event. Deliberately one: a trackpad
/// emits a burst of events per gesture, and a bigger step makes the list bolt.
const TREE_SCROLL_STEP: isize = 1;

fn in_area(area: Option<(u16, u16, u16, u16)>, (col, row): (u16, u16)) -> bool {
    let Some((x, y, w, h)) = area else {
        return false;
    };

    col >= x && col < x + w && row >= y && row < y + h
}

pub struct App {
    pub buffer: Option<Buffer>,

    /// A non-text file (image or other binary) open in place of a text buffer;
    /// `buffer` and `media` are never both `Some`.
    pub media: Option<Media>,

    /// Cell box (x, y, cols, rows) where the run loop should paint the open
    /// image; set by the renderer each frame, `None` when no image is shown.
    pub image_cells: Option<(u16, u16, u16, u16)>,

    /// Bumped every time `media` is replaced, so the run loop can tell one open
    /// image from the next even when both land on the same cell box. Must be
    /// bumped alongside any assignment to `media`.
    media_epoch: u64,

    /// First visible row of the media inspector (metadata, then the hex dump).
    pub media_scroll: u64,

    /// Whether an open image is showing its metadata instead of the picture.
    /// Binaries always show the inspector, so this only applies to images.
    pub media_info: bool,

    /// Rows the inspector can show at once, recorded each render so paging and
    /// clamping know the step.
    pub media_rows: usize,

    pub tree: FileTree,

    pub highlighter: SyntaxHighlighter,

    pub focus: Focus,

    pub tree_visible: bool,

    pub search: Option<Search>,

    /// When `Some(idx)` the startup style picker is shown with `idx` highlighted;
    /// `None` once a style has been chosen and the editor is active.
    pub picker: Option<usize>,

    pub status: String,

    /// When set, `status` is a success "flash" (green) that auto-clears at this
    /// instant; `None` means a persistent message (or none).
    pub status_until: Option<Instant>,

    pub status_ok: bool,

    pub quit_confirm: bool,

    /// Set after a refused save when the file changed on disk; a second Ctrl+S
    /// then overwrites it.
    pub overwrite_confirm: bool,

    /// Set when Esc has nothing left to cancel; a second Esc then quits.
    pub esc_confirm: bool,

    /// A file the user asked to open while the current buffer has unsaved edits;
    /// opening it again confirms discarding those edits.
    pub pending_open: Option<PathBuf>,

    pub should_quit: bool,

    /// Visible editor text rows, updated every render so paging knows the step.
    pub page_rows: usize,

    /// Editor text area (x, y, w, h) and its gutter width, recorded each render
    /// so a mouse position can be mapped back to a buffer line and column.
    pub editor_area: Option<(u16, u16, u16, u16)>,

    pub gutter_w: u16,

    /// Inner area of the file tree from the last render, for routing the mouse.
    pub tree_area: Option<(u16, u16, u16, u16)>,

    /// Set while the wheel has scrolled the view away from the caret, so the
    /// renderer stops pulling the view back until the caret moves again.
    pub scroll_free: bool,

    /// Same, for the file tree: the wheel scrolls the list without dragging the
    /// selection along, so the renderer must not chase the selection either.
    pub tree_scroll_free: bool,

    /// Row a previous click selected. Clicking it again opens it, so a stray
    /// click only moves the selection. Tracked separately from `tree.selected`
    /// because row 0 starts selected and would otherwise open on first click.
    pub tree_click: Option<usize>,

    /// System clipboard handle (`None` if the platform has none); copies also go
    /// to `clip_internal` so paste still works without a system clipboard.
    clipboard: Option<arboard::Clipboard>,

    clip_internal: String,
}

impl App {
    pub fn new(path: PathBuf, force_picker: bool) -> Result<Self> {
        let mut highlighter = SyntaxHighlighter::new();

        let is_dir = path.is_dir();

        let (buffer, media, tree_root, focus, tree_visible) = if is_dir {
            // Open on the welcome screen alone; the file browser is one Enter (or
            // Ctrl+B) away rather than taking half the screen unprompted.
            (None, None, path.clone(), Focus::Editor, false)
        } else {
            let root = buffer::parent_dir(&path);

            match media::classify(&path)? {
                Loaded::Text => {
                    let syntax = highlighter.syntax_name_for_path(&path);

                    (Some(Buffer::open(path.clone(), syntax)?), None, root, Focus::Editor, false)
                }

                Loaded::Media(m) => (None, Some(m), root, Focus::Editor, false),
            }
        };

        // Use the saved style and skip the picker, unless this is the first run
        // or the user explicitly asked to re-pick with `--style`.
        let saved = crate::config::saved_theme().and_then(|name| highlighter.theme_index(&name));

        let picker = match saved {
            Some(idx) => {
                highlighter.set_theme(idx);

                if force_picker { Some(idx) } else { None }
            }

            None => Some(highlighter.current_theme()),
        };

        Ok(Self {
            buffer,
            media,
            image_cells: None,
            media_epoch: 0,
            media_scroll: 0,
            media_info: false,
            media_rows: 1,
            tree: FileTree::new(tree_root),
            highlighter,
            focus,
            tree_visible,
            search: None,
            picker,
            status: String::new(),
            status_until: None,
            status_ok: false,
            quit_confirm: false,
            overwrite_confirm: false,
            esc_confirm: false,
            pending_open: None,
            should_quit: false,
            page_rows: 1,
            editor_area: None,
            gutter_w: 0,
            tree_area: None,
            scroll_free: false,
            tree_scroll_free: false,
            tree_click: None,
            clipboard: arboard::Clipboard::new().ok(),
            clip_internal: String::new(),
        })
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        // Any keystroke hands both views back to their cursors after a scroll,
        // and drops a half-finished click (the selection may have moved since).
        self.scroll_free = false;

        self.tree_scroll_free = false;

        self.tree_click = None;

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if self.picker.is_some() {
            self.on_picker_key(key, ctrl);

            return;
        }

        let is_quit_key = ctrl && key.code == KeyCode::Char('q');

        if !is_quit_key {
            self.quit_confirm = false;
        }

        if !(ctrl && key.code == KeyCode::Char('s')) {
            self.overwrite_confirm = false;
        }

        if key.code != KeyCode::Esc {
            self.esc_confirm = false;
        }

        if self.search.is_some() {
            self.on_search_key(key);

            return;
        }

        if ctrl {
            match key.code {
                KeyCode::Char('q') => return self.request_quit(),

                KeyCode::Char('s') => return self.save(),

                KeyCode::Char('b') => return self.toggle_tree(),

                KeyCode::Char('f') => return self.open_search(),

                KeyCode::Char('c') => return self.copy(),

                KeyCode::Char('x') => return self.cut(),

                KeyCode::Char('v') => return self.paste(),

                KeyCode::Char('a') => return self.select_all(),

                KeyCode::Char('r') => return self.reload(),

                // Toggle comment. A legacy terminal sends Ctrl+/ as byte 0x1F,
                // which crossterm cannot tell from Ctrl+7; a terminal speaking
                // the kitty keyboard protocol reports Ctrl+/ directly. Accept
                // both so the key works everywhere.
                KeyCode::Char('7') | KeyCode::Char('/') => return self.comment_toggle(),

                // Other Ctrl combos (Ctrl+arrows, Ctrl+Z/Y, …) fall through to
                // the focused pane.
                _ => {}
            }
        }

        // Esc peels back one layer at a time, then quits (search is handled
        // above): selection → file tree → quit (confirmed with a second Esc).
        if key.code == KeyCode::Esc {
            self.handle_escape();

            return;
        }

        match self.focus {
            Focus::Tree => self.on_tree_key(key),

            Focus::Editor => self.on_editor_key(key),
        }
    }

    /// True only on the empty welcome screen — no file is open, text or media.
    fn on_welcome(&self) -> bool {
        self.buffer.is_none() && self.media.is_none()
    }

    fn handle_escape(&mut self) {
        if let Some(buf) = self.buffer.as_mut() {
            if buf.selection().is_some() {
                buf.clear_selection();

                return;
            }
        }

        // Working in the code with the list still up (where a mouse click leaves
        // things): peel the sidebar away. This is a layer of its own, so it does
        // not arm the quit; the next Esc then offers the browser again exactly
        // like the keyboard path does.
        if self.tree_visible && self.focus == Focus::Editor && !self.on_welcome() {
            self.tree_visible = false;

            self.esc_confirm = false;

            self.clear_flash();

            return;
        }

        // A second Esc in a row confirms the quit, wherever focus landed (the
        // arming Esc below may have moved it into the freshly-opened tree).
        if self.esc_confirm {
            self.should_quit = true;

            return;
        }

        self.esc_confirm = true;

        let dirty = self.buffer.as_ref().map(|b| b.modified).unwrap_or(false);

        if dirty {
            self.set_error("Unsaved changes — Esc again to discard and quit".to_string());
        } else if !self.on_welcome() && !self.tree_visible {
            // A file is open (text or image/binary) with nothing to cancel: pop
            // the browser open AND focus it, so one Esc jumps to picking another
            // file. If it's already open (or we're on the welcome screen) leave
            // it untouched — just arm the quit.
            self.show_tree();

            self.set_error("Esc again to quit · or pick a file".to_string());
        } else {
            self.set_error("Esc again to quit".to_string());
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(sel) = self.picker else {
            return;
        };

        if ctrl && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c')) {
            self.should_quit = true;

            return;
        }

        let count = self.highlighter.theme_count();

        match key.code {
            KeyCode::Up => self.picker = Some(sel.saturating_sub(1)),

            KeyCode::Down => self.picker = Some((sel + 1).min(count - 1)),

            KeyCode::Enter => self.apply_style(sel),

            KeyCode::Esc | KeyCode::Char('q') => self.should_quit = true,

            _ => {}
        }
    }

    fn apply_style(&mut self, idx: usize) {
        self.highlighter.set_theme(idx);

        if let Some(name) = self.highlighter.theme_names().get(idx) {
            let _ = crate::config::save_theme(name);
        }

        if let Some(buf) = self.buffer.as_mut() {
            buf.hl.invalidate(0);
        }

        self.picker = None;
    }

    fn on_tree_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.tree.move_up(),

            KeyCode::Down => self.tree.move_down(),

            KeyCode::Left => self.tree.collapse(),

            KeyCode::Right => self.tree.expand(),

            KeyCode::Tab => self.focus = Focus::Editor,

            // Enter or Space both open a file / toggle a directory.
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_tree_entry(false),

            _ => {}
        }
    }

    /// Route a mouse event to whichever pane the pointer is over. The picker is
    /// keyboard-only, so it swallows the mouse rather than acting on stale areas.
    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if self.picker.is_some() {
            return;
        }

        // Mouse input disarms the same one-shot confirmations a keystroke does.
        // Without this an Esc armed before a click stays armed, and the next Esc
        // quits instead of re-arming.
        self.quit_confirm = false;

        self.overwrite_confirm = false;

        self.esc_confirm = false;

        let at = (ev.column, ev.row);

        if self.tree_visible && in_area(self.tree_area, at) {
            self.on_tree_mouse(ev);
        } else if in_area(self.editor_area, at) {
            self.on_editor_mouse(ev);
        }
    }

    fn on_tree_mouse(&mut self, ev: MouseEvent) {
        let Some((_, y, _, _)) = self.tree_area else {
            return;
        };

        match ev.kind {
            // The wheel scrolls the list itself and leaves the selection alone,
            // the way any file browser behaves.
            MouseEventKind::ScrollUp => self.scroll_tree(-TREE_SCROLL_STEP),

            MouseEventKind::ScrollDown => self.scroll_tree(TREE_SCROLL_STEP),

            MouseEventKind::Down(MouseButton::Left) => {
                let idx = self.tree.scroll + (ev.row - y) as usize;

                if idx < self.tree.nodes.len() {
                    self.tree.selected = idx;

                    self.focus = Focus::Tree;

                    self.scroll_free = false;

                    self.tree_scroll_free = false;

                    // Expanding a folder is cheap and reversible, so it happens on
                    // the first click. Opening a file is not, so it takes two.
                    // Either way the row indices below may shift, which retires
                    // any half-finished click aimed at the old layout.
                    if self.tree.nodes[idx].is_dir {
                        self.tree_click = None;

                        self.activate_tree_entry(true);
                    } else if self.tree_click == Some(idx) {
                        self.tree_click = None;

                        // Opened with the mouse: keep the sidebar up so the next
                        // file is a click away (the keyboard still collapses it).
                        self.activate_tree_entry(true);
                    } else {
                        // First click only moves the selection, so a misclick
                        // never opens the wrong file.
                        self.tree_click = Some(idx);
                    }
                }
            }

            _ => {}
        }
    }

    fn scroll_tree(&mut self, delta: isize) {
        let visible = self.tree_area.map(|(_, _, _, h)| h as usize).unwrap_or(1);

        self.tree.scroll_view(delta, visible);

        self.tree_scroll_free = true;
    }

    fn on_editor_mouse(&mut self, ev: MouseEvent) {
        // A media view has no caret to place, so the wheel drives the inspector
        // and the other buttons do nothing.
        if self.buffer.is_none() && self.media.is_some() {
            match ev.kind {
                MouseEventKind::ScrollUp => self.scroll_media(-EDITOR_SCROLL_STEP as i64),

                MouseEventKind::ScrollDown => self.scroll_media(EDITOR_SCROLL_STEP as i64),

                _ => {}
            }

            return;
        }

        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_view(-EDITOR_SCROLL_STEP),

            MouseEventKind::ScrollDown => self.scroll_view(EDITOR_SCROLL_STEP),

            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = Focus::Editor;

                // Shift+click extends the selection from the caret (or the
                // existing anchor) to the click; a plain click collapses it.
                let extend = ev.modifiers.contains(KeyModifiers::SHIFT);

                self.place_caret(ev.column, ev.row, extend);
            }

            // Dragging with the button held sweeps a selection from the press.
            MouseEventKind::Drag(MouseButton::Left) => self.place_caret(ev.column, ev.row, true),

            _ => {}
        }
    }

    /// Map a screen cell to a buffer position and move the caret there; with
    /// `extend` the move sweeps a selection instead of collapsing it.
    fn place_caret(&mut self, col: u16, row: u16, extend: bool) {
        let Some((x, y, _, _)) = self.editor_area else {
            return;
        };

        let gutter = self.gutter_w;

        let Some(buf) = self.buffer.as_mut() else {
            return;
        };

        let line = buf.scroll_row + (row - y) as usize;

        // A click on the gutter lands on the first visible column of that line.
        let column = buf.scroll_col + col.saturating_sub(x + gutter) as usize;

        buf.sel(extend);

        buf.move_to_pos(line, column);

        self.scroll_free = false;
    }

    /// Scroll the media inspector. The renderer clamps against the real row
    /// count, which it alone knows, so `u64::MAX` is a valid "go to the end".
    fn scroll_media(&mut self, delta: i64) {
        self.media_scroll = if delta < 0 {
            self.media_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.media_scroll.saturating_add(delta as u64)
        };
    }

    fn scroll_view(&mut self, delta: isize) {
        let Some(buf) = self.buffer.as_mut() else {
            return;
        };

        buf.scroll_view(delta);

        self.scroll_free = true;
    }

    fn on_editor_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        let alt = key.modifiers.contains(KeyModifiers::ALT);

        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // One navigation modifier per platform — Option on macOS, Ctrl on the
        // rest — so each combo maps to exactly one action (no Ctrl/Alt aliasing).
        let nav = if cfg!(target_os = "macos") { alt } else { ctrl };

        let Some(buf) = self.buffer.as_mut() else {
            match key.code {
                KeyCode::Tab if self.tree_visible => self.focus = Focus::Tree,

                // On the welcome screen, Enter opens the file browser (so does
                // Ctrl+B). Once it's open Enter does nothing here — it only acts
                // again after Ctrl+B closes it back to the welcome screen.
                // Enter opens the browser only from the welcome screen — not
                // while viewing an image / binary (also a no-buffer view).
                KeyCode::Enter if self.on_welcome() && !self.tree_visible => self.show_tree(),

                // A media view takes no typed text, so plain letters are free
                // to act as commands here.
                KeyCode::Char('i') | KeyCode::Char('I') if self.media.is_some() => {
                    self.media_info = !self.media_info;

                    self.media_scroll = 0;
                }

                KeyCode::Up => self.scroll_media(-1),

                KeyCode::Down => self.scroll_media(1),

                KeyCode::PageUp => self.scroll_media(-(self.media_rows as i64)),

                KeyCode::PageDown => self.scroll_media(self.media_rows as i64),

                KeyCode::Home => self.media_scroll = 0,

                KeyCode::End => self.media_scroll = u64::MAX,

                _ => {}
            }

            return;
        };

        // `Shift` turns any motion into a selection; without it the motion
        // collapses the selection. `nav` makes the step bigger (word / block).
        match key.code {
            KeyCode::Up => {
                buf.sel(shift);

                if nav { buf.move_para_up() } else { buf.move_up() }
            }

            KeyCode::Down => {
                buf.sel(shift);

                if nav { buf.move_para_down() } else { buf.move_down() }
            }

            KeyCode::Left => {
                buf.sel(shift);

                if nav { buf.move_word_left() } else { buf.move_left() }
            }

            KeyCode::Right => {
                buf.sel(shift);

                if nav { buf.move_word_right() } else { buf.move_right() }
            }

            KeyCode::Home => {
                buf.sel(shift);

                if ctrl { buf.move_doc_start() } else { buf.move_home() }
            }

            KeyCode::End => {
                buf.sel(shift);

                if ctrl { buf.move_doc_end() } else { buf.move_end() }
            }

            KeyCode::PageUp => {
                buf.sel(shift);

                buf.page(-(self.page_rows as isize));
            }

            KeyCode::PageDown => {
                buf.sel(shift);

                buf.page(self.page_rows as isize);
            }

            KeyCode::Enter => buf.insert_newline(),

            KeyCode::Backspace if nav => buf.delete_word_left(),

            KeyCode::Backspace => buf.backspace(),

            KeyCode::Delete if nav => buf.delete_word_right(),

            KeyCode::Delete => buf.delete(),

            // Ctrl+K rather than a Delete variant: Delete is one keystroke on a
            // PC keyboard, too easy to hit for something this destructive, and
            // overloading it would leave no plain forward delete.
            KeyCode::Char('k') | KeyCode::Char('K') if ctrl => buf.delete_line(),

            // Undo / redo (Ctrl+Z, Ctrl+Y, Ctrl+Shift+Z). macOS terminals do not
            // forward Cmd, so Ctrl is used on both platforms.
            KeyCode::Char('z') | KeyCode::Char('Z') if ctrl && shift => buf.redo(),

            KeyCode::Char('z') | KeyCode::Char('Z') if ctrl => buf.undo(),

            KeyCode::Char('y') if ctrl => buf.redo(),

            // macOS terminals with Option-as-Meta / "Natural Text Editing" send
            // ESC-b / ESC-f for Option+←/→ (and the upper-case form when Shift is
            // held). Map them to word motion so Option+arrow works there too.
            KeyCode::Char('b') if alt => {
                buf.sel(false);

                buf.move_word_left();
            }

            KeyCode::Char('B') if alt => {
                buf.sel(true);

                buf.move_word_left();
            }

            KeyCode::Char('f') if alt => {
                buf.sel(false);

                buf.move_word_right();
            }

            KeyCode::Char('F') if alt => {
                buf.sel(true);

                buf.move_word_right();
            }

            // meta-d (Option+Fn+Delete on macOS) deletes the word ahead.
            KeyCode::Char('d') if alt => buf.delete_word_right(),

            // Shift+Tab outdents the current line (Tab below indents).
            KeyCode::BackTab => buf.outdent(),

            // Always indents, sidebar open or not, to match Shift+Tab and the
            // documented behaviour. Ctrl+B is the way to reach the sidebar; Tab
            // going there would swallow indentation for as long as it is up.
            KeyCode::Tab => buf.indent(),

            KeyCode::Char(c) if !ctrl && !alt => buf.insert_char(c),

            _ => {}
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        let Some(search) = self.search.as_mut() else {
            return;
        };

        // A modified key is a command, not text. Without this Ctrl+S typed an
        // "s" into the query instead of saving, and every other Ctrl combo
        // likewise leaked its letter.
        let modified = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);

        match key.code {
            KeyCode::Esc => {
                self.search = None;

                self.clear_flash();
            }

            KeyCode::Backspace if !modified => {
                search.query.pop();

                self.drop_current_match();
            }

            KeyCode::Enter => self.find_next(),

            KeyCode::Char(c) if !modified => {
                search.query.push(c);

                self.drop_current_match();
            }

            _ => {}
        }
    }

    /// The selection marks the current match, so editing the query drops it:
    /// otherwise text that no longer matches would keep the current-match
    /// highlight until the next Enter.
    fn drop_current_match(&mut self) {
        if let Some(buf) = self.buffer.as_mut() {
            buf.clear_selection();
        }
    }

    /// Open the selected file (or expand/collapse a directory). `keep_tree`
    /// leaves the sidebar up, which is what a mouse click wants: browsing by
    /// clicking should not collapse the list you are clicking in.
    fn activate_tree_entry(&mut self, keep_tree: bool) {
        let Some(node) = self.tree.selected_node() else {
            return;
        };

        if node.is_dir {
            if node.expanded {
                self.tree.collapse();
            } else {
                self.tree.expand();
            }

            return;
        }

        let path = node.path.clone();

        let dirty = self.buffer.as_ref().map(|b| b.modified).unwrap_or(false);

        // Switching files must never silently lose edits: warn once, then let a
        // second Enter on the same file discard them (or Ctrl+S to keep them).
        if dirty && self.pending_open.as_deref() != Some(path.as_path()) {
            self.pending_open = Some(path);

            self.set_error("Unsaved changes — Ctrl+S to save, or Enter again to discard & open".to_string());

            return;
        }

        self.open_file(path, keep_tree);
    }

    fn open_file(&mut self, path: PathBuf, keep_tree: bool) {
        let loaded = match media::classify(&path) {
            Ok(Loaded::Text) => {
                let syntax = self.highlighter.syntax_name_for_path(&path);

                match Buffer::open(path, syntax) {
                    Ok(buf) => Some((Some(buf), None)),

                    Err(e) => {
                        self.set_error(format!("Error: {e}"));

                        None
                    }
                }
            }

            Ok(Loaded::Media(m)) => Some((None, Some(m))),

            Err(e) => {
                self.set_error(format!("Error: {e}"));

                None
            }
        };

        let Some((buffer, media)) = loaded else {
            return;
        };

        self.buffer = buffer;

        self.media = media;

        self.media_epoch = self.media_epoch.wrapping_add(1);

        // A new file starts at the top, showing the picture rather than the
        // metadata it was last left on.
        self.media_scroll = 0;

        self.media_info = false;

        self.focus = Focus::Editor;

        // Close the file list so the view goes full-screen; Ctrl+B brings it
        // back to pick another file. A mouse click keeps it up instead.
        if !keep_tree {
            self.tree_visible = false;
        }

        self.pending_open = None;

        self.clear_flash();
    }

    /// Where the run loop should paint the open image (only when one is shown);
    /// `None` for text, binaries, or while the tree covers the view.
    pub fn image_placement(&self) -> Option<(u64, (u16, u16, u16, u16))> {
        let Some(Media::Image(doc)) = &self.media else {
            return None;
        };

        let (x, y, w, h) = self.image_cells?;

        let (cols, rows) = doc.fitted_cells(w, h);

        // Centre it in the pane rather than pinning it to the top-left corner.
        let cx = x + w.saturating_sub(cols) / 2;

        let cy = y + h.saturating_sub(rows) / 2;

        // The epoch rides along so the run loop notices a switch between images
        // that land on the same cell box, including re-opening the same path
        // after the file changed on disk.
        Some((self.media_epoch, (cx, cy, cols, rows)))
    }

    /// kitty escape to paint the open image into a `cols`×`rows` cell box.
    pub fn kitty_image_sequence(&self, cols: u16, rows: u16) -> Vec<u8> {
        match &self.media {
            Some(Media::Image(doc)) => doc.kitty_sequence(cols, rows),

            _ => Vec::new(),
        }
    }

    fn save(&mut self) {
        let Some(buf) = self.buffer.as_mut() else {
            return;
        };

        // Guard against clobbering an external change with a single keystroke.
        if buf.disk_changed && !self.overwrite_confirm {
            self.overwrite_confirm = true;

            self.set_error(
                "⚠ Changed on disk — Ctrl+S again to overwrite, or Ctrl+R to reload".to_string(),
            );

            return;
        }

        match buf.save() {
            Ok(()) => {
                let name = buf.file_name();

                self.overwrite_confirm = false;

                self.pending_open = None;

                self.flash(format!("✓ Saved {name}"));
            }

            Err(e) => self.set_error(format!("Error: {e}")),
        }
    }

    fn reload(&mut self) {
        let Some(buf) = self.buffer.as_mut() else {
            return;
        };

        match buf.reload_from_disk() {
            Ok(()) => self.flash("↻ Reloaded from disk".to_string()),

            Err(e) => self.set_error(format!("Error: {e}")),
        }
    }

    /// Check the open file against disk (called on idle ticks). Clean buffers
    /// auto-reload; the conflict warning is derived from `buf.disk_changed`.
    fn poll_disk(&mut self) {
        let Some(buf) = self.buffer.as_mut() else {
            return;
        };

        if buf.poll_disk() == DiskEvent::Reloaded {
            self.flash("↻ Reloaded from disk".to_string());
        }
    }

    /// How long the run loop should wait before waking itself: the sooner of a
    /// fading flash and the disk-poll interval (only while a file is open).
    pub fn wake_after(&self) -> Option<Duration> {
        let mut wake: Option<Duration> = None;

        if let Some(until) = self.status_until {
            wake = Some(until.saturating_duration_since(Instant::now()));
        }

        // Poll once a second while a file is open (watch it on disk) or while the
        // tree is up (watch the folder for new / removed files).
        if self.buffer.is_some() || self.tree_visible {
            let poll = Duration::from_millis(1000);

            wake = Some(wake.map_or(poll, |w| w.min(poll)));
        }

        wake
    }

    /// Idle housekeeping done when the loop wakes without a keypress.
    pub fn tick(&mut self) {
        if let Some(until) = self.status_until {
            if Instant::now() >= until {
                self.clear_flash();
            }
        }

        self.poll_disk();

        // A refresh reorders rows as files appear or disappear, so a click armed
        // against the old list would now point at a different file.
        if self.tree_visible && self.tree.poll() {
            self.tree_click = None;
        }
    }

    fn copy(&mut self) {
        let Some(text) = self.buffer.as_ref().and_then(|b| b.selected_text()) else {
            self.set_error("Nothing selected".to_string());

            return;
        };

        self.set_clipboard(text);

        self.flash("Copied".to_string());
    }

    fn cut(&mut self) {
        let Some(buf) = self.buffer.as_mut() else {
            return;
        };

        let Some(text) = buf.selected_text() else {
            self.set_error("Nothing selected".to_string());

            return;
        };

        buf.delete_selection();

        self.set_clipboard(text);

        self.flash("Cut".to_string());
    }

    fn paste(&mut self) {
        let text = self.read_clipboard();

        if let Some(buf) = self.buffer.as_mut() {
            buf.insert_str(&text);
        }
    }

    fn select_all(&mut self) {
        if let Some(buf) = self.buffer.as_mut() {
            buf.select_all();
        }
    }

    fn comment_toggle(&mut self) {
        if let Some(buf) = self.buffer.as_mut() {
            buf.toggle_comment();
        }
    }

    fn set_clipboard(&mut self, text: String) {
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text.clone());
        }

        self.clip_internal = text;
    }

    fn read_clipboard(&mut self) -> String {
        if let Some(cb) = self.clipboard.as_mut() {
            if let Ok(text) = cb.get_text() {
                return text;
            }
        }

        self.clip_internal.clone()
    }

    /// A green success message that disappears on its own after a moment.
    fn flash(&mut self, msg: String) {
        self.status = msg;

        self.status_ok = true;

        self.status_until = Some(Instant::now() + Duration::from_millis(1500));
    }

    /// A message that stays until the next action (e.g. an error).
    fn set_error(&mut self, msg: String) {
        self.status = msg;

        self.status_ok = false;

        self.status_until = None;
    }

    pub fn clear_flash(&mut self) {
        self.status.clear();

        self.status_ok = false;

        self.status_until = None;
    }

    /// Open the file browser and focus it, re-reading the folder so newly
    /// created / deleted files show up.
    fn show_tree(&mut self) {
        self.tree.refresh();

        self.tree_visible = true;

        self.focus = Focus::Tree;
    }

    /// Ctrl+B as a three-step cycle so one key both opens the file list and
    /// returns to it: hidden → show & focus → (from editor) focus it → hide.
    fn toggle_tree(&mut self) {
        if !self.tree_visible {
            self.show_tree();
        } else if self.focus != Focus::Tree {
            self.focus = Focus::Tree;
        } else {
            self.tree_visible = false;

            self.focus = Focus::Editor;
        }
    }

    fn open_search(&mut self) {
        let Some(buf) = self.buffer.as_ref() else {
            return;
        };

        // Seed the query from the selection, the way an editor does. Only a
        // single-line selection: the query cannot contain a newline (Enter runs
        // the search), so a multi-line one could never match anything. The cap
        // keeps a stray Ctrl+A out of the status bar.
        let seed = buf
            .selected_text()
            .filter(|t| !t.contains('\n') && t.chars().count() <= SEARCH_SEED_MAX)
            .unwrap_or_default();

        self.search = Some(Search { query: seed });
    }

    fn find_next(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return;
        };

        if query.is_empty() {
            return;
        }

        let Some(buf) = self.buffer.as_mut() else {
            return;
        };

        // Start at the caret, which sits at the end of the current match, so
        // the next hit is the next non-overlapping one. Searching from caret+1
        // instead would skip a match sitting exactly at the caret.
        let found = buffer::find_next(&buf.rope, &query, buf.char_idx());

        match found {
            Some(idx) => {
                // Select the hit: it shows up as the current match, and Ctrl+C
                // then copies exactly what was found.
                buf.clear_selection();

                buf.move_to_char(idx);

                buf.sel(true);

                buf.move_to_char(idx + query.chars().count());

                self.flash(format!("Found '{query}'"));
            }

            None => self.set_error(format!("Not found: '{query}'")),
        }
    }

    fn request_quit(&mut self) {
        let dirty = self.buffer.as_ref().map(|b| b.modified).unwrap_or(false);

        if dirty && !self.quit_confirm {
            self.quit_confirm = true;

            self.set_error("Unsaved changes — Ctrl+Q again to quit, or Ctrl+S to save".to_string());
        } else {
            self.should_quit = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app_with(name: &str, text: &str) -> App {
        let path = std::env::temp_dir().join(format!("ocode_app_{name}.txt"));

        fs::write(&path, text).unwrap();

        let mut app = App::new(path, false).unwrap();

        app.picker = None;

        app
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    fn shift_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::SHIFT,
        }
    }

    /// Editor pane as the renderer would record it: full width, 3-digit gutter.
    fn with_editor_area(app: &mut App) {
        app.editor_area = Some((0, 0, 80, 10));

        app.gutter_w = 4;
    }

    #[test]
    fn mouse_click_places_the_caret() {
        let mut app = app_with("click", "fn main() {\n    let x = 1;\n    x\n}\n");

        with_editor_area(&mut app);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4 + 8, 1));

        let buf = app.buffer.as_ref().unwrap();

        assert_eq!((buf.cursor_line, buf.cursor_col), (1, 8));
    }

    #[test]
    fn mouse_click_clamps_past_the_line_end_and_in_the_gutter() {
        let mut app = app_with("clamp", "fn main() {\n    let x = 1;\n    x\n}\n");

        with_editor_area(&mut app);

        // Past the end of line 2 ("    x", 5 chars) but still inside the pane.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 60, 2));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 5, "clamped to line end");

        // Inside the gutter: lands on the first column, never underflows.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 0);
    }

    #[test]
    fn shift_click_extends_selection_from_the_caret() {
        let mut app = app_with("shiftclick", "hello world\nsecond line\n");

        with_editor_area(&mut app);

        // Plain click to drop the caret at line 0, column 2 (gutter is 4).
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4 + 2, 0));

        assert!(app.buffer.as_ref().unwrap().selection().is_none(), "a plain click has no selection");

        // Shift+click on line 1, column 5, selects from the caret to there.
        app.on_mouse(shift_click(4 + 5, 1));

        assert_eq!(
            app.buffer.as_ref().unwrap().selected_text().as_deref(),
            Some("llo world\nsecon")
        );
    }

    /// Repeated Shift+clicks pivot around the original anchor, not the previous
    /// click, the way a modern editor extends a selection.
    #[test]
    fn repeated_shift_click_keeps_the_original_anchor() {
        let mut app = app_with("shiftanchor", "hello world\nsecond line\n");

        with_editor_area(&mut app);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4 + 2, 0)); // caret at (0,2)

        app.on_mouse(shift_click(4 + 8, 0)); // extend to (0,8)

        assert_eq!(app.buffer.as_ref().unwrap().selected_text().as_deref(), Some("llo wo"));

        app.on_mouse(shift_click(4 + 3, 1)); // extend again, still from (0,2)

        assert_eq!(
            app.buffer.as_ref().unwrap().selected_text().as_deref(),
            Some("llo world\nsec"),
            "the anchor stayed at the first caret, it did not jump to the last click"
        );
    }

    #[test]
    fn mouse_drag_selects_from_the_press() {
        let mut app = app_with("drag", "hello world\n");

        with_editor_area(&mut app);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 0));

        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4 + 5, 0));

        assert_eq!(
            app.buffer.as_ref().unwrap().selected_text().as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn wheel_scrolls_the_view_without_moving_the_caret() {
        let text: String = (0..50).map(|i| format!("line {i}\n")).collect();

        let mut app = app_with("wheel", &text);

        with_editor_area(&mut app);

        app.on_mouse(mouse(MouseEventKind::ScrollDown, 10, 5));

        {
            let buf = app.buffer.as_ref().unwrap();

            assert_eq!(buf.scroll_row, EDITOR_SCROLL_STEP as usize);

            assert_eq!(buf.cursor_line, 0, "the wheel must not drag the caret");
        }

        assert!(app.scroll_free);

        app.on_key(KeyEvent::from(KeyCode::Right));

        assert!(!app.scroll_free, "a keystroke hands the view back to the caret");
    }

    #[test]
    fn wheel_up_stops_at_the_top() {
        let mut app = app_with("wheel_top", "a\nb\nc\n");

        with_editor_area(&mut app);

        app.on_mouse(mouse(MouseEventKind::ScrollUp, 10, 1));

        assert_eq!(app.buffer.as_ref().unwrap().scroll_row, 0);
    }

    /// Esc arms the quit, then a click must disarm it: the reported bug was Esc
    /// (which pops the browser) then clicking a file then Esc quitting outright.
    #[test]
    fn a_click_disarms_a_pending_esc_quit() {
        let dir = std::env::temp_dir().join("ocode_esc_click");

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("a.txt"), "hello\n").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter)); // welcome -> browser

        app.tree_area = Some((0, 0, 32, 10));

        // Arm the quit with one Esc.
        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(app.esc_confirm, "first Esc arms the quit");

        // Click a file in the sidebar (select, then open).
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert!(!app.esc_confirm, "a click must disarm the pending quit");

        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(!app.should_quit, "Esc after a click re-arms instead of quitting");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn mouse_open_keeps_the_sidebar_but_the_keyboard_still_closes_it() {
        let dir = std::env::temp_dir().join("ocode_keep_sidebar");

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("a.txt"), "hello\n").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert!(app.buffer.is_none(), "the first click only selects");

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert!(app.buffer.is_some(), "the second click opened the file");

        assert!(app.tree_visible, "a mouse click keeps the sidebar up");

        // The keyboard path keeps the old behavior.
        app.focus = Focus::Tree;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        assert!(!app.tree_visible, "Enter still collapses the sidebar");

        let _ = fs::remove_dir_all(dir);
    }

    /// A stray click must never open a file: the first lands the selection, only
    /// a second click on that same row opens it.
    #[test]
    fn first_tree_click_selects_and_only_the_second_opens() {
        let dir = std::env::temp_dir().join("ocode_two_step_click");

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("a.txt"), "aaa\n").unwrap();

        fs::write(dir.join("b.txt"), "bbb\n").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        // Row 0 starts selected, so this is the case that used to open at once.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert!(app.buffer.is_none(), "first click on the pre-selected row selects only");

        assert_eq!(app.tree.selected, 0);

        // Clicking a different row moves the selection instead of opening.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1));

        assert!(app.buffer.is_none(), "moving to another row must not open it");

        assert_eq!(app.tree.selected, 1);

        // Second click on that row opens it.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1));

        assert!(app.buffer.is_some(), "second click on the same row opens");

        let _ = fs::remove_dir_all(dir);
    }

    /// Esc peels the sidebar away when the user is in the code, and the ladder
    /// then rejoins the keyboard path: the next Esc offers the browser back, and
    /// only the one after that quits.
    #[test]
    fn esc_peels_the_sidebar_then_reoffers_it_before_quitting() {
        let dir = std::env::temp_dir().join("ocode_esc_peel");

        let _ = fs::remove_dir_all(&dir);

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("a.txt"), "hello\n").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        // Open it with the mouse: sidebar stays up, focus lands in the code.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert!(app.tree_visible && app.focus == Focus::Editor);

        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(!app.tree_visible, "Esc hides the sidebar");

        assert!(!app.should_quit, "and does not quit yet");

        assert!(!app.esc_confirm, "peeling must not leave the quit armed");

        // Back on the keyboard ladder: this Esc offers the browser again.
        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(app.tree_visible, "the next Esc brings the browser back");

        assert_eq!(app.focus, Focus::Tree, "and focuses it");

        assert!(!app.should_quit, "still not quitting");

        // Only now does Esc quit.
        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(app.should_quit, "the third Esc quits");

        let _ = fs::remove_dir_all(dir);
    }

    /// Keyboard-only path is untouched: with focus in the tree, Esc still just
    /// arms the quit and leaves the sidebar alone.
    #[test]
    fn esc_with_focus_in_the_tree_keeps_the_old_behavior() {
        let dir = std::env::temp_dir().join("ocode_esc_tree_focus");

        let _ = fs::remove_dir_all(&dir);

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("a.txt"), "hello\n").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter)); // browser, focus = Tree

        app.on_key(KeyEvent::from(KeyCode::Enter)); // open the file (hides tree)

        assert!(!app.tree_visible, "the keyboard still collapses the sidebar");

        // Esc with a file open and no sidebar: unchanged, it pops the browser.
        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(app.tree_visible && app.focus == Focus::Tree, "Esc opens the browser");

        assert!(app.esc_confirm);

        // Focus is in the tree, so the next Esc quits exactly as before.
        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(app.should_quit);

        let _ = fs::remove_dir_all(dir);
    }

    /// Tab used to jump to the sidebar whenever it was open, so a mouse-opened
    /// file (which keeps the sidebar up) could not be indented at all, while
    /// Shift+Tab still outdented.
    #[test]
    fn tab_indents_even_with_the_sidebar_open() {
        let dir = std::env::temp_dir().join("ocode_tab_sidebar");

        let _ = fs::remove_dir_all(&dir);

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("a.txt"), "code\n").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        // Two clicks to open: the sidebar stays up, focus lands in the code.
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert!(app.tree_visible && app.focus == Focus::Editor);

        app.on_key(plain(KeyCode::Tab));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "    code\n",
            "Tab indents instead of jumping to the sidebar"
        );

        assert_eq!(app.focus, Focus::Editor, "focus stays in the code");

        let _ = fs::remove_dir_all(dir);
    }

    /// Tab / Shift+Tab over a selection must move every line it covers, keep
    /// the selection on the same text so it can be repeated, and undo as one.
    #[test]
    fn tab_indents_every_selected_line() {
        let mut app = app_with("multiindent", "one\ntwo\nthree\n");

        // Select from the start of line 0 down into line 2.
        app.on_key(shift(KeyCode::Down));

        app.on_key(shift(KeyCode::Down));

        app.on_key(shift(KeyCode::Right));

        app.on_key(plain(KeyCode::Tab));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "    one\n    two\n    three\n",
            "every covered line moved"
        );

        assert!(
            app.buffer.as_ref().unwrap().selection().is_some(),
            "selection survives so Tab can be pressed again"
        );

        // Repeatable.
        app.on_key(plain(KeyCode::Tab));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "        one\n        two\n        three\n"
        );

        // Shift+Tab walks it back.
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "    one\n    two\n    three\n"
        );

        // Each press is a single undo step.
        app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "        one\n        two\n        three\n"
        );
    }

    /// A selection ending at column 0 must not drag the next line in.
    #[test]
    fn selection_ending_at_a_line_start_does_not_indent_that_line() {
        let mut app = app_with("indentedge", "one\ntwo\nthree\n");

        // Select exactly line 0, ending at the start of line 1.
        app.on_key(shift(KeyCode::Down));

        app.on_key(plain(KeyCode::Tab));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "    one\ntwo\nthree\n",
            "only the line actually covered moved"
        );
    }

    #[test]
    fn outdent_on_unindented_selection_does_nothing() {
        let mut app = app_with("outdentnoop", "one\ntwo\n");

        app.on_key(shift(KeyCode::Down));

        app.on_key(shift(KeyCode::Right));

        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "one\ntwo\n");

        assert!(!app.buffer.as_ref().unwrap().modified, "no edit, so nothing to save");
    }

    /// Ctrl+/ arrives as Ctrl+7 on a legacy terminal and as Ctrl+/ under the
    /// kitty protocol; both must toggle the comment.
    #[test]
    fn ctrl_f_seeds_the_query_from_the_selection() {
        let mut app = app_with("findseed", "alpha beta\ngamma\n");

        // Select "alpha".
        for _ in 0..5 {
            app.on_key(shift(KeyCode::Right));
        }

        app.on_key(ctrl(KeyCode::Char('f')));

        assert_eq!(app.search.as_ref().map(|s| s.query.as_str()), Some("alpha"));
    }

    /// The query can never contain a newline (Enter runs the search), so a
    /// multi-line selection could not match anything and must not seed it.
    #[test]
    fn ctrl_f_ignores_a_multi_line_selection() {
        let mut app = app_with("findmulti", "alpha\nbeta\n");

        app.on_key(shift(KeyCode::Down));

        app.on_key(shift(KeyCode::Right));

        assert!(app.buffer.as_ref().unwrap().selected_text().unwrap().contains('\n'));

        app.on_key(ctrl(KeyCode::Char('f')));

        assert_eq!(app.search.as_ref().map(|s| s.query.as_str()), Some(""));
    }

    /// Select-all on a large file must not push the whole buffer into the
    /// status bar.
    #[test]
    fn ctrl_f_ignores_an_oversized_selection() {
        let long: String = "x".repeat(SEARCH_SEED_MAX + 1);

        let mut app = app_with("findlong", &format!("{long}\n"));

        app.on_key(ctrl(KeyCode::Char('a')));

        app.on_key(ctrl(KeyCode::Char('f')));

        assert_eq!(app.search.as_ref().map(|s| s.query.as_str()), Some(""));
    }

    #[test]
    fn enter_selects_the_match_and_advances_through_hits() {
        let mut app = app_with("findnav", "foo bar\nbaz foo\nfoo end\n");

        app.on_key(ctrl(KeyCode::Char('f')));

        for c in "foo".chars() {
            app.on_key(plain(KeyCode::Char(c)));
        }

        // First Enter takes the match at the caret (line 0, col 0).
        app.on_key(plain(KeyCode::Enter));

        {
            let buf = app.buffer.as_ref().unwrap();

            assert_eq!(buf.selected_text().as_deref(), Some("foo"), "the hit is selected");

            assert_eq!((buf.cursor_line, buf.cursor_col), (0, 3), "caret sits after the hit");
        }

        app.on_key(plain(KeyCode::Enter));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_line, 1, "second hit is on line 1");

        app.on_key(plain(KeyCode::Enter));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_line, 2, "third hit is on line 2");

        // Past the last hit it wraps back to the first.
        app.on_key(plain(KeyCode::Enter));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_line, 0, "wraps to the top");
    }

    /// Overlapping text must advance rather than re-select the same hit, which
    /// would make Enter appear stuck.
    #[test]
    fn enter_advances_through_overlapping_text() {
        let mut app = app_with("findoverlap", "aaaa\n");

        app.on_key(ctrl(KeyCode::Char('f')));

        app.on_key(plain(KeyCode::Char('a')));

        app.on_key(plain(KeyCode::Char('a')));

        app.on_key(plain(KeyCode::Enter));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 2);

        app.on_key(plain(KeyCode::Enter));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 4, "moved on, not stuck");
    }

    /// While the find bar is open a modified key is a command, not text. This
    /// used to type the letter into the query, so Ctrl+S wrote "s" instead of
    /// saving.
    #[test]
    fn modified_keys_do_not_leak_into_the_query() {
        let mut app = app_with("findmod", "hello\n");

        app.on_key(ctrl(KeyCode::Char('f')));

        app.on_key(ctrl(KeyCode::Char('s')));

        app.on_key(ctrl(KeyCode::Char('c')));

        app.on_key(nav(KeyCode::Char('d')));

        assert_eq!(
            app.search.as_ref().map(|s| s.query.as_str()),
            Some(""),
            "no modified key reached the query"
        );
    }

    #[test]
    fn editing_the_query_drops_the_current_match() {
        let mut app = app_with("findedit", "foo bar\n");

        app.on_key(ctrl(KeyCode::Char('f')));

        for c in "foo".chars() {
            app.on_key(plain(KeyCode::Char(c)));
        }

        app.on_key(plain(KeyCode::Enter));

        assert!(app.buffer.as_ref().unwrap().selection().is_some());

        // Editing the query means the old hit is no longer the current match.
        app.on_key(plain(KeyCode::Backspace));

        assert!(
            app.buffer.as_ref().unwrap().selection().is_none(),
            "the stale current-match highlight is cleared"
        );
    }

    #[test]
    fn enter_on_an_empty_query_does_nothing() {
        let mut app = app_with("findempty", "hello\n");

        app.on_key(ctrl(KeyCode::Char('f')));

        app.on_key(plain(KeyCode::Enter));

        let buf = app.buffer.as_ref().unwrap();

        assert!(buf.selection().is_none());

        assert_eq!(buf.cursor_col, 0);

        assert!(app.status.is_empty(), "no 'not found' noise for an empty query");
    }

    #[test]
    fn ctrl_slash_toggles_line_comment() {
        let path = std::env::temp_dir().join("ocode_toggle_comment.rs");

        fs::write(&path, "let x = 1;\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "// let x = 1;\n",
            "Ctrl+7 (legacy Ctrl+/) comments"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL));

        assert_eq!(
            app.buffer.as_ref().unwrap().rope.to_string(),
            "let x = 1;\n",
            "Ctrl+/ (kitty protocol) uncomments"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn ctrl_k_deletes_the_whole_line() {
        let mut app = app_with("killline", "one\ntwo\nthree\n");

        // Caret on line 1 ("two"), somewhere in the middle.
        app.on_key(KeyEvent::from(KeyCode::Down));

        app.on_key(KeyEvent::from(KeyCode::Right));

        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));

        {
            let buf = app.buffer.as_ref().unwrap();

            assert_eq!(buf.rope.to_string(), "one\nthree\n", "the line is gone entirely");

            assert_eq!(
                (buf.cursor_line, buf.cursor_col),
                (1, 0),
                "caret lands on the line that moved up"
            );
        }

        // Undone as a single step.
        app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "one\ntwo\nthree\n");
    }

    /// The last line has no trailing break, so the one before it has to go or a
    /// blank line would be left behind.
    #[test]
    fn ctrl_k_on_the_last_line_leaves_no_blank() {
        let mut app = app_with("killlast", "one\ntwo");

        app.on_key(KeyEvent::from(KeyCode::Down));

        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));

        let buf = app.buffer.as_ref().unwrap();

        assert_eq!(buf.rope.to_string(), "one");

        assert_eq!(buf.last_line(), 0, "no empty line left over");
    }

    #[test]
    fn ctrl_k_on_the_only_line_empties_it() {
        let mut app = app_with("killonly", "solo");

        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "");
    }

    #[test]
    fn a_folder_expands_on_a_single_click() {
        let dir = std::env::temp_dir().join("ocode_folder_click");

        let _ = fs::remove_dir_all(&dir);

        let _ = fs::create_dir_all(dir.join("sub"));

        fs::write(dir.join("sub").join("inner.txt"), "x").unwrap();

        fs::write(dir.join("a.txt"), "a").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        let row = app.tree.nodes.iter().position(|n| n.is_dir).expect("a directory row");

        assert!(!app.tree.nodes[row].expanded, "starts collapsed");

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, row as u16));

        assert!(app.tree.nodes[row].expanded, "one click expands a folder");

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, row as u16));

        assert!(!app.tree.nodes[row].expanded, "clicking it again collapses");

        let _ = fs::remove_dir_all(dir);
    }

    /// Expanding a folder inserts rows, so a file click armed against the old
    /// layout must not survive to open whatever now sits on that row.
    #[test]
    fn expanding_a_folder_drops_a_half_finished_file_click() {
        let dir = std::env::temp_dir().join("ocode_click_shift");

        let _ = fs::remove_dir_all(&dir);

        let _ = fs::create_dir_all(dir.join("sub"));

        fs::write(dir.join("sub").join("inner.txt"), "x").unwrap();

        fs::write(dir.join("a.txt"), "a").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        let file_row = app.tree.nodes.iter().position(|n| !n.is_dir).expect("a file row");

        let dir_row = app.tree.nodes.iter().position(|n| n.is_dir).expect("a directory row");

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, file_row as u16));

        assert_eq!(app.tree_click, Some(file_row), "file click armed");

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, dir_row as u16));

        assert_eq!(app.tree_click, None, "expanding retires the armed click");

        assert!(app.buffer.is_none(), "and nothing was opened");

        let _ = fs::remove_dir_all(dir);
    }

    /// Keyboard use must re-arm the two-step, otherwise a single later click
    /// could open a row the keyboard had moved away from.
    #[test]
    fn a_keystroke_cancels_a_half_finished_click() {
        let dir = std::env::temp_dir().join("ocode_click_rearm");

        let _ = fs::create_dir_all(&dir);

        fs::write(dir.join("a.txt"), "aaa\n").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert_eq!(app.tree_click, Some(0), "click armed the row");

        app.on_key(KeyEvent::from(KeyCode::Down));

        assert_eq!(app.tree_click, None, "a keystroke disarms it");

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));

        assert!(app.buffer.is_none(), "the click must select again, not open");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tree_wheel_scrolls_the_list_without_moving_the_selection() {
        let dir = std::env::temp_dir().join("ocode_tree_wheel");

        let _ = fs::create_dir_all(&dir);

        for i in 0..30 {
            fs::write(dir.join(format!("f{i:02}.txt")), "x").unwrap();
        }

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.tree_area = Some((0, 0, 32, 10));

        let selected = app.tree.selected;

        app.on_mouse(mouse(MouseEventKind::ScrollDown, 2, 5));

        assert_eq!(app.tree.scroll, TREE_SCROLL_STEP as usize, "one row per event");

        assert_eq!(app.tree.selected, selected, "the wheel must not move the selection");

        assert!(app.tree_scroll_free);

        // Scrolling up stops at the top rather than underflowing.
        for _ in 0..10 {
            app.on_mouse(mouse(MouseEventKind::ScrollUp, 2, 5));
        }

        assert_eq!(app.tree.scroll, 0);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn mouse_outside_the_panes_is_ignored() {
        let mut app = app_with("outside", "abc\n");

        app.editor_area = Some((0, 0, 10, 3));

        app.gutter_w = 4;

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 50, 50));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 0);
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// The platform navigation modifier the editor actually listens to.
    fn nav(code: KeyCode) -> KeyEvent {
        let m = if cfg!(target_os = "macos") {
            KeyModifiers::ALT
        } else {
            KeyModifiers::CONTROL
        };

        KeyEvent::new(code, m)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn meta_b_f_jump_by_word() {
        // What macOS terminals send for Option+←/→ (Option-as-Meta).
        let mut app = app_with("metaword", "foo bar");

        app.on_key(alt(KeyCode::Char('f')));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 3);

        app.on_key(alt(KeyCode::Char('f')));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 7);

        app.on_key(alt(KeyCode::Char('b')));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 4);

        assert!(!app.buffer.as_ref().unwrap().modified, "meta motion must not type letters");
    }

    #[test]
    fn delete_word_forward_keys() {
        // meta-d (Option+Fn+Delete) and nav+Delete both delete the word ahead.
        let mut app = app_with("delfwd1", "foo bar");

        app.on_key(alt(KeyCode::Char('d')));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), " bar");

        let mut app2 = app_with("delfwd2", "foo bar");

        app2.on_key(nav(KeyCode::Delete));

        assert_eq!(app2.buffer.as_ref().unwrap().rope.to_string(), " bar");
    }

    #[test]
    fn typing_inserts_and_marks_modified() {
        let mut app = app_with("type", "ab cd");

        app.on_key(plain(KeyCode::Char('X')));

        let buf = app.buffer.as_ref().unwrap();

        assert!(buf.modified);

        assert_eq!(buf.rope.to_string(), "Xab cd");
    }

    #[test]
    fn ctrl_letter_command_does_not_type_text() {
        let mut app = app_with("ctrlletter", "ab");

        // An unbound Ctrl+letter must never insert that letter.
        app.on_key(ctrl(KeyCode::Char('g')));

        let buf = app.buffer.as_ref().unwrap();

        assert!(!buf.modified);

        assert_eq!(buf.rope.to_string(), "ab");
    }

    #[test]
    fn home_end_are_line_motions() {
        let mut app = app_with("linemo", "hello world");

        app.on_key(plain(KeyCode::End));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 11);

        app.on_key(plain(KeyCode::Home));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 0);
    }

    #[test]
    fn ctrl_a_selects_all() {
        let mut app = app_with("selectall", "line one\nline two\n");

        app.on_key(ctrl(KeyCode::Char('a')));

        assert_eq!(
            app.buffer.as_ref().unwrap().selected_text().as_deref(),
            Some("line one\nline two\n")
        );
    }

    #[test]
    fn nav_up_reaches_doc_top_without_fn() {
        let mut app = app_with("doctop", "one\ntwo\nthree");

        app.buffer.as_mut().unwrap().cursor_line = 2;

        app.on_key(nav(KeyCode::Up)); // block jump with no blank line lands at top

        assert_eq!(app.buffer.as_ref().unwrap().cursor_line, 0);
    }

    #[test]
    fn nav_right_jumps_by_word() {
        let mut app = app_with("navword", "foo bar");

        app.on_key(nav(KeyCode::Right));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_col, 3);
    }

    #[test]
    fn nav_up_down_jumps_blocks() {
        let mut app = app_with("blocks", "a\nb\n\nc\n");

        app.on_key(nav(KeyCode::Down));

        assert_eq!(app.buffer.as_ref().unwrap().cursor_line, 2);
    }

    #[test]
    fn ctrl_q_guards_quit_when_modified() {
        let mut app = app_with("guard", "data");

        app.on_key(plain(KeyCode::Char('!')));

        app.on_key(ctrl(KeyCode::Char('q')));

        assert!(!app.should_quit);

        assert!(app.quit_confirm);

        app.on_key(ctrl(KeyCode::Char('q')));

        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_q_quits_immediately_when_clean() {
        let mut app = app_with("clean", "data");

        app.on_key(ctrl(KeyCode::Char('q')));

        assert!(app.should_quit);
    }

    #[test]
    fn esc_clears_selection_then_quits_on_second_press() {
        let mut app = app_with("esc", "hello");

        app.on_key(shift(KeyCode::Right)); // make a selection

        app.on_key(plain(KeyCode::Esc)); // peels off the selection

        assert!(app.buffer.as_ref().unwrap().selection().is_none());

        assert!(!app.should_quit);

        app.on_key(plain(KeyCode::Esc)); // nothing left → arm quit

        assert!(app.esc_confirm && !app.should_quit);

        app.on_key(plain(KeyCode::Esc)); // confirm → quit

        assert!(app.should_quit);
    }

    #[test]
    fn esc_when_clean_arms_opens_and_focuses_file_browser() {
        let mut app = app_with("escclean", "hello");

        assert!(!app.tree_visible);

        app.on_key(plain(KeyCode::Esc)); // clean → arm quit, open AND focus the tree

        assert!(app.esc_confirm);

        assert!(app.tree_visible, "first Esc should open the file browser");

        assert_eq!(app.focus, Focus::Tree, "and focus it, ready to pick a file");

        assert!(!app.should_quit);

        app.on_key(plain(KeyCode::Esc)); // second Esc quits even from the tree

        assert!(app.should_quit);
    }

    #[test]
    fn esc_when_dirty_arms_without_opening_browser() {
        let mut app = app_with("escdirty", "hello");

        app.on_key(plain(KeyCode::Char('x'))); // make an unsaved edit

        assert!(app.buffer.as_ref().unwrap().modified);

        app.on_key(plain(KeyCode::Esc)); // dirty → just warn, no browser

        assert!(app.esc_confirm);

        assert!(!app.tree_visible, "dirty Esc must not pop the browser");

        assert!(!app.should_quit);
    }

    #[test]
    fn esc_quit_arming_resets_on_another_key() {
        let mut app = app_with("escreset", "hi");

        app.on_key(plain(KeyCode::Esc)); // arm

        assert!(app.esc_confirm);

        app.on_key(plain(KeyCode::Right)); // any other key cancels the arming

        assert!(!app.esc_confirm);

        app.on_key(plain(KeyCode::Esc)); // arms again, does not quit

        assert!(!app.should_quit);
    }

    fn app_in_dir(name: &str) -> (App, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ocode_dir_{name}"));

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("a.txt"), "x").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        (app, dir)
    }

    #[test]
    fn welcome_starts_without_sidebar_and_enter_opens_it() {
        let (mut app, dir) = app_in_dir("welcome");

        assert!(app.buffer.is_none() && !app.tree_visible, "starts on the welcome screen alone");

        assert_eq!(app.focus, Focus::Editor);

        app.on_key(plain(KeyCode::Enter)); // Enter from welcome opens + focuses the browser

        assert!(app.tree_visible);

        assert_eq!(app.focus, Focus::Tree);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn esc_on_welcome_quits_without_opening_sidebar() {
        let (mut app, dir) = app_in_dir("escwelcome");

        app.on_key(plain(KeyCode::Esc)); // arm, but no file open → don't pop the sidebar

        assert!(app.esc_confirm);

        assert!(!app.tree_visible, "Esc on the welcome screen must not open the sidebar");

        app.on_key(plain(KeyCode::Esc));

        assert!(app.should_quit);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn esc_with_sidebar_open_arms_then_quits_without_focus_bounce() {
        let (mut app, dir) = app_in_dir("escsidebar");

        app.on_key(plain(KeyCode::Enter)); // open + focus the sidebar

        assert!(app.tree_visible && app.focus == Focus::Tree);

        app.on_key(plain(KeyCode::Esc)); // already open → just arm, no re-show / no bounce

        assert!(app.esc_confirm);

        assert_eq!(app.focus, Focus::Tree, "focus stays on the sidebar");

        app.on_key(plain(KeyCode::Esc));

        assert!(app.should_quit);

        let _ = fs::remove_dir_all(dir);
    }

    fn app_with_binary(name: &str) -> (App, PathBuf) {
        let path = std::env::temp_dir().join(format!("ocode_bin_{name}.pdf"));

        fs::write(&path, b"%PDF-1.4\n\x00\x01\x02binary").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        (app, path)
    }

    #[test]
    fn esc_on_a_media_view_opens_and_focuses_the_sidebar() {
        let (mut app, path) = app_with_binary("esc");

        assert!(app.media.is_some() && app.buffer.is_none(), "binary opens as a media view");

        app.on_key(plain(KeyCode::Esc)); // a file is open → pop + focus the browser

        assert!(app.tree_visible, "Esc on a binary/image view must open the sidebar");

        assert_eq!(app.focus, Focus::Tree);

        app.on_key(plain(KeyCode::Esc));

        assert!(app.should_quit);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enter_on_a_media_view_does_not_open_the_sidebar() {
        let (mut app, path) = app_with_binary("enter");

        app.on_key(plain(KeyCode::Enter)); // not the welcome screen → Enter is inert here

        assert!(!app.tree_visible, "Enter must not open the sidebar from a media view");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn shift_arrow_selects_and_ctrl_c_copies() {
        let mut app = app_with("selcopy", "hello world");

        app.clipboard = None; // use the internal clipboard; don't touch the OS one

        app.on_key(shift(KeyCode::Right));

        app.on_key(shift(KeyCode::Right));

        let buf = app.buffer.as_ref().unwrap();

        assert_eq!(buf.selected_text().as_deref(), Some("he"));

        app.on_key(ctrl(KeyCode::Char('c'))); // copy (no longer quit)

        assert!(!app.should_quit, "Ctrl+C must copy, not quit");

        assert_eq!(app.clip_internal, "he");
    }

    #[test]
    fn typing_over_selection_replaces_it() {
        let mut app = app_with("replace", "abcde");

        app.on_key(shift(KeyCode::Right));

        app.on_key(shift(KeyCode::Right));

        app.on_key(plain(KeyCode::Char('X')));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "Xcde");
    }

    #[test]
    fn cut_then_paste_round_trips() {
        let mut app = app_with("cutpaste", "abcde");

        app.clipboard = None; // deterministic: round-trip through the internal clipboard

        app.on_key(shift(KeyCode::Right));

        app.on_key(shift(KeyCode::Right)); // select "ab"

        app.on_key(ctrl(KeyCode::Char('x'))); // cut -> "cde"

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "cde");

        app.on_key(plain(KeyCode::End)); // move to end of line

        app.on_key(ctrl(KeyCode::Char('v'))); // paste "ab"

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "cdeab");
    }

    #[test]
    fn ctrl_b_cycles_tree_open_focus_hide() {
        let mut app = app_with("tree", "x");

        assert!(!app.tree_visible);

        app.on_key(ctrl(KeyCode::Char('b')));

        assert!(app.tree_visible && app.focus == Focus::Tree);

        app.focus = Focus::Editor;

        app.on_key(ctrl(KeyCode::Char('b')));

        assert!(app.tree_visible && app.focus == Focus::Tree);

        app.on_key(ctrl(KeyCode::Char('b')));

        assert!(!app.tree_visible && app.focus == Focus::Editor);
    }

    #[test]
    fn tab_indents_backtab_outdents() {
        let mut app = app_with("indentkey", "foo");

        app.on_key(plain(KeyCode::Tab));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "    foo");

        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "foo");
    }

    #[test]
    fn shift_arrow_grows_selection_plain_arrow_clears_it() {
        let mut app = app_with("selclear", "hello");

        app.on_key(shift(KeyCode::Right));

        app.on_key(shift(KeyCode::Right));

        assert_eq!(app.buffer.as_ref().unwrap().selected_text().as_deref(), Some("he"));

        app.on_key(plain(KeyCode::Right)); // a plain move collapses the selection

        assert!(app.buffer.as_ref().unwrap().selection().is_none());
    }

    #[test]
    fn plain_angle_bracket_is_inserted() {
        let mut app = app_with("angle", "");

        app.on_key(plain(KeyCode::Char('>')));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), ">");
    }

    #[test]
    fn switching_files_warns_then_discards() {
        let dir = std::env::temp_dir().join("ocode_switch_test");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("a.txt"), "aaa").unwrap();

        fs::write(dir.join("b.txt"), "bbb").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(plain(KeyCode::Enter)); // welcome → open the file browser

        app.on_key(plain(KeyCode::Enter)); // open a.txt (selected first)

        app.on_key(plain(KeyCode::Char('X'))); // make it dirty

        assert!(app.buffer.as_ref().unwrap().modified);

        app.on_key(ctrl(KeyCode::Char('b'))); // focus the tree

        app.on_key(plain(KeyCode::Down)); // select b.txt

        app.on_key(plain(KeyCode::Enter)); // first Enter: warn, do not switch

        assert!(app.buffer.as_ref().unwrap().rope.to_string().contains("aaa"));

        assert!(app.status.contains("Unsaved"));

        app.on_key(plain(KeyCode::Enter)); // second Enter: discard & open b

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "bbb");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn space_opens_file_in_tree() {
        let dir = std::env::temp_dir().join("ocode_space_test");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("a.txt"), "hello").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        assert!(app.buffer.is_none());

        app.on_key(plain(KeyCode::Enter)); // welcome → open the file browser

        app.on_key(plain(KeyCode::Char(' '))); // Space opens the selected file

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "hello");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn opening_a_file_from_tree_hides_it() {
        let dir = std::env::temp_dir().join("ocode_hidetree_test");

        let _ = fs::remove_dir_all(&dir);

        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("a.txt"), "hi").unwrap();

        let mut app = App::new(dir.clone(), false).unwrap();

        app.picker = None;

        app.on_key(plain(KeyCode::Enter)); // welcome → open the file browser

        assert!(app.tree_visible, "Enter opens the tree from the welcome screen");

        app.on_key(plain(KeyCode::Enter)); // open a.txt from the tree

        assert!(!app.tree_visible, "tree closes after opening a file");

        assert!(app.buffer.is_some());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_conflict_needs_a_second_ctrl_s() {
        let path = std::env::temp_dir().join("ocode_app_conflict.txt");

        fs::write(&path, "v1\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        app.on_key(plain(KeyCode::Char('X'))); // dirty: "Xv1\n"

        fs::write(&path, "v2\n").unwrap(); // external change

        app.buffer.as_mut().unwrap().disk_changed = true; // (detection covered by buffer tests)

        app.on_key(ctrl(KeyCode::Char('s'))); // refused, arms overwrite

        assert!(app.overwrite_confirm);

        assert_eq!(fs::read_to_string(&path).unwrap(), "v2\n"); // not written yet

        app.on_key(ctrl(KeyCode::Char('s'))); // overwrite

        assert!(!app.buffer.as_ref().unwrap().disk_changed);

        assert_eq!(fs::read_to_string(&path).unwrap(), "Xv1\n");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn ctrl_r_reloads_from_disk() {
        let path = std::env::temp_dir().join("ocode_app_reload.txt");

        fs::write(&path, "v1\n").unwrap();

        let mut app = App::new(path.clone(), false).unwrap();

        app.picker = None;

        app.on_key(plain(KeyCode::Char('X')));

        fs::write(&path, "v2\n").unwrap();

        app.buffer.as_mut().unwrap().disk_changed = true;

        app.on_key(ctrl(KeyCode::Char('r'))); // reload from disk

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "v2\n");

        assert!(!app.buffer.as_ref().unwrap().disk_changed);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn ctrl_z_undoes_and_ctrl_y_redoes() {
        let mut app = app_with("undokey", "");

        app.on_key(plain(KeyCode::Char('h')));

        app.on_key(plain(KeyCode::Char('i')));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "hi");

        app.on_key(ctrl(KeyCode::Char('z')));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "");

        app.on_key(ctrl(KeyCode::Char('y')));

        assert_eq!(app.buffer.as_ref().unwrap().rope.to_string(), "hi");
    }
}
