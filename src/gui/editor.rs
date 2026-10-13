//! The code editor: egui's `TextEdit` plus TextMate-style column selection
//! (one caret per line) and an autocomplete popup.
//!
//! Column selection: Alt+drag selects a rectangle; tapping Alt turns a
//! multi-line selection into a rectangle; Alt+Shift+↑/↓ adds a caret on the
//! line above/below. Typing, Backspace/Delete, ←/→, Home/End, copy, cut and
//! paste then act on every line. Esc, a plain click or any other key goes back
//! to a single caret.
//!
//! Autocomplete opens while typing a word, after `table.` and on Ctrl+Space.
//! ↑/↓ choose, Enter/Tab accept, Esc closes.

use crate::highlight::{self, DBS_KEYWORDS, DBS_TYPES, Lang, SQL_KEYWORDS};
use eframe::egui::{
    self, Event, FontId, Galley, Key, Pos2, RichText, Stroke,
    text::{CCursor, CCursorRange},
};
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
struct State {
    /// Column selection, sorted by line, one per line. Empty in normal editing.
    carets: Vec<Caret>,
    /// (line, virtual column) where an Alt+drag started.
    drag_from: Option<(usize, usize)>,
    alt_down: bool,
    /// Alt is down and nothing else happened since it went down.
    alt_tap: bool,
    popup: Option<Popup>,
    /// Last frame's layout, to map the pointer before `TextEdit` handles it.
    galley: Option<(Arc<Galley>, Pos2)>,
}

const FONT_SIZE: f32 = 13.5;

/// A monospace code editor with highlighting, column selection and autocomplete.
pub fn code_editor(
    ui: &mut egui::Ui,
    text: &mut String,
    lang: Lang,
    id_salt: &str,
    rows: usize,
    vocab: &Vocabulary,
) -> egui::Response {
    let id = ui.make_persistent_id(id_salt);
    let state_id = id.with("editor");
    let mut st: State = ui.data(|d| d.get_temp(state_id)).unwrap_or_default();
    let focused = ui.memory(|m| m.has_focus(id));
    if !focused {
        st.carets.clear();
        st.popup = None;
    }
    let mut te_state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    let mut cursor = te_state.cursor.char_range();
    let mut set_cursor: Option<CCursorRange> = None;
    let mut typed = false;

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
                                let end = replace_chars(text, start, at, &c.text);
                                let r = CCursorRange::one(CCursor::new(end));
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
                Event::Text(s) => typed = s.chars().all(|c| is_ident(c) || c == '.'),
                _ => {}
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
    let out = egui::TextEdit::multiline(text)
        .id(id)
        .code_editor()
        .desired_rows(rows)
        .desired_width(f32::INFINITY)
        .lock_focus(true)
        .layouter(&mut layouter)
        .show(ui);
    let response = out.response.response;
    let galley = out.galley;
    let origin = out.galley_pos;
    let char_w = ui.fonts_mut(|f| f.glyph_width(&FontId::monospace(FONT_SIZE), ' '));
    let to_line_col = |p: Pos2| {
        let (line, _) = line_col(text, galley.cursor_from_pos(p - origin).index.0);
        (line, ((p.x - origin.x) / char_w).round().max(0.0) as usize)
    };

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
                        let end = replace_chars(text, start, at, &cands[chosen].text);
                        let mut s = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                        s.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(end))));
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
    ui.data_mut(|d| d.insert_temp(state_id, st));
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
        Event::Text(s) if !s.is_empty() && !s.contains(['\n', '\r']) => {
            splice(text, carets, |_, c, _| (c.range().0, c.range().1, s.clone()));
        }
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
                _ => return Column::Exit { consume: false },
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

// ---------------------------------------------------------------- autocomplete

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Column,
    Table,
    Keyword,
    Type,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Column => "column",
            Kind::Table => "table",
            Kind::Keyword => "keyword",
            Kind::Type => "type",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    text: String,
    kind: Kind,
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
    let mut add = |names: &mut dyn Iterator<Item = String>, kind| pool.extend(names.map(|text| Candidate { text, kind }));
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
    } else {
        let before: String = chars[..start].iter().collect();
        let after_arrow = lang == Lang::Dbs && before.trim_end().ends_with("->");
        let upper = prefix.starts_with(|c: char| c.is_uppercase());
        let case = |w: &&str| if upper { w.to_uppercase() } else { w.to_string() };
        add(&mut vocab.tables.iter().map(|(t, _)| t.clone()), Kind::Table);
        if !after_arrow {
            if lang == Lang::Sql {
                let mut cols: Vec<String> = vocab.tables.iter().flat_map(|(_, c)| c.iter().cloned()).collect();
                cols.sort();
                cols.dedup();
                add(&mut cols.into_iter(), Kind::Column);
            }
            let keywords = if lang == Lang::Sql { SQL_KEYWORDS } else { DBS_KEYWORDS };
            add(&mut keywords.iter().map(case), Kind::Keyword);
            add(&mut DBS_TYPES.iter().map(case), Kind::Type);
        }
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
        let (_, c) = completions("  author_id int -> p", 20, Lang::Dbs, &v);
        assert_eq!(names(&c), vec!["posts"]);
        let (_, c) = completions("  x times", 9, Lang::Dbs, &v);
        assert_eq!(names(&c), vec!["timestamp"]);
        let (_, c) = completions("  x time", 8, Lang::Dbs, &v);
        assert!(c.is_empty(), "a complete word keeps Enter for newlines");
        let (_, c) = completions("  # a comment ti", 16, Lang::Dbs, &v);
        assert!(c.is_empty());
        let (_, c) = completions("  x text default 'ti", 20, Lang::Dbs, &v);
        assert!(c.is_empty());
    }
}
