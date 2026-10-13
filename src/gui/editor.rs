//! The code editor: egui's `TextEdit` plus line numbers, problem marks,
//! editing commands, find/replace, TextMate-style column selection (one caret
//! per line) and an autocomplete popup.
//!
//! Commands: Enter keeps the indentation (and opens a block after `{`),
//! Tab/Shift+Tab indent and outdent the selected lines, Ctrl+/ toggles
//! comments, Ctrl+D duplicates lines, Alt+↑/↓ moves them, Ctrl+Shift+K
//! deletes them. Ctrl+F finds and replaces; F3/Shift+F3 step through matches.
//! In the DSL, Ctrl+click on a table or `table.column` jumps to it.
//!
//! Column selection: Alt+drag selects a rectangle; tapping Alt turns a
//! multi-line selection into a rectangle; Alt+Shift+↑/↓ adds a caret on the
//! line above/below. Typing, Backspace/Delete, ←/→, Home/End, copy, cut and
//! paste then act on every line. Esc, a plain click or any other key goes back
//! to a single caret.
//!
//! Autocomplete opens while typing a word, after `table.` and on Ctrl+Space.
//! ↑/↓ choose, Enter/Tab accept, Esc closes.

use crate::highlight::{self, DBS_TYPES, Lang, SQL_KEYWORDS};
use eframe::egui::{
    self, Align, Align2, Color32, Event, FontId, ImeEvent, Galley, Key, Modifiers, Pos2, Rect, RichText, Shape, Stroke, vec2,
    text::{CCursor, CCursorRange},
};
use simple_database::parser;
use simple_database::schema::Schema;
use std::sync::Arc;

/// Names offered by autocomplete besides keywords: tables and their columns.
#[derive(Default)]
pub struct Vocabulary {
    tables: Vec<(String, Vec<String>)>,
}

impl Vocabulary {
    pub fn from_schema(schema: &Schema) -> Self {
        let tables = schema
            .tables
            .iter()
            .map(|t| (t.name.clone(), t.columns.iter().map(|c| c.name.clone()).collect()))
            .collect();
        Vocabulary { tables }
    }

    fn columns_of(&self, table: &str) -> Option<&[String]> {
        self.tables.iter().find(|(t, _)| t.eq_ignore_ascii_case(table)).map(|(_, c)| c.as_slice())
    }
}

/// A problem shown on a line (0-based): tinted, with its message after the text.
pub struct Mark {
    pub line: usize,
    pub error: bool,
    pub message: String,
}

/// What the code around an editor asks of it and learns from it, by `id_salt`.
#[derive(Clone, Default)]
struct Shared {
    goto: Option<usize>,
    caret_line: Option<usize>,
}

fn shared_id(id_salt: &str) -> egui::Id {
    egui::Id::new(("code_editor", id_salt))
}

/// Puts the caret of editor `id_salt` on `line` (0-based), focuses it and scrolls there.
pub fn goto_line(ctx: &egui::Context, id_salt: &str, line: usize) {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Shared>(shared_id(id_salt)).goto = Some(line));
    ctx.request_repaint();
}

/// The line (0-based) of the caret of editor `id_salt` while it has focus.
pub fn caret_line(ctx: &egui::Context, id_salt: &str) -> Option<usize> {
    ctx.data(|d| d.get_temp::<Shared>(shared_id(id_salt))).and_then(|s| s.caret_line)
}

/// Consumes a key press with exactly these modifiers (`consume_key` would
/// also take Ctrl+Shift+F for Ctrl+F).
pub fn take_key(ctx: &egui::Context, modifiers: Modifiers, key: Key) -> bool {
    ctx.input_mut(|i| {
        let found = i.events.iter().position(
            |e| matches!(e, Event::Key { key: k, pressed: true, modifiers: m, .. } if *k == key && m.matches_exact(modifiers)),
        );
        found.map(|n| i.events.remove(n)).is_some()
    })
}

/// One caret of a column selection. Columns are char offsets within `line`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Caret {
    line: usize,
    anchor: usize,
    head: usize,
}

impl Caret {
    fn at(line: usize, col: usize) -> Self {
        Caret { line, anchor: col, head: col }
    }

    fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

#[derive(Clone, Copy)]
struct Popup {
    /// Char index where the word being completed starts.
    start: usize,
    selected: usize,
    /// Opened with Ctrl+Space: stays open with an empty prefix. `start` is
    /// `usize::MAX` until the first layout fills it in.
    forced: bool,
    scroll: bool,
}

#[derive(Clone, Default)]
struct Find {
    query: String,
    replace: String,
    match_case: bool,
    /// Focus the query field on the next frame.
    focus: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum FindAction {
    Next,
    Prev,
    /// The query changed: select the first match from the caret on.
    Search,
    Replace,
    ReplaceAll,
    Close,
}

#[derive(Clone, Default)]
struct State {
    /// Column selection, sorted by line, one per line. Empty in normal editing.
    carets: Vec<Caret>,
    /// (line, virtual column) where an Alt+drag started.
    drag_from: Option<(usize, usize)>,
    alt_down: bool,
    /// Alt is down and nothing else happened since it went down.
    alt_tap: bool,
    popup: Option<Popup>,
    find: Option<Find>,
    /// Char index to bring into view after the next layout; `true` centers it.
    scroll_to: Option<(usize, bool)>,
    /// Last frame's layout, to map the pointer before `TextEdit` handles it.
    galley: Option<(Arc<Galley>, Pos2)>,
}

const FONT_SIZE: f32 = 13.5;

/// A monospace code editor with line numbers, highlighting, editing commands,
/// find/replace, column selection and autocomplete.
#[allow(clippy::too_many_arguments)]
pub fn code_editor(
    ui: &mut egui::Ui,
    text: &mut String,
    lang: Lang,
    id_salt: &str,
    rows: usize,
    vocab: &Vocabulary,
    marks: &[Mark],
) -> egui::Response {
    let id = ui.make_persistent_id(id_salt);
    let state_id = id.with("editor");
    let find_ids = [id.with("find_query"), id.with("find_replace")];
    let mut st: State = ui.data(|d| d.get_temp(state_id)).unwrap_or_default();
    let mut shared: Shared = ui.data(|d| d.get_temp(shared_id(id_salt))).unwrap_or_default();
    let focused = ui.memory(|m| m.has_focus(id));
    let find_focused = ui.memory(|m| find_ids.iter().any(|f| m.has_focus(*f)));
    if !focused {
        st.carets.clear();
        st.popup = None;
    }
    let mut te_state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    let mut cursor = te_state.cursor.char_range();
    let mut set_cursor: Option<CCursorRange> = None;
    let mut typed = false;

    if let Some(line) = shared.goto.take() {
        let line = line.min(line_count(text) - 1);
        let indent = text.split('\n').nth(line).map_or(0, |l| l.chars().take_while(|c| c.is_whitespace()).count());
        let at = char_index(text, line, indent);
        let r = CCursorRange::one(CCursor::new(at));
        set_cursor = Some(r);
        cursor = Some(r);
        st.carets.clear();
        st.scroll_to = Some((at, true));
        ui.memory_mut(|m| m.request_focus(id));
    }

    // Find bar shortcuts work from the editor and from the bar itself.
    let mut find_action = None;
    if focused || find_focused {
        if take_key(ui.ctx(), Modifiers::COMMAND, Key::F) {
            let selected = cursor
                .filter(|r| !r.is_empty())
                .map(|r| char_slice(text, r.primary.index.0.min(r.secondary.index.0), r.primary.index.0.max(r.secondary.index.0)))
                .filter(|s| !s.contains('\n'));
            let find = st.find.get_or_insert_with(Find::default);
            if let Some(s) = selected {
                find.query = s;
            }
            find.focus = true;
            st.popup = None;
        }
        if st.find.is_some() {
            if take_key(ui.ctx(), Modifiers::NONE, Key::F3) {
                find_action = Some(FindAction::Next);
            } else if take_key(ui.ctx(), Modifiers::SHIFT, Key::F3) {
                find_action = Some(FindAction::Prev);
            }
        }
    }

    if focused {
        let events = ui.input(|i| i.events.clone());
        let mut consumed = vec![false; events.len()];
        for (n, event) in events.iter().enumerate() {
            if !st.carets.is_empty() {
                let mut copied = None;
                match column_event(text, &mut st.carets, event, &mut copied) {
                    Column::Consumed => consumed[n] = true,
                    Column::Ignored => {}
                    Column::Exit { consume } => {
                        let c = st.carets.last().copied().unwrap_or(Caret::at(0, 0));
                        let r = CCursorRange::one(CCursor::new(char_index(text, c.line, c.head)));
                        set_cursor = Some(r);
                        cursor = Some(r);
                        st.carets.clear();
                        consumed[n] = consume;
                    }
                }
                if let Some(s) = copied {
                    ui.ctx().copy_text(s);
                }
                continue;
            }
            let at = cursor.map_or(0, |r| r.primary.index.0);
            match event {
                Event::Key { key: Key::Space, pressed: true, modifiers, .. } if modifiers.ctrl => {
                    st.popup = Some(Popup { start: usize::MAX, selected: 0, forced: true, scroll: false });
                    consumed[n] = true;
                }
                Event::Key { key: key @ (Key::ArrowUp | Key::ArrowDown), pressed: true, modifiers, .. }
                    if modifiers.alt && modifiers.shift =>
                {
                    let (line, col) = line_col(text, at);
                    let other = if *key == Key::ArrowUp { line.checked_sub(1) } else { Some(line + 1) };
                    if let Some(other) = other.filter(|&l| l < line_count(text)) {
                        st.carets = column_carets(text, (line, col), (other, col));
                        st.popup = None;
                    }
                    consumed[n] = true;
                }
                Event::Key { key, pressed: true, modifiers, .. } if st.popup.is_some() && modifiers.is_none() => {
                    let p = st.popup.as_mut().unwrap();
                    match key {
                        Key::ArrowDown | Key::ArrowUp => {
                            let count = completions(text, at, lang, vocab).1.len().max(1);
                            p.selected = if *key == Key::ArrowDown { (p.selected + 1) % count } else { (p.selected + count - 1) % count };
                            p.scroll = true;
                            consumed[n] = true;
                        }
                        Key::Enter | Key::Tab => {
                            let (start, cands) = completions(text, at, lang, vocab);
                            if let Some(c) = cands.get(p.selected) {
                                let (a, h) = accept(text, start, at, c);
                                let r = CCursorRange::two(CCursor::new(a), CCursor::new(h));
                                set_cursor = Some(r);
                                cursor = Some(r);
                                consumed[n] = true;
                            }
                            st.popup = None;
                        }
                        Key::Escape => {
                            st.popup = None;
                            consumed[n] = true;
                        }
                        _ => {}
                    }
                }
                Event::Text(s) | Event::Ime(ImeEvent::Commit(s)) => typed = s.chars().all(|c| is_ident(c) || c == '.'),
                _ => {}
            }
            if !consumed[n]
                && let Some(r) = cursor
                && let Some((a, h)) = command(text, (r.secondary.index.0, r.primary.index.0), event, lang)
            {
                let r = CCursorRange::two(CCursor::new(a), CCursor::new(h));
                set_cursor = Some(r);
                cursor = Some(r);
                st.popup = None;
                st.scroll_to = Some((h, false));
                consumed[n] = true;
            }
        }
        if consumed.contains(&true) {
            ui.input_mut(|i| {
                let mut n = 0;
                i.events.retain(|_| {
                    n += 1;
                    !consumed[n - 1]
                });
            });
        }
    }

    // Tapping Alt alone turns a multi-line selection into a column selection.
    let (alt, busy) = ui.input(|i| {
        let busy = !i.focused
            || i.pointer.any_down()
            || i.events.iter().any(|e| matches!(e, Event::Key { pressed: true, .. } | Event::Text(_)));
        (i.modifiers.alt, busy)
    });
    if alt && !st.alt_down {
        st.alt_tap = true;
    }
    if busy {
        st.alt_tap = false;
    }
    if !alt && st.alt_down && st.alt_tap && focused && st.carets.is_empty()
        && let Some(r) = cursor
    {
        let a = line_col(text, r.secondary.index.0);
        let h = line_col(text, r.primary.index.0);
        if a.0 != h.0 {
            st.carets = column_carets(text, a, h);
            st.popup = None;
        }
    }
    st.alt_down = alt;

    // The find bar floats over the top right corner of the visible area.
    if let Some(find) = &mut st.find {
        let action = find_bar(ui, find, &find_ids, text, cursor);
        find_action = action.or(find_action);
    }
    if let Some(action) = find_action
        && let Some(find) = &st.find
    {
        if action == FindAction::Close {
            st.find = None;
            ui.memory_mut(|m| m.request_focus(id));
        } else {
            let sel = cursor.map_or((0, 0), |r| {
                let (a, b) = (r.primary.index.0, r.secondary.index.0);
                (a.min(b), a.max(b))
            });
            if let Some((a, h)) = find_step(text, &find.query, &find.replace, find.match_case, sel, action) {
                let r = CCursorRange::two(CCursor::new(a), CCursor::new(h));
                set_cursor = Some(r);
                cursor = Some(r);
                st.scroll_to = Some((h, true));
            }
        }
    }

    // Keep TextEdit's own selection empty while a column selection is shown, so
    // it doesn't paint a stream selection over ours.
    if st.drag_from.is_some()
        && let Some((galley, origin)) = &st.galley
        && let Some(p) = ui.input(|i| i.pointer.interact_pos())
    {
        set_cursor = Some(CCursorRange::one(galley.cursor_from_pos(p - *origin)));
    } else if let Some(c) = st.carets.last() {
        set_cursor = Some(CCursorRange::one(CCursor::new(char_index(text, c.line, c.head))));
    }
    if let Some(r) = set_cursor {
        te_state.cursor.set_char_range(Some(r));
        te_state.store(ui.ctx(), id);
    }

    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap_width: f32| {
        let mut job = highlight::highlight(ui, buf.as_str(), lang);
        job.wrap.max_width = f32::INFINITY;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let char_w = ui.fonts_mut(|f| f.glyph_width(&FontId::monospace(FONT_SIZE), ' '));
    let digits = line_count(text).to_string().len().max(3);
    let gutter_w = char_w * digits as f32 + 14.0;
    let (out, backdrop, gutter_x) = ui
        .horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            // Painted under the text once the layout is known.
            let backdrop = ui.painter().add(Shape::Noop);
            let (gutter, _) = ui.allocate_exact_size(vec2(gutter_w, 0.0), egui::Sense::hover());
            let out = egui::TextEdit::multiline(text)
                .id(id)
                .code_editor()
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .lock_focus(true)
                .background_color(Color32::TRANSPARENT)
                .layouter(&mut layouter)
                .show(ui);
            (out, backdrop, gutter.left())
        })
        .inner;
    let response = out.response.response;
    let galley = out.galley;
    let origin = out.galley_pos;
    let to_line_col = |p: Pos2| {
        let (line, _) = line_col(text, galley.cursor_from_pos(p - origin).index.0);
        (line, ((p.x - origin.x) / char_w).round().max(0.0) as usize)
    };
    let lens = line_lengths(text);
    let line_starts: Vec<usize> = lens.iter().scan(0, |s, len| {
        let start = *s;
        *s += len + 1;
        Some(start)
    }).collect();
    let row_rect = |line: usize| {
        let r = galley.pos_from_cursor(CCursor::new(line_starts[line])).translate(origin.to_vec2());
        Rect::from_x_y_ranges(gutter_x..=response.rect.right(), r.y_range())
    };

    // Background, current line, problem lines, find matches, line numbers.
    {
        let visuals = ui.visuals();
        let painter = ui.painter();
        let mut shapes = vec![Shape::rect_filled(
            Rect::from_min_max(Pos2::new(gutter_x, response.rect.top()), response.rect.max),
            2.0,
            visuals.text_edit_bg_color(),
        )];
        let caret_line = cursor.map(|r| line_col(text, r.primary.index.0).0);
        if focused && st.carets.is_empty() && let Some(l) = caret_line.filter(|&l| l < line_starts.len()) {
            shapes.push(Shape::rect_filled(row_rect(l), 0.0, visuals.faint_bg_color.gamma_multiply(1.6)));
        }
        let mark_color = |error: bool| if error { visuals.error_fg_color } else { visuals.warn_fg_color };
        for m in marks.iter().filter(|m| m.line < line_starts.len()) {
            shapes.push(Shape::rect_filled(row_rect(m.line), 0.0, mark_color(m.error).gamma_multiply(0.12)));
        }
        if let Some(find) = &st.find {
            let sel = cursor.map(|r| (r.primary.index.0.min(r.secondary.index.0), r.primary.index.0.max(r.secondary.index.0)));
            for (lo, hi) in find_all(text, &find.query, find.match_case) {
                let a = galley.pos_from_cursor(CCursor::new(lo)).translate(origin.to_vec2());
                let b = galley.pos_from_cursor(CCursor::new(hi)).translate(origin.to_vec2());
                let current = sel == Some((lo, hi));
                let color = if current { visuals.selection.bg_fill } else { visuals.selection.bg_fill.gamma_multiply(0.35) };
                shapes.push(Shape::rect_filled(Rect::from_min_max(a.min, b.max), 2.0, color));
            }
        }
        painter.set(backdrop, Shape::Vec(shapes));

        let clip = ui.clip_rect();
        let font = FontId::monospace(FONT_SIZE);
        for (line, _) in line_starts.iter().enumerate() {
            let r = row_rect(line);
            if r.bottom() < clip.top() || r.top() > clip.bottom() {
                continue;
            }
            let mark = marks.iter().filter(|m| m.line == line).min_by_key(|m| !m.error);
            let color = match mark {
                Some(m) => mark_color(m.error),
                None if focused && caret_line == Some(line) => visuals.strong_text_color(),
                None => visuals.weak_text_color(),
            };
            painter.text(Pos2::new(gutter_x + gutter_w - 8.0, r.center().y), Align2::RIGHT_CENTER, line + 1, font.clone(), color);
        }
        // Messages after the end of their line.
        let small = FontId::proportional(FONT_SIZE - 1.5);
        for (line, start) in line_starts.iter().enumerate() {
            let line_marks: Vec<&Mark> = marks.iter().filter(|m| m.line == line).collect();
            if line_marks.is_empty() {
                continue;
            }
            let end = galley.pos_from_cursor(CCursor::new(start + lens[line])).translate(origin.to_vec2());
            let message = line_marks
                .iter()
                .map(|m| format!("{} {}", if m.error { "✖" } else { "⚠" }, m.message))
                .collect::<Vec<_>>()
                .join("   ");
            let error = line_marks.iter().any(|m| m.error);
            painter.text(Pos2::new(end.right() + 3.0 * char_w, end.center().y), Align2::LEFT_CENTER, message, small.clone(), mark_color(error));
        }
        shared.caret_line = caret_line.filter(|_| focused);
    }

    if let Some((at, center)) = st.scroll_to.take() {
        let r = galley.pos_from_cursor(CCursor::new(at.min(galley.len()))).translate(origin.to_vec2());
        let r = Rect::from_x_y_ranges(gutter_x..=r.right() + 2.0 * char_w, r.y_range()).expand2(vec2(0.0, 2.0 * r.height()));
        ui.scroll_to_rect(r, center.then_some(Align::Center));
    }

    // Ctrl+click on a table or `table.column` in the DSL jumps to its definition.
    if lang == Lang::Dbs
        && response.hovered()
        && ui.input(|i| i.modifiers.command)
        && let Some(p) = ui.input(|i| i.pointer.hover_pos())
        && let Some(line) = definition(text, galley.cursor_from_pos(p - origin).index.0)
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if response.clicked() {
            shared.goto = Some(line);
            ui.ctx().request_repaint();
        }
    }

    // Alt+drag: rectangular selection.
    let (pressed, down, pointer) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.pointer.interact_pos()));
    if pressed && response.hovered() {
        st.carets.clear();
        st.drag_from = None;
        if alt && let Some(p) = pointer {
            st.drag_from = Some(to_line_col(p));
            st.popup = None;
        }
    }
    if let Some(from) = st.drag_from {
        if down && let Some(p) = pointer {
            st.carets = column_carets(text, from, to_line_col(p));
        }
        if !down {
            st.drag_from = None;
            // A rectangle on a single line is an ordinary selection.
            if let [c] = st.carets[..] {
                let r = CCursorRange::two(
                    CCursor::new(char_index(text, c.line, c.anchor)),
                    CCursor::new(char_index(text, c.line, c.head)),
                );
                let mut s = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                s.cursor.set_char_range(Some(r));
                s.store(ui.ctx(), id);
                st.carets.clear();
            }
        }
    }

    // Paint the column selection over the text.
    if !st.carets.is_empty() {
        let painter = ui.painter().with_clip_rect(out.text_clip_rect);
        let fill = ui.visuals().selection.bg_fill.gamma_multiply(0.45);
        let stroke = Stroke::new(ui.visuals().text_cursor.stroke.width.max(1.5), ui.visuals().text_cursor.stroke.color);
        for c in &st.carets {
            let base = char_index(text, c.line, 0);
            let rect = |col: usize| galley.pos_from_cursor(CCursor::new(base + col)).translate(origin.to_vec2());
            let (lo, hi) = c.range();
            if lo < hi {
                painter.rect_filled(egui::Rect::from_min_max(rect(lo).min, rect(hi).max), 0.0, fill);
            }
            let r = rect(c.head);
            painter.line_segment([r.center_top(), r.center_bottom()], stroke);
        }
    }

    // Autocomplete popup.
    let cursor = out.cursor_range.filter(|r| r.is_empty()).map(|r| r.primary.index.0);
    match cursor {
        Some(at) if focused && st.carets.is_empty() => {
            let (start, cands) = completions(text, at, lang, vocab);
            if typed && st.popup.is_none() {
                st.popup = Some(Popup { start, selected: 0, forced: false, scroll: false });
            }
            if let Some(p) = &mut st.popup {
                if p.start == usize::MAX {
                    p.start = start;
                }
                let qualified = start > 0 && text.chars().nth(start - 1) == Some('.');
                if p.start != start || cands.is_empty() || (at == start && !qualified && !p.forced) {
                    st.popup = None;
                } else {
                    p.selected = p.selected.min(cands.len() - 1);
                    let caret = galley.pos_from_cursor(CCursor::new(start)).translate(origin.to_vec2());
                    if let Some(chosen) = show_popup(ui, id, caret.left_bottom(), &cands, p) {
                        let (a, h) = accept(text, start, at, &cands[chosen]);
                        let mut s = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                        s.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(h))));
                        s.store(ui.ctx(), id);
                        ui.memory_mut(|m| m.request_focus(id));
                        st.popup = None;
                    }
                }
            }
        }
        _ => st.popup = None,
    }

    st.galley = Some((galley, origin));
    ui.data_mut(|d| {
        d.insert_temp(state_id, st);
        d.insert_temp(shared_id(id_salt), shared);
    });
    response
}

/// Draws the completion list; returns the index of a clicked candidate.
fn show_popup(ui: &egui::Ui, id: egui::Id, pos: Pos2, cands: &[Candidate], p: &mut Popup) -> Option<usize> {
    let mut chosen = None;
    egui::Area::new(id.with("autocomplete"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    ui.set_min_width(180.0);
                    for (n, c) in cands.iter().enumerate() {
                        let label = format!("{:<24} {}", c.text, c.kind.label());
                        let r = ui.selectable_label(n == p.selected, RichText::new(label).monospace());
                        if n == p.selected && p.scroll {
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            chosen = Some(n);
                        }
                    }
                });
            });
        });
    p.scroll = false;
    chosen
}

/// The find/replace bar; returns what the user asked for this frame.
fn find_bar(ui: &egui::Ui, find: &mut Find, ids: &[egui::Id; 2], text: &str, cursor: Option<CCursorRange>) -> Option<FindAction> {
    let mut action = None;
    let matches = find_all(text, &find.query, find.match_case);
    let sel = cursor.map(|r| (r.primary.index.0.min(r.secondary.index.0), r.primary.index.0.max(r.secondary.index.0)));
    let current = matches.iter().position(|m| Some(*m) == sel);
    let pos = ui.clip_rect().right_top() + vec2(-12.0, 6.0);
    egui::Area::new(ids[0].with("bar"))
        .order(egui::Order::Foreground)
        .pivot(Align2::RIGHT_TOP)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                let enter = |ui: &egui::Ui, r: &egui::Response| r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                let escape = |ui: &egui::Ui, r: &egui::Response| r.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape));
                ui.horizontal(|ui| {
                    let q = ui.add(egui::TextEdit::singleline(&mut find.query).id(ids[0]).hint_text("Find").desired_width(180.0));
                    if find.focus {
                        q.request_focus();
                        // Select the query, so typing replaces it (once the field exists).
                        if let Some(mut s) = egui::TextEdit::load_state(ui.ctx(), ids[0]) {
                            let all = CCursorRange::two(CCursor::new(0), CCursor::new(find.query.chars().count()));
                            s.cursor.set_char_range(Some(all));
                            s.store(ui.ctx(), ids[0]);
                            find.focus = false;
                        }
                    }
                    if q.changed() {
                        action = Some(FindAction::Search);
                    }
                    if enter(ui, &q) {
                        action = Some(if ui.input(|i| i.modifiers.shift) { FindAction::Prev } else { FindAction::Next });
                        q.request_focus();
                    }
                    if escape(ui, &q) {
                        action = Some(FindAction::Close);
                    }
                    let count = match (current, matches.len()) {
                        (_, 0) if !find.query.is_empty() => RichText::new("No results").color(ui.visuals().warn_fg_color),
                        (_, 0) => RichText::new(""),
                        (Some(c), n) => RichText::new(format!("{} of {n}", c + 1)),
                        (None, n) => RichText::new(format!("{n} found")),
                    };
                    ui.add_sized([72.0, 18.0], egui::Label::new(count.small()));
                    if ui.selectable_label(find.match_case, "Aa").on_hover_text("Match case").clicked() {
                        find.match_case = !find.match_case;
                        action = Some(FindAction::Search);
                    }
                    if ui.small_button("⏶").on_hover_text("Previous match (Shift+F3, Shift+Enter)").clicked() {
                        action = Some(FindAction::Prev);
                    }
                    if ui.small_button("⏷").on_hover_text("Next match (F3, Enter)").clicked() {
                        action = Some(FindAction::Next);
                    }
                    if ui.small_button("✖").on_hover_text("Close (Esc)").clicked() {
                        action = Some(FindAction::Close);
                    }
                });
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut find.replace).id(ids[1]).hint_text("Replace").desired_width(180.0));
                    if enter(ui, &r) {
                        action = Some(FindAction::Replace);
                        r.request_focus();
                    }
                    if escape(ui, &r) {
                        action = Some(FindAction::Close);
                    }
                    ui.add_enabled_ui(!matches.is_empty(), |ui| {
                        if ui.button("Replace").on_hover_text("Replace this match and go to the next (Enter)").clicked() {
                            action = Some(FindAction::Replace);
                        }
                        if ui.button("All").on_hover_text("Replace every match").clicked() {
                            action = Some(FindAction::ReplaceAll);
                        }
                    });
                });
            });
        });
    action
}

// ---------------------------------------------------------------- column editing

enum Column {
    Consumed,
    /// Not an editing event (pointer move, key release…).
    Ignored,
    /// Leave column mode; `consume` keeps the event from `TextEdit`.
    Exit { consume: bool },
}

fn column_event(text: &mut String, carets: &mut Vec<Caret>, event: &Event, copied: &mut Option<String>) -> Column {
    match event {
        // With an input method running (GNOME/ibus), typed characters arrive as IME commits.
        Event::Text(s) | Event::Ime(ImeEvent::Commit(s)) if !s.is_empty() && !s.contains(['\n', '\r']) => {
            splice(text, carets, |_, c, _| (c.range().0, c.range().1, s.clone()));
        }
        // Composition in progress (dead keys…): wait for the commit.
        Event::Ime(ImeEvent::Preedit { .. } | ImeEvent::DeleteSurrounding { .. }) => {}
        Event::Paste(s) => {
            let s = s.replace("\r\n", "\n");
            let parts: Vec<&str> = s.strip_suffix('\n').unwrap_or(&s).split('\n').collect();
            if parts.len() == carets.len() {
                splice(text, carets, |n, c, _| (c.range().0, c.range().1, parts[n].to_string()));
            } else if parts.len() == 1 {
                splice(text, carets, |_, c, _| (c.range().0, c.range().1, parts[0].to_string()));
            } else {
                return Column::Exit { consume: false };
            }
        }
        Event::Copy | Event::Cut => {
            *copied = Some(selected_text(text, carets));
            if matches!(event, Event::Cut) {
                splice(text, carets, |_, c, _| (c.range().0, c.range().1, String::new()));
            }
        }
        Event::Key { key, pressed: true, modifiers, .. } => {
            let extend = modifiers.shift;
            let line_wise = modifiers.mac_cmd || (modifiers.command && !cfg!(target_os = "macos"));
            match key {
                Key::Escape => return Column::Exit { consume: true },
                Key::Backspace => splice(text, carets, |_, c, _| match c.range() {
                    (lo, hi) if lo < hi => (lo, hi, String::new()),
                    (lo, _) => (lo.saturating_sub(1), lo, String::new()),
                }),
                Key::Delete => splice(text, carets, |_, c, len| match c.range() {
                    (lo, hi) if lo < hi => (lo, hi, String::new()),
                    (lo, _) => (lo, (lo + 1).min(len), String::new()),
                }),
                Key::Tab => splice(text, carets, |_, c, _| (c.range().0, c.range().1, "\t".into())),
                Key::ArrowLeft | Key::ArrowRight | Key::Home | Key::End => {
                    let lens = line_lengths(text);
                    for c in carets.iter_mut() {
                        let len = lens[c.line];
                        let (lo, hi) = c.range();
                        let head = match key {
                            Key::Home => 0,
                            Key::End => len,
                            Key::ArrowLeft if line_wise => 0,
                            Key::ArrowRight if line_wise => len,
                            Key::ArrowLeft if !extend && lo < hi => lo,
                            Key::ArrowRight if !extend && lo < hi => hi,
                            Key::ArrowLeft => c.head.saturating_sub(1),
                            _ => (c.head + 1).min(len),
                        };
                        *c = Caret { line: c.line, anchor: if extend { c.anchor } else { head }, head };
                    }
                }
                Key::ArrowUp | Key::ArrowDown if modifiers.alt && modifiers.shift => {
                    let lens = line_lengths(text);
                    let (first, last) = (carets[0], carets[carets.len() - 1]);
                    if *key == Key::ArrowUp && first.line > 0 {
                        let l = first.line - 1;
                        carets.insert(0, Caret { line: l, anchor: first.anchor.min(lens[l]), head: first.head.min(lens[l]) });
                    } else if *key == Key::ArrowDown && last.line + 1 < lens.len() {
                        let l = last.line + 1;
                        carets.push(Caret { line: l, anchor: last.anchor.min(lens[l]), head: last.head.min(lens[l]) });
                    }
                }
                // Shortcuts (undo, select all…) and keys that leave the line.
                Key::Enter | Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown => {
                    return Column::Exit { consume: false };
                }
                _ if modifiers.command || modifiers.ctrl || modifiers.mac_cmd => return Column::Exit { consume: false },
                // A character key: its `Event::Text` follows and is typed on every line.
                _ => {}
            }
        }
        _ => return Column::Ignored,
    }
    Column::Consumed
}

/// Replaces, on each caret's line, the char columns `lo..hi` returned by `f`
/// (given the caret index, the caret and the line length) with a string, and
/// leaves the caret after it.
fn splice(text: &mut String, carets: &mut [Caret], mut f: impl FnMut(usize, Caret, usize) -> (usize, usize, String)) {
    let starts = line_starts(text);
    for (n, c) in carets.iter_mut().enumerate().rev() {
        let start = starts[c.line];
        let line = &text[start..];
        let line = &line[..line.find('\n').unwrap_or(line.len())];
        let len = line.chars().count();
        let (lo, hi, s) = f(n, *c, len);
        let byte = |col: usize| start + line.char_indices().nth(col).map_or(line.len(), |(b, _)| b);
        let range = byte(lo.min(len))..byte(hi.min(len));
        text.replace_range(range, &s);
        *c = Caret::at(c.line, lo.min(len) + s.chars().count());
    }
}

fn selected_text(text: &str, carets: &[Caret]) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    carets
        .iter()
        .map(|c| {
            let (lo, hi) = c.range();
            lines[c.line].chars().skip(lo).take(hi - lo).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Carets for the rectangle between two (line, virtual column) corners;
/// columns past the end of a short line stop at its end.
fn column_carets(text: &str, from: (usize, usize), to: (usize, usize)) -> Vec<Caret> {
    let lens = line_lengths(text);
    let last = lens.len() - 1;
    let (a, b) = (from.0.min(last), to.0.min(last));
    (a.min(b)..=a.max(b)).map(|l| Caret { line: l, anchor: from.1.min(lens[l]), head: to.1.min(lens[l]) }).collect()
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

fn line_lengths(text: &str) -> Vec<usize> {
    text.split('\n').map(|l| l.chars().count()).collect()
}

fn line_count(text: &str) -> usize {
    text.split('\n').count()
}

/// (line, column) of a char index.
fn line_col(text: &str, index: usize) -> (usize, usize) {
    let (mut line, mut col) = (0, 0);
    for c in text.chars().take(index) {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Char index of (line, column), the column clamped to the line's length.
fn char_index(text: &str, line: usize, col: usize) -> usize {
    let mut index = 0;
    for (n, l) in text.split('\n').enumerate() {
        let len = l.chars().count();
        if n == line {
            return index + col.min(len);
        }
        index += len + 1;
    }
    text.chars().count()
}

/// Replaces chars `lo..hi` with `s`; returns the char index after it.
fn replace_chars(text: &mut String, lo: usize, hi: usize, s: &str) -> usize {
    let byte = |i: usize| text.char_indices().nth(i).map_or(text.len(), |(b, _)| b);
    let range = byte(lo)..byte(hi);
    text.replace_range(range, s);
    lo + s.chars().count()
}

// ---------------------------------------------------------------- commands

/// A selection as (anchor, head) char indices.
type Sel = (usize, usize);

const INDENT: &str = "  ";

/// Runs the editing command bound to `event`, if any; returns the new selection.
fn command(text: &mut String, sel: Sel, event: &Event, lang: Lang) -> Option<Sel> {
    let (lo, hi) = (sel.0.min(sel.1), sel.0.max(sel.1));
    match event {
        Event::Key { key, pressed: true, modifiers: m, .. } => {
            let cmd = m.command && !m.alt && !m.shift;
            let one_line = line_col(text, lo).0 == line_col(text, hi).0;
            match key {
                Key::Enter if m.is_none() => Some(newline(text, lo, hi, lang)),
                Key::Tab if m.is_none() && one_line => {
                    let col = line_col(text, lo).1;
                    let at = replace_chars(text, lo, hi, &" ".repeat(INDENT.len() - col % INDENT.len()));
                    Some((at, at))
                }
                Key::Tab if m.is_none() || m.matches_exact(Modifiers::SHIFT) => Some(indent_lines(text, sel, !m.shift)),
                Key::Slash if cmd => Some(toggle_comment(text, sel, lang)),
                Key::D if cmd => Some(duplicate_lines(text, sel)),
                Key::K if m.command && m.shift && !m.alt => Some(delete_lines(text, sel)),
                Key::ArrowUp | Key::ArrowDown if m.alt && !m.shift && !m.command => Some(move_lines(text, sel, *key == Key::ArrowUp)),
                _ => None,
            }
        }
        // `}` typed on an indented blank line closes a block: one level less.
        Event::Text(s) if s == "}" && lang == Lang::Dbs && lo == hi => {
            let (line, col) = line_col(text, lo);
            let before = char_slice(text, char_index(text, line, 0), lo);
            if col == 0 || !before.chars().all(|c| c == ' ') {
                return None;
            }
            let start = lo - col.min(INDENT.len());
            let at = replace_chars(text, start, lo, "}");
            Some((at, at))
        }
        _ => None,
    }
}

/// Enter: a new line with the current indentation, one level more after an
/// opening bracket. Between `{` and `}` the closing bracket gets its own line;
/// after a `{` with no `}` to match it, the DSL gets one.
fn newline(text: &mut String, lo: usize, hi: usize, lang: Lang) -> Sel {
    replace_chars(text, lo, hi, "");
    let (line, col) = line_col(text, lo);
    let line_start = lo - col;
    let line_text: String = text.split('\n').nth(line).unwrap_or_default().to_string();
    let before: String = line_text.chars().take(col).collect();
    let after: String = line_text.chars().skip(col).collect();
    let indent: String = before.chars().take_while(|&c| c == ' ' || c == '\t').collect();
    let opens = before.trim_end().ends_with(['{', '(']);
    let closes = after.trim_start().starts_with(['}', ')']);
    let unbalanced = lang == Lang::Dbs && before.trim_end().ends_with('{') && text.matches('{').count() > text.matches('}').count();
    // A line left with only spaces loses them.
    let lo = if before.trim().is_empty() {
        replace_chars(text, line_start, lo, "");
        line_start
    } else {
        lo
    };
    let inner = format!("\n{indent}{INDENT}");
    if opens && closes {
        let spaces = after.chars().take_while(|c| c.is_whitespace()).count();
        replace_chars(text, lo, lo + spaces, &format!("{inner}\n{indent}"));
        let at = lo + inner.chars().count();
        (at, at)
    } else if unbalanced && after.trim().is_empty() {
        let at = replace_chars(text, lo, lo, &inner);
        replace_chars(text, at, at, &format!("\n{indent}}}"));
        (at, at)
    } else {
        let plain = format!("\n{indent}");
        let at = replace_chars(text, lo, lo, if opens { &inner } else { &plain });
        (at, at)
    }
}

/// First and last line of a selection; a selection ending at the start of a
/// line doesn't take that line in.
fn line_span(text: &str, sel: Sel) -> (usize, usize) {
    let (lo, hi) = (sel.0.min(sel.1), sel.0.max(sel.1));
    let (a, b) = (line_col(text, lo), line_col(text, hi));
    (a.0, if b.0 > a.0 && b.1 == 0 { b.0 - 1 } else { b.0 })
}

/// Rewrites the lines of `text`; `f` gets them and returns how a (line,
/// column) position moves, which is applied to the selection.
fn edit_lines(text: &mut String, sel: Sel, f: impl FnOnce(&mut Vec<String>) -> Box<dyn Fn(usize, usize) -> (usize, usize)>) -> Sel {
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    let (a, h) = (line_col(text, sel.0), line_col(text, sel.1));
    let map = f(&mut lines);
    *text = lines.join("\n");
    let (a, h) = (map(a.0, a.1), map(h.0, h.1));
    (char_index(text, a.0, a.1), char_index(text, h.0, h.1))
}

fn indent_lines(text: &mut String, sel: Sel, indent: bool) -> Sel {
    let (first, last) = line_span(text, sel);
    edit_lines(text, sel, |lines| {
        let mut shift = vec![0isize; lines.len()];
        for (n, l) in lines.iter_mut().enumerate().take(last + 1).skip(first) {
            if indent && !l.trim().is_empty() {
                l.insert_str(0, INDENT);
                shift[n] = INDENT.len() as isize;
            } else if !indent {
                let n_spaces = if l.starts_with('\t') { 1 } else { l.chars().take(INDENT.len()).take_while(|&c| c == ' ').count() };
                l.drain(..n_spaces);
                shift[n] = -(n_spaces as isize);
            }
        }
        Box::new(move |line, col| (line, col.saturating_add_signed(shift[line])))
    })
}

fn toggle_comment(text: &mut String, sel: Sel, lang: Lang) -> Sel {
    let (first, last) = line_span(text, sel);
    let markers: &[&str] = if lang == Lang::Dbs { &["#", "//"] } else { &["--"] };
    edit_lines(text, sel, |lines| {
        let span = &mut lines[first..=last];
        let filled = || span.iter().filter(|l| !l.trim().is_empty());
        let commented = filled().count() > 0 && filled().all(|l| markers.iter().any(|m| l.trim_start().starts_with(m)));
        let indent = filled().map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
        let mut shift = vec![(0usize, 0isize); span.len()];
        for (n, l) in span.iter_mut().enumerate() {
            if l.trim().is_empty() {
                continue;
            }
            let at = l.len() - l.trim_start().len();
            if commented {
                let m = markers.iter().find(|m| l[at..].starts_with(**m)).unwrap();
                let len = m.len() + usize::from(l[at + m.len()..].starts_with(' '));
                l.drain(at..at + len);
                shift[n] = (at, -(len as isize));
            } else {
                let prefix = format!("{} ", markers[0]);
                l.insert_str(indent, &prefix);
                shift[n] = (indent, prefix.len() as isize);
            }
        }
        Box::new(move |line, col| match line.checked_sub(first).and_then(|n| shift.get(n)) {
            Some(&(at, d)) if col >= at => (line, col.saturating_add_signed(d).max(at)),
            _ => (line, col),
        })
    })
}

fn duplicate_lines(text: &mut String, sel: Sel) -> Sel {
    let (first, last) = line_span(text, sel);
    let count = last - first + 1;
    edit_lines(text, sel, |lines| {
        let copy: Vec<String> = lines[first..=last].to_vec();
        lines.splice(last + 1..last + 1, copy);
        Box::new(move |line, col| (line + count, col))
    })
}

fn delete_lines(text: &mut String, sel: Sel) -> Sel {
    let (first, last) = line_span(text, sel);
    let head_col = line_col(text, sel.1).1;
    edit_lines(text, sel, |lines| {
        lines.drain(first..=last);
        if lines.is_empty() {
            lines.push(String::new());
        }
        let line = first.min(lines.len() - 1);
        Box::new(move |_, _| (line, head_col))
    })
}

fn move_lines(text: &mut String, sel: Sel, up: bool) -> Sel {
    let (first, last) = line_span(text, sel);
    edit_lines(text, sel, |lines| {
        if up && first > 0 {
            let l = lines.remove(first - 1);
            lines.insert(last, l);
            Box::new(|line, col| (line - 1, col))
        } else if !up && last + 1 < lines.len() {
            let l = lines.remove(last + 1);
            lines.insert(first, l);
            Box::new(|line, col| (line + 1, col))
        } else {
            Box::new(|line, col| (line, col))
        }
    })
}

/// Chars `lo..hi` of `text`.
fn char_slice(text: &str, lo: usize, hi: usize) -> String {
    text.chars().skip(lo).take(hi.saturating_sub(lo)).collect()
}

// ---------------------------------------------------------------- find

/// Char ranges of the non-overlapping matches of `query`.
fn find_all(text: &str, query: &str, match_case: bool) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = |c: char| if match_case { c } else { c.to_lowercase().next().unwrap_or(c) };
    let hay: Vec<char> = text.chars().map(fold).collect();
    let needle: Vec<char> = query.chars().map(fold).collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == needle[..] {
            found.push((i, i + needle.len()));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    found
}

/// Applies a find action given the current selection `lo..hi`; returns the
/// selection to show.
fn find_step(text: &mut String, query: &str, replace: &str, match_case: bool, (lo, hi): (usize, usize), action: FindAction) -> Option<Sel> {
    let matches = find_all(text, query, match_case);
    let on_match = matches.contains(&(lo, hi));
    let next_from = |matches: &[(usize, usize)], from: usize| matches.iter().find(|m| m.0 >= from).or(matches.first()).copied();
    match action {
        FindAction::Search => next_from(&matches, lo),
        FindAction::Next => next_from(&matches, if on_match { hi } else { lo }),
        FindAction::Prev => matches.iter().rev().find(|m| m.0 < lo).or(matches.last()).copied(),
        FindAction::Replace if on_match => {
            let end = replace_chars(text, lo, hi, replace);
            next_from(&find_all(text, query, match_case), end)
        }
        FindAction::Replace => next_from(&matches, lo),
        FindAction::ReplaceAll => {
            for &(a, b) in matches.iter().rev() {
                replace_chars(text, a, b, replace);
            }
            let at = lo.min(text.chars().count());
            Some((at, at))
        }
        FindAction::Close => None,
    }
}

// ---------------------------------------------------------------- definitions

/// The line (0-based) defining the table or `table.column` at char `index`.
fn definition(text: &str, index: usize) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let word = |c: char| is_ident(c) || c == '.';
    let mut lo = index.min(chars.len());
    while lo > 0 && word(chars[lo - 1]) {
        lo -= 1;
    }
    let mut hi = index.min(chars.len());
    while hi < chars.len() && word(chars[hi]) {
        hi += 1;
    }
    let target: String = chars[lo..hi].iter().collect();
    let target = target.trim_matches('.');
    if target.is_empty() {
        return None;
    }
    let outline = parser::outline(text);
    // A column's own name, on its own definition line, goes nowhere.
    let line = parser::locate(&outline, target)?;
    (line - 1 != line_col(text, index).0).then_some(line - 1)
}

// ---------------------------------------------------------------- autocomplete

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Column,
    Table,
    Keyword,
    Type,
    Snippet,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Column => "column",
            Kind::Table => "table",
            Kind::Keyword => "keyword",
            Kind::Type => "type",
            Kind::Snippet => "snippet",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    text: String,
    kind: Kind,
    /// Inserted instead of `text`, with the char range to select afterwards.
    snippet: Option<(&'static str, usize, usize)>,
}

const TABLE_SNIPPET: (&str, usize, usize) = ("table name {\n  id  int  pk autoincrement\n}", 6, 10);

/// Inserts `c` over chars `start..at`; returns the selection after it.
fn accept(text: &mut String, start: usize, at: usize, c: &Candidate) -> Sel {
    match c.snippet {
        Some((s, lo, hi)) => {
            replace_chars(text, start, at, s);
            (start + lo, start + hi)
        }
        None => {
            let end = replace_chars(text, start, at, &c.text);
            (end, end)
        }
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The start of the word ending at `cursor` and what could complete it.
fn completions(text: &str, cursor: usize, lang: Lang, vocab: &Vocabulary) -> (usize, Vec<Candidate>) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && is_ident(chars[start - 1]) {
        start -= 1;
    }
    let prefix: String = chars[start..cursor].iter().collect();
    if prefix.starts_with(|c: char| c.is_ascii_digit()) || in_comment_or_string(&chars[..start], lang) {
        return (start, Vec::new());
    }

    let mut pool: Vec<Candidate> = Vec::new();
    let mut add = |names: &mut dyn Iterator<Item = String>, kind| pool.extend(names.map(|text| Candidate { text, kind, snippet: None }));
    if start > 0 && chars[start - 1] == '.' {
        let mut q = start - 1;
        while q > 0 && is_ident(chars[q - 1]) {
            q -= 1;
        }
        let qualifier: String = chars[q..start - 1].iter().collect();
        let table = match lang {
            Lang::Sql => sql_aliases(text, vocab)
                .into_iter()
                .find(|(alias, _)| alias.eq_ignore_ascii_case(&qualifier))
                .map_or(qualifier, |(_, t)| t),
            Lang::Dbs => qualifier,
        };
        if let Some(cols) = vocab.columns_of(&table) {
            add(&mut cols.iter().cloned(), Kind::Column);
        }
    } else if lang == Lang::Dbs {
        pool = dbs_candidates(&chars[..start], vocab);
    } else {
        let upper = prefix.starts_with(|c: char| c.is_uppercase());
        let case = |w: &&str| if upper { w.to_uppercase() } else { w.to_string() };
        add(&mut vocab.tables.iter().map(|(t, _)| t.clone()), Kind::Table);
        let mut cols: Vec<String> = vocab.tables.iter().flat_map(|(_, c)| c.iter().cloned()).collect();
        cols.sort();
        cols.dedup();
        add(&mut cols.into_iter(), Kind::Column);
        add(&mut SQL_KEYWORDS.iter().map(case), Kind::Keyword);
        add(&mut DBS_TYPES.iter().map(case), Kind::Type);
    }

    // A word that is already complete gets no popup, so Enter still breaks the line.
    let lower = prefix.to_lowercase();
    if pool.iter().any(|c| c.text.to_lowercase() == lower) {
        return (start, Vec::new());
    }
    pool.retain(|c| c.text.to_lowercase().starts_with(&lower));
    pool.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.text.len().cmp(&b.text.len())).then_with(|| a.text.cmp(&b.text)));
    pool.dedup_by(|a, b| a.text.eq_ignore_ascii_case(&b.text));
    (start, pool)
}

/// What fits at the end of `before` in the DSL: a table snippet between
/// tables; in a table, table-level keywords first, then types, then the
/// modifiers that can follow what is already on the line.
fn dbs_candidates(before: &[char], vocab: &Vocabulary) -> Vec<Candidate> {
    let words = |list: &[&str], kind| list.iter().map(|w| Candidate { text: w.to_string(), kind, snippet: None }).collect::<Vec<_>>();

    // Brace depth, skipping comments and strings.
    let mut depth = 0i32;
    let mut quote = None;
    let mut comment = false;
    for (i, &c) in before.iter().enumerate() {
        match (comment, quote, c) {
            (true, _, '\n') => comment = false,
            (true, ..) => {}
            (_, Some(q), c) if c == q => quote = None,
            (_, Some(_), _) => {}
            (_, None, '\'' | '"') => quote = Some(c),
            (_, None, '#') => comment = true,
            (_, None, '/') if before.get(i + 1) == Some(&'/') => comment = true,
            (_, None, '{') => depth += 1,
            (_, None, '}') => depth -= 1,
            _ => {}
        }
    }

    // The statement so far: back to a line break, `,`, `;` or brace outside parentheses.
    let mut parens = 0;
    let mut i = before.len();
    while i > 0 {
        let c = before[i - 1];
        match c {
            ')' => parens += 1,
            '(' if parens == 0 => {
                // Inside `index (…)`, `pk (…)` or `check (…)`: this table's columns.
                let text: String = before.iter().collect();
                let table = parser::outline(&text).pop().filter(|_| depth > 0);
                let cols = table.and_then(|t| vocab.columns_of(&t.name).map(<[String]>::to_vec)).unwrap_or_default();
                return cols.into_iter().map(|text| Candidate { text, kind: Kind::Column, snippet: None }).collect();
            }
            '(' => parens -= 1,
            '\n' | ',' | ';' | '{' | '}' if parens == 0 => break,
            _ => {}
        }
        i -= 1;
    }
    let stmt: String = before[i..].iter().collect();
    let stmt = stmt.replace("->", " -> ");
    let lower: Vec<String> = stmt.split_whitespace().map(str::to_lowercase).collect();
    let has = |w: &str| lower.iter().any(|x| x == w || x.starts_with(&format!("{w}(")));

    if depth <= 0 {
        return if lower.is_empty() { vec![Candidate { text: "table".into(), kind: Kind::Snippet, snippet: Some(TABLE_SNIPPET) }] } else { vec![] };
    }
    let first = lower.first().map(String::as_str);
    let table_level = match first {
        Some("unique" | "pk" | "primary") => true,
        // `index name (…)`, not a column called "index".
        Some("index") => lower.get(1).is_some_and(|w| !DBS_TYPES.contains(&w.split('(').next().unwrap_or_default())),
        _ => false,
    };
    if table_level {
        return match (first, lower.len()) {
            (Some("unique"), 1) => words(&["index"], Kind::Keyword),
            (Some("primary"), 1) => words(&["key"], Kind::Keyword),
            _ => vec![],
        };
    }
    match lower.len() {
        0 => return words(&["index", "unique index", "pk"], Kind::Keyword),
        1 => return words(DBS_TYPES, Kind::Type),
        _ => {}
    }
    let last = lower.last().map(String::as_str).unwrap_or_default();
    let prev = lower.len().checked_sub(2).map(|n| lower[n].as_str());
    match last {
        "->" | "references" | "ref" => vocab
            .tables
            .iter()
            .flat_map(|(t, cols)| cols.iter().map(move |c| Candidate { text: format!("{t}.{c}"), kind: Kind::Column, snippet: None }))
            .collect(),
        "on" => words(&["delete", "update"], Kind::Keyword),
        "delete" | "update" if prev == Some("on") => words(&["cascade", "set_null", "set_default", "restrict", "no_action"], Kind::Keyword),
        "default" => words(&["now", "true", "false", "null"], Kind::Keyword),
        "not" => words(&["null"], Kind::Keyword),
        "check" | "set" | "no" => vec![],
        _ => {
            let has_ref = has("->") || has("references") || has("ref");
            let mut list = Vec::new();
            for (word, used) in [
                ("pk", has("pk") || has("primary")),
                ("autoincrement", has("autoincrement")),
                ("unique", has("unique")),
                ("not null", has("not") || has("null")),
                ("null", has("not") || has("null")),
                ("default", has("default")),
                ("check", has("check")),
                ("on delete", !has_ref || has("delete")),
                ("on update", !has_ref || has("update")),
            ] {
                if !used {
                    list.push(word);
                }
            }
            words(&list, Kind::Keyword)
        }
    }
}

/// Whether the end of `before` is inside a comment or a string literal on its line.
fn in_comment_or_string(before: &[char], lang: Lang) -> bool {
    let line_start = before.iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
    let line = &before[line_start..];
    let mut quote = None;
    for (i, &c) in line.iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' || c == '`' => quote = Some(c),
            None => {
                let next = line.get(i + 1).copied();
                let comment = match lang {
                    Lang::Dbs => c == '#' || (c == '/' && next == Some('/')),
                    Lang::Sql => c == '-' && next == Some('-'),
                };
                if comment {
                    return true;
                }
            }
        }
    }
    quote.is_some()
}

/// `(alias, table)` pairs from `FROM users u` / `JOIN posts AS p`.
fn sql_aliases(text: &str, vocab: &Vocabulary) -> Vec<(String, String)> {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if is_ident(c) {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            tokens.push(std::mem::take(&mut word));
        }
        if !c.is_whitespace() {
            tokens.push(c.to_string());
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    let is_word = |t: &str| t.starts_with(|c: char| c.is_alphabetic() || c == '_');
    let mut aliases = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if vocab.columns_of(t).is_none() {
            continue;
        }
        let mut j = i + 1;
        if tokens.get(j).is_some_and(|w| w.eq_ignore_ascii_case("as")) {
            j += 1;
        }
        if let Some(alias) = tokens.get(j)
            && is_word(alias)
            && !SQL_KEYWORDS.contains(&alias.to_lowercase().as_str())
        {
            aliases.push((alias.clone(), t.clone()));
        }
    }
    aliases
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocabulary {
        Vocabulary {
            tables: vec![
                ("users".into(), vec!["id".into(), "email".into(), "name".into()]),
                ("posts".into(), vec!["id".into(), "author_id".into(), "title".into()]),
            ],
        }
    }

    fn names(c: &[Candidate]) -> Vec<&str> {
        c.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn rectangle_clamps_to_short_lines() {
        let text = "abcdef\nab\nabcdef";
        let c = column_carets(text, (0, 2), (2, 4));
        assert_eq!(c, vec![Caret { line: 0, anchor: 2, head: 4 }, Caret { line: 1, anchor: 2, head: 2 }, Caret { line: 2, anchor: 2, head: 4 }]);
    }

    #[test]
    fn typing_replaces_every_line() {
        let mut text = "id int\nname text\nx".to_string();
        let mut carets = column_carets(&text, (0, 0), (1, 0));
        let mut copied = None;
        column_event(&mut text, &mut carets, &Event::Text("  ".into()), &mut copied);
        assert_eq!(text, "  id int\n  name text\nx");
        column_event(&mut text, &mut carets, &Event::Key {
            key: Key::Backspace,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }, &mut copied);
        assert_eq!(text, " id int\n name text\nx");
        assert_eq!(carets, vec![Caret::at(0, 1), Caret::at(1, 1)]);
    }

    fn key(key: Key, modifiers: egui::Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn character_keys_stay_in_column_mode() {
        let mut text = "a\nb".to_string();
        let mut carets = column_carets(&text, (0, 1), (1, 1));
        // egui sends the key, then the text it produced.
        assert!(matches!(column_event(&mut text, &mut carets, &key(Key::X, Default::default()), &mut None), Column::Consumed));
        column_event(&mut text, &mut carets, &Event::Text("x".into()), &mut None);
        assert_eq!(text, "ax\nbx");
        assert!(matches!(column_event(&mut text, &mut carets, &key(Key::Z, egui::Modifiers::COMMAND), &mut None), Column::Exit { consume: false }));
        assert!(matches!(column_event(&mut text, &mut carets, &key(Key::Enter, Default::default()), &mut None), Column::Exit { consume: false }));
    }

    #[test]
    fn input_method_commits_type_on_every_line() {
        let mut text = "a
b".to_string();
        let mut carets = column_carets(&text, (0, 1), (1, 1));
        let preedit = Event::Ime(ImeEvent::Preedit { text: "x".into(), active_range_chars: None });
        assert!(matches!(column_event(&mut text, &mut carets, &preedit, &mut None), Column::Consumed));
        column_event(&mut text, &mut carets, &Event::Ime(ImeEvent::Commit("x".into())), &mut None);
        assert_eq!(text, "ax
bx");
    }

    #[test]
    fn copy_and_paste_line_by_line() {
        let mut text = "aXXb\ncYYd".to_string();
        let mut carets = column_carets(&text, (0, 1), (1, 3));
        let mut copied = None;
        column_event(&mut text, &mut carets, &Event::Cut, &mut copied);
        assert_eq!(copied.as_deref(), Some("XX\nYY"));
        assert_eq!(text, "ab\ncd");
        column_event(&mut text, &mut carets, &Event::Paste("1\n2\n".into()), &mut copied);
        assert_eq!(text, "a1b\nc2d");
    }

    #[test]
    fn multibyte_lines() {
        let mut text = "é1\nü2".to_string();
        let mut carets = column_carets(&text, (0, 1), (1, 1));
        column_event(&mut text, &mut carets, &Event::Text("-".into()), &mut None);
        assert_eq!(text, "é-1\nü-2");
        assert_eq!(char_index(&text, 1, 2), 6);
        assert_eq!(line_col(&text, 6), (1, 2));
    }

    #[test]
    fn completes_keywords_tables_and_columns() {
        let v = vocab();
        let (start, c) = completions("SEL", 3, Lang::Sql, &v);
        assert_eq!((start, names(&c)), (0, vec!["SELECT"]));
        let (_, c) = completions("select * from us", 16, Lang::Sql, &v);
        assert_eq!(names(&c), vec!["users", "using"]);
        let text = "select p. from posts p";
        let (start, c) = completions(text, 9, Lang::Sql, &v);
        assert_eq!((start, names(&c)), (9, vec!["id", "title", "author_id"]));
        let (_, c) = completions("select users.na", 15, Lang::Sql, &v);
        assert_eq!(names(&c), vec!["name"]);
    }

    #[test]
    fn dsl_completion() {
        let v = vocab();
        let at = |t: &str| completions(t, t.chars().count(), Lang::Dbs, &v).1;
        assert_eq!(names(&at("table x {\n  author_id int -> p")), vec!["posts.id", "posts.title", "posts.author_id"]);
        assert_eq!(names(&at("table x {\n  author_id int -> posts.ti")), vec!["title"]);
        assert_eq!(names(&at("table x {\n  x times")), vec!["timestamp"]);
        assert!(at("table x {\n  x time").is_empty(), "a complete word keeps Enter for newlines");
        assert!(at("table x {\n  # a comment ti").is_empty());
        assert!(at("table x {\n  x text default 'ti").is_empty());
        // Modifiers that fit, and not twice.
        assert_eq!(names(&at("table x {\n  x int not null u")), vec!["unique"]);
        assert_eq!(names(&at("table x {\n  x int not null n")), Vec::<&str>::new());
        assert_eq!(names(&at("table x {\n  x int -> users.id o")), vec!["on delete", "on update"]);
        assert_eq!(names(&at("table x {\n  x int o")), Vec::<&str>::new(), "no reference, no on delete");
        assert_eq!(names(&at("table x {\n  x int -> users.id on delete c")), vec!["cascade"]);
        assert_eq!(names(&at("table x {\n  x int default n")), vec!["now", "null"]);
        // Statement starts: table-level keywords; between tables: the snippet.
        assert_eq!(names(&at("table x {\n  un")), vec!["unique index"]);
        assert_eq!(names(&at("table users {\n  id int\n  index (")), vec!["id", "name", "email"], "for Ctrl+Space");
        assert_eq!(names(&at("table users {\n  id int\n  index (na")), vec!["name"]);
        let c = at("table x {\n}\ntab");
        assert_eq!((names(&c), c[0].kind), (vec!["table"], Kind::Snippet));
        let mut text = "tab".to_string();
        let sel = accept(&mut text, 0, 3, &c[0]);
        assert_eq!(char_slice(&text, sel.0, sel.1), "name");
        assert!(at("table x {\n}\ntable y").is_empty());
    }

    fn press(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    /// Runs a command on `text` with the caret (or selection) marked by `|`.
    fn run(text: &str, event: Event, lang: Lang) -> String {
        let marks: Vec<usize> = text.chars().enumerate().filter(|(_, c)| *c == '|').map(|(i, _)| i).collect();
        let (anchor, head) = match marks[..] {
            [a] => (a, a),
            [a, h] => (a, h - 1),
            _ => panic!("one or two carets"),
        };
        let mut t = text.replace('|', "");
        let (a, h) = command(&mut t, (anchor, head), &event, lang).expect("handled");
        let (lo, hi) = (a.min(h), a.max(h));
        let mut out: Vec<char> = t.chars().collect();
        if lo != hi {
            out.insert(hi, '|');
        }
        out.insert(lo, '|');
        out.into_iter().collect()
    }

    #[test]
    fn enter_indents() {
        let enter = || press(Key::Enter, Modifiers::NONE);
        assert_eq!(run("table x {|", enter(), Lang::Dbs), "table x {\n  |\n}");
        assert_eq!(run("table x {|}", enter(), Lang::Dbs), "table x {\n  |\n}");
        assert_eq!(run("table x {|\n}", enter(), Lang::Dbs), "table x {\n  |\n}");
        assert_eq!(run("  id int|", enter(), Lang::Dbs), "  id int\n  |");
        assert_eq!(run("  |", enter(), Lang::Dbs), "\n  |");
        assert_eq!(run("select (|)", enter(), Lang::Sql), "select (\n  |\n)");
    }

    #[test]
    fn line_commands() {
        let none = Modifiers::NONE;
        assert_eq!(run("a\n|b\nc|", press(Key::Tab, none), Lang::Dbs), "a\n  |b\n  c|");
        assert_eq!(run("  a\n |b", press(Key::Tab, Modifiers::SHIFT), Lang::Dbs), "  a\n|b");
        assert_eq!(run("a|b", press(Key::Tab, none), Lang::Dbs), "a |b");
        assert_eq!(run("  |a\n\n  b|", press(Key::Slash, Modifiers::COMMAND), Lang::Dbs), "  # |a\n\n  # b|");
        assert_eq!(run("  # a|\n  // b", press(Key::Slash, Modifiers::COMMAND), Lang::Dbs), "  a|\n  // b");
        assert_eq!(run("  # |a\n  # b|", press(Key::Slash, Modifiers::COMMAND), Lang::Dbs), "  |a\n  b|");
        assert_eq!(run("sel|ect", press(Key::Slash, Modifiers::COMMAND), Lang::Sql), "-- sel|ect");
        assert_eq!(run("a|\nb", press(Key::D, Modifiers::COMMAND), Lang::Dbs), "a\na|\nb");
        assert_eq!(run("a\nb|\nc", press(Key::ArrowUp, Modifiers::ALT), Lang::Dbs), "b|\na\nc");
        assert_eq!(run("a\nb|\nc", press(Key::ArrowDown, Modifiers::ALT), Lang::Dbs), "a\nc\nb|");
        assert_eq!(run("|a\nb", press(Key::ArrowUp, Modifiers::ALT), Lang::Dbs), "|a\nb");
        assert_eq!(run("a\nb|b\nc", press(Key::K, Modifiers::COMMAND | Modifiers::SHIFT), Lang::Dbs), "a\nc|");
        assert_eq!(run("  x\n  |", Event::Text("}".into()), Lang::Dbs), "  x\n}|");
        let mut t = "a".to_string();
        assert_eq!(command(&mut t, (0, 0), &press(Key::K, Modifiers::COMMAND), Lang::Dbs), None, "Ctrl+K is the app's");
    }

    #[test]
    fn find_and_replace() {
        assert_eq!(find_all("Id id ID", "id", false), vec![(0, 2), (3, 5), (6, 8)]);
        assert_eq!(find_all("Id id ID", "id", true), vec![(3, 5)]);
        assert_eq!(find_all("aaa", "aa", false), vec![(0, 2)]);
        let mut t = "x y x y x".to_string();
        assert_eq!(find_step(&mut t, "x", "", false, (1, 1), FindAction::Search), Some((4, 5)));
        assert_eq!(find_step(&mut t, "x", "", false, (4, 5), FindAction::Next), Some((8, 9)));
        assert_eq!(find_step(&mut t, "x", "", false, (8, 9), FindAction::Next), Some((0, 1)), "wraps");
        assert_eq!(find_step(&mut t, "x", "", false, (0, 1), FindAction::Prev), Some((8, 9)));
        assert_eq!(find_step(&mut t, "x", "zz", false, (4, 5), FindAction::Replace), Some((9, 10)));
        assert_eq!(t, "x y zz y x");
        find_step(&mut t, "x", "w", false, (0, 0), FindAction::ReplaceAll);
        assert_eq!(t, "w y zz y w");
    }

    #[test]
    fn go_to_definition() {
        let text = "table users {\n  id int pk\n}\ntable posts {\n  author int -> users.id\n}";
        let at = |needle: &str| text.find(needle).map(|b| text[..b].chars().count() + 1).unwrap();
        assert_eq!(definition(text, at("users.id")), Some(1), "column");
        assert_eq!(definition(text, at("users.id") + 7), Some(1), "anywhere in the word");
        assert_eq!(definition(text, at("users {")), None, "its own definition");
        assert_eq!(definition(text, at("int ->")), None);
    }
}
