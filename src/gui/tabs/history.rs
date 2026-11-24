//! The History tab: a Laravel project's migrations, one step at a time. The
//! diagram shows the schema after the selected migration, what it changed in
//! color and the rest in grey; the panel on the right lists the changes.

use crate::er::{self, mark_color};
use crate::highlight::{self, Lang};
use crate::{App, theme, warn_color};
use eframe::egui::{self, Key, RichText};
use simple_database::laravel;
use simple_database::schema::Schema;
use simple_database::timeline::{Changes, Mark, Step, Warning, changes};
use simple_database::format;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// How often the folder is checked for new or edited migrations.
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HistoryView {
    #[default]
    Diagram,
    Schema,
    /// The migration's PHP source.
    File,
}

#[derive(Default)]
pub(crate) struct HistoryState {
    /// The migrations folder; `None` until one is found or picked.
    pub(crate) dir: Option<PathBuf>,
    /// The steps read from `dir` and each one's changes.
    loaded: Option<Result<Vec<(Step, Changes)>, String>>,
    /// File names and modification times when read, to notice edits.
    signature: Vec<(PathBuf, Option<SystemTime>)>,
    checked: Option<Instant>,
    selected: usize,
    /// The step shown last frame, to notice a new selection.
    shown_step: Option<usize>,
    filter: String,
    only_changed: bool,
    view: HistoryView,
    er: er::ErView,
    /// "Only changes" draws on its own canvas, laid out again for each step.
    er_changes: er::ErView,
    /// The diagram's boxes are placed for the whole history.
    arranged: bool,
    /// A folder was looked for (or the saved one opened) once.
    searched: bool,
    /// The `.dbs` text of the selected step, made when shown.
    text: Option<(usize, HistoryView, String)>,
    /// The step whose file the File view shows, to jump to its first warning once.
    file_shown: Option<usize>,
    /// A line of the file to scroll to (a warning clicked in the changes panel).
    goto_line: Option<usize>,
}

fn signature(dir: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("php")))
                .map(|p| {
                    let t = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                    (p, t)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// A step's warnings, one per line as `path:line: message` (what editors and
/// terminals open at the line), for the clipboard.
fn warning_text(step: &Step) -> String {
    let path = step.file.display();
    step.warnings
        .iter()
        .map(|w| match w.line {
            Some(n) => format!("{path}:{n}: ⚠ {}\n", w.message),
            None => format!("{path}: ⚠ {}\n", w.message),
        })
        .collect()
}

/// The migration's source with line numbers; lines with a warning are tinted
/// and show it on hover. Scrolls to `goto` (taken) once laid out.
fn file_view(ui: &mut egui::Ui, text: &str, warnings: &[Warning], goto: &mut Option<usize>) {
    let galley = highlight::layout(ui, text, Lang::Php);
    let warn = warn_color(ui);
    let number_font = egui::FontId::monospace(11.0);
    let gutter_w = 14.0 + 8.0 * galley.rows.len().to_string().len() as f32;
    let warned = |line: usize| warnings.iter().filter(move |w| w.line == Some(line));
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let (gutter, _) = ui.allocate_exact_size(egui::vec2(gutter_w, galley.size().y), egui::Sense::hover());
        let origin = egui::pos2(gutter.right() + 8.0, gutter.top());
        let right = origin.x + galley.size().x.max(ui.available_width());
        let painter = ui.painter().clone();
        let mut target = None;
        for (n, row) in galley.rows.iter().enumerate() {
            let line = n + 1;
            let r = row.rect().translate(origin.to_vec2());
            let band = egui::Rect::from_x_y_ranges(gutter.left()..=right, r.y_range());
            let hit = warned(line).next().is_some();
            if hit {
                painter.rect_filled(band, 0.0, warn.gamma_multiply(0.18));
                painter.rect_filled(egui::Rect::from_x_y_ranges(gutter.left()..=gutter.left() + 3.0, r.y_range()), 0.0, warn);
            }
            let color = if hit { warn } else { ui.visuals().weak_text_color() };
            painter.text(egui::pos2(gutter.right() - 4.0, r.center().y), egui::Align2::RIGHT_CENTER, line.to_string(), number_font.clone(), color);
            if *goto == Some(line) {
                target = Some(band);
            }
        }
        ui.add_space(8.0);
        let resp = ui.add(egui::Label::new(galley.clone()).selectable(true).wrap_mode(egui::TextWrapMode::Extend));
        // A warning line shows its messages on hover.
        if let Some(pos) = resp.hover_pos()
            && let Some(n) = galley.rows.iter().position(|row| row.rect().translate(origin.to_vec2()).y_range().contains(pos.y))
        {
            let text: Vec<String> = warned(n + 1).map(|w| format!("⚠ {}", w.message)).collect();
            if !text.is_empty() {
                resp.on_hover_text(text.join("\n"));
            }
        }
        if let Some(band) = target {
            ui.scroll_to_rect(band, Some(egui::Align::Center));
            *goto = None;
        }
    });
}

/// Every table any step has, as last seen, for one layout of the whole history.
fn every_table(steps: &[(Step, Changes)]) -> Schema {
    let mut all = Schema::default();
    for (_, c) in steps {
        for t in &c.shown.tables {
            match all.tables.iter_mut().find(|a| a.name == t.name) {
                Some(a) => *a = t.clone(),
                None => all.tables.push(t.clone()),
            }
        }
    }
    all
}

impl HistoryState {
    /// Reads the folder again; keeps the selected step, or follows the last one.
    fn load(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        let previous = self.steps().and_then(|s| s.get(self.selected)).map(|(s, _)| s.id.clone());
        let was_last = self.steps().is_some_and(|s| self.selected.saturating_add(1) >= s.len());
        self.signature = signature(&dir);
        self.checked = Some(Instant::now());
        self.text = None;
        self.loaded = Some(laravel::read_dir(&dir).map_err(|e| format!("{e:#}")).map(|steps| {
            let mut before = Schema::default();
            steps
                .into_iter()
                .map(|s| {
                    let c = changes(&before, &s.schema, &s.renames);
                    before = s.schema.clone();
                    (s, c)
                })
                .collect()
        }));
        let n = self.steps().map_or(0, <[_]>::len);
        self.selected = match previous.and_then(|id| self.steps()?.iter().position(|(s, _)| s.id == id)) {
            Some(i) if !was_last => i,
            _ => n.saturating_sub(1),
        };
    }

    fn steps(&self) -> Option<&[(Step, Changes)]> {
        self.loaded.as_ref().and_then(|r| r.as_ref().ok()).map(Vec::as_slice)
    }

    pub(crate) fn with_dir(dir: Option<PathBuf>) -> Self {
        HistoryState { dir, ..Default::default() }
    }

    /// Shows another folder from its last migration, forgetting the one shown before.
    pub(crate) fn open(&mut self, dir: PathBuf) {
        self.dir = Some(dir);
        self.loaded = None;
        self.selected = 0;
        self.shown_step = None;
        self.arranged = false;
        self.er_changes.reset_layout();
        self.load();
    }

    /// Re-reads the folder when its files changed, at most every [`WATCH_EVERY`].
    fn watch(&mut self, ctx: &egui::Context) {
        let Some(dir) = &self.dir else { return };
        ctx.request_repaint_after(WATCH_EVERY);
        if self.checked.is_some_and(|t| t.elapsed() < WATCH_EVERY) {
            return;
        }
        self.checked = Some(Instant::now());
        if signature(dir) != self.signature {
            let before = self.steps().map(|s| s.iter().map(|(s, _)| s.id.clone()).collect::<Vec<_>>());
            self.load();
            let after = self.steps().map(|s| s.iter().map(|(s, _)| s.id.clone()).collect::<Vec<_>>());
            if before != after {
                self.arranged = false;
            }
        }
    }
}

impl App {
    /// Where to look for migrations first: the schema file's folder, then the working directory.
    fn find_migrations(&self) -> Option<PathBuf> {
        let from_schema = self.schema_path.as_ref().and_then(|p| p.parent()).and_then(laravel::find_dir);
        from_schema.or_else(|| std::env::current_dir().ok().and_then(|d| laravel::find_dir(&d)))
    }

    pub(crate) fn tab_history(&mut self, ui: &mut egui::Ui) {
        if self.history.loaded.is_none() && !self.history.searched {
            self.history.searched = true;
            if let Some(dir) = self.history.dir.clone().or_else(|| self.find_migrations()) {
                self.history.open(dir);
            }
        }
        self.history.watch(ui.ctx());
        let h = &mut self.history;

        let mut pick = false;
        let mut copied = None;
        egui::Panel::top("history_toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                match &h.dir {
                    Some(d) => {
                        ui.label(RichText::new(d.display().to_string()).weak()).on_hover_text("Laravel migrations folder, watched for changes");
                    }
                    None => {
                        ui.label(RichText::new("No Laravel migrations folder").weak());
                    }
                }
                pick = ui.button("Open folder…").on_hover_text("A Laravel project, or its database/migrations folder").clicked();
                if h.dir.is_some() && ui.button("Reload").clicked() {
                    h.load();
                }
                let warned: Vec<&Step> = h.steps().unwrap_or_default().iter().map(|(s, _)| s).filter(|s| !s.warnings.is_empty()).collect();
                if !warned.is_empty()
                    && ui
                        .button(format!("Copy all warnings ({})", warned.len()))
                        .on_hover_text("Every migration with a warning: its file and the messages, as text")
                        .clicked()
                {
                    ui.ctx().copy_text(warned.iter().map(|s| warning_text(s)).collect::<Vec<_>>().join("\n"));
                    copied = Some(warned.len());
                }
            });
        });
        if let Some(n) = copied {
            self.set_status(format!("Copied the warnings of {n} migration{}", if n == 1 { "" } else { "s" }), false);
        }
        if pick && let Some(folder) = rfd::FileDialog::new().pick_folder() {
            match laravel::resolve_dir(&folder) {
                Some(dir) => self.history.open(dir),
                None => self.set_status(format!("No Laravel migrations in {}", folder.display()), true),
            }
            return;
        }
        let h = &mut self.history;
        let loaded = h.loaded.take();
        match &loaded {
            None => {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Explore how a Laravel project's data model evolved").size(16.0));
                    ui.label(RichText::new("Open a Laravel project (or its database/migrations folder): every migration becomes a step, drawn as a diagram with its changes in color.").weak());
                });
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
            }
            Some(Ok(s)) if s.is_empty() => {
                ui.label(RichText::new("The folder has no migrations yet.").weak());
            }
            Some(Ok(steps)) => copied = steps_view(h, ui, steps).then_some(1),
        }
        h.loaded = loaded;
        if copied.is_some() {
            self.set_status("Copied the warnings with the migration's file name", false);
        }
    }
}

/// The step list, the changes of the selected step and its diagram or schema.
/// Returns whether a step's warnings were copied.
fn steps_view(h: &mut HistoryState, ui: &mut egui::Ui, steps: &[(Step, Changes)]) -> bool {
    h.selected = h.selected.min(steps.len() - 1);
    if !h.arranged {
        h.er.arrange(ui, &every_table(steps));
        h.arranged = true;
    }

    // Up / Down step through the migrations while no text field has the keyboard.
    if ui.ctx().memory(|m| m.focused().is_none()) {
        let (up, down) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown)));
        if up {
            h.selected = h.selected.saturating_sub(1);
        }
        if down {
            h.selected = (h.selected + 1).min(steps.len() - 1);
        }
    }

    let dark = ui.visuals().dark_mode;
    egui::Panel::left("history_steps").resizable(true).default_size(290.0).min_size(180.0).show(ui, |ui| {
        ui.add_space(4.0);
        theme::search_field(ui, &mut h.filter, "Filter migrations or tables");
        ui.add_space(4.0);
        let needle = h.filter.to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, (step, c)) in steps.iter().enumerate() {
                if !needle.is_empty() && !step.id.to_lowercase().contains(&needle) && !c.tables.iter().any(|(t, _)| t.to_lowercase().contains(&needle)) {
                    continue;
                }
                let (added, altered, dropped) = c.counts();
                let resp = theme::list_row(ui, i == h.selected, |ui| {
                    ui.label(RichText::new(format!("{:>3}", i + 1)).monospace().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        for (n, sign, mark) in [(dropped, "−", Mark::Dropped), (altered, "~", Mark::Altered), (added, "+", Mark::Added)] {
                            if n > 0 {
                                ui.label(RichText::new(format!("{sign}{n}")).color(mark_color(mark, dark).unwrap()).small());
                            }
                        }
                        if !step.warnings.is_empty() {
                            ui.label(RichText::new("⚠").color(warn_color(ui)).small());
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(&step.title).truncate());
                        });
                    });
                });
                if i == h.selected && h.shown_step != Some(i) {
                    resp.scroll_to_me(None);
                }
                let mut hover = format!("{}{}", step.id, step.date.as_deref().map(|d| format!("\n{d}")).unwrap_or_default());
                for w in &step.warnings {
                    hover.push_str(&format!("\n⚠ {w}"));
                }
                if resp.on_hover_text(hover).clicked() {
                    h.selected = i;
                }
            }
        });
    });

    if h.shown_step != Some(h.selected) {
        h.er_changes.reset_layout();
        // Highlight what the migration touched first.
        h.er.selected = steps[h.selected].1.tables.first().map(|(t, _)| t.clone());
        h.er_changes.selected = h.er.selected.clone();
    }
    let (step, c) = &steps[h.selected];
    let mut copied_one = false;
    egui::Panel::right("history_changes").resizable(true).default_size(320.0).min_size(200.0).show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(RichText::new(&step.title).font(theme::bold(15.0)));
            ui.label(RichText::new(step.file.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()).weak().small());
            ui.add_space(6.0);
            // Warnings first: a long list of changes would push them out of sight.
            if !step.warnings.is_empty() {
                let warn = warn_color(ui);
                egui::Frame::new().fill(warn.gamma_multiply(0.12)).stroke(egui::Stroke::new(1.0, warn.gamma_multiply(0.5))).corner_radius(6).inner_margin(8).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("⚠ Not modeled: the diagram may miss what these do").color(warn).strong());
                    for w in &step.warnings {
                        let label = ui.add(egui::Label::new(RichText::new(format!("• {w}")).color(warn)).sense(egui::Sense::click()));
                        if let Some(line) = w.line
                            && label.on_hover_text(format!("Show line {line} of the migration file")).clicked()
                        {
                            h.view = HistoryView::File;
                            h.file_shown = Some(h.selected);
                            h.goto_line = Some(line);
                        }
                    }
                    ui.add_space(4.0);
                    if ui.button("Copy with file name").on_hover_text("The migration's path and these messages, as text").clicked() {
                        ui.ctx().copy_text(warning_text(step));
                        copied_one = true;
                    }
                });
                ui.add_space(8.0);
            }
            if c.tables.is_empty() {
                ui.label(RichText::new("No change to the schema.").weak());
            }
            for (table, m) in &c.tables {
                let color = mark_color(m.mark, dark).unwrap_or(ui.visuals().text_color());
                let sign = match m.mark {
                    Mark::Added => "+",
                    Mark::Dropped => "−",
                    _ => "~",
                };
                let count = if m.details.len() > 12 { format!("  ({} changes)", m.details.len()) } else { String::new() };
                let title = RichText::new(format!("{sign} {table}{count}")).color(color).strong();
                let header = if m.mark == Mark::Dropped || m.details.is_empty() {
                    ui.add(egui::Label::new(title).sense(egui::Sense::click()))
                } else {
                    // Long lists (a new table's columns) start folded.
                    egui::CollapsingHeader::new(title)
                        .id_salt((h.selected, table))
                        .default_open(m.details.len() <= 12)
                        .show(ui, |ui| {
                            for d in &m.details {
                                ui.label(RichText::new(d).small());
                            }
                        })
                        .header_response
                };
                if header.on_hover_text("Select it in the diagram").clicked() {
                    h.er.selected = Some(table.clone());
                    h.er_changes.selected = Some(table.clone());
                }
            }
        });
    });

    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let n = steps.len();
            if ui.add_enabled(h.selected > 0, egui::Button::new("◀")).on_hover_text("Previous migration (↑)").clicked() {
                h.selected -= 1;
            }
            if ui.add_enabled(h.selected + 1 < n, egui::Button::new("▶")).on_hover_text("Next migration (↓)").clicked() {
                h.selected += 1;
            }
            ui.label(format!("{} of {n}", h.selected + 1));
            if let Some(d) = &steps[h.selected].0.date {
                ui.label(RichText::new(d).weak());
            }
            ui.separator();
            let items = [(HistoryView::Diagram, RichText::new("Diagram"), "Tables after this migration; its changes in color"), (HistoryView::Schema, RichText::new("Schema"), "The schema after this migration, in the sdb language"), (HistoryView::File, RichText::new("File"), "The migration's PHP source")];
            if let (Some(v), _) = theme::segmented(ui, h.view, &items, false) {
                h.view = v;
            }
            if h.view == HistoryView::Diagram {
                ui.separator();
                let only = ui.checkbox(&mut h.only_changed, "Only changes").on_hover_text("Hide the tables this migration leaves alone, except those the changed ones reference");
                if only.changed() {
                    h.er_changes.reset_layout();
                }
                let canvas = if h.only_changed { &mut h.er_changes } else { &mut h.er };
                if ui.button("Fit").clicked() {
                    canvas.request_fit();
                }
                if ui.button("Auto layout").on_hover_text("Arrange the tables of this step only").clicked() {
                    canvas.reset_layout();
                }
                if !h.only_changed && ui.button("Whole-history layout").on_hover_text("Place every table the project ever had, so boxes keep their places from step to step").clicked() {
                    h.arranged = false;
                }
            }
            if ui.button("Copy schema").on_hover_text("The schema after this migration as sdb text").clicked() {
                ui.ctx().copy_text(format::format_schema(&steps[h.selected].0.schema));
            }
        });
        // The colors' key, for the diagram.
        if h.view == HistoryView::Diagram {
            ui.horizontal(|ui| {
                for (mark, label) in [(Mark::Added, "added"), (Mark::Altered, "changed"), (Mark::Dropped, "dropped")] {
                    ui.label(RichText::new("■").color(mark_color(mark, dark).unwrap()));
                    ui.label(RichText::new(label).small());
                }
                ui.label(RichText::new("■").color(ui.visuals().weak_text_color().gamma_multiply(0.5)));
                ui.label(RichText::new("as before").small());
            });
        }
        ui.add_space(4.0);
        let (step, c) = &steps[h.selected];
        match h.view {
            HistoryView::Diagram => {
                let mut shown = c.shown.clone();
                if h.only_changed {
                    let keep: Vec<String> = c
                        .tables
                        .iter()
                        .flat_map(|(t, _)| {
                            let parents = shown.table(t).map(|t| t.all_foreign_keys().into_iter().map(|fk| fk.table).collect::<Vec<_>>()).unwrap_or_default();
                            std::iter::once(t.clone()).chain(parents)
                        })
                        .collect();
                    shown.tables.retain(|t| keep.contains(&t.name));
                }
                let canvas = if h.only_changed { &mut h.er_changes } else { &mut h.er };
                canvas.marks = Some(c.clone());
                let _ = canvas.ui(ui, &shown, false, &Default::default());
            }
            HistoryView::Schema | HistoryView::File => {
                // Made once per step and view; a reload of the folder drops it, so edits show.
                if h.text.as_ref().is_none_or(|(i, v, _)| *i != h.selected || *v != h.view) {
                    let text = match h.view {
                        HistoryView::File => std::fs::read_to_string(&step.file)
                            .map(|t| t.replace("\r\n", "\n"))
                            .unwrap_or_else(|e| format!("// cannot read {}: {e}", step.file.display())),
                        _ => format::format_schema(&step.schema),
                    };
                    h.text = Some((h.selected, h.view, text));
                }
                let text = &h.text.as_ref().unwrap().2;
                if h.view == HistoryView::File {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(step.file.display().to_string()).weak().small());
                        if ui.small_button("Copy path").clicked() {
                            ui.ctx().copy_text(step.file.display().to_string());
                        }
                        if ui.small_button("Copy file").clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                    });
                    ui.add_space(4.0);
                }
                if h.view == HistoryView::File {
                    // Opening a file (or another step's) goes to its first warning.
                    if h.file_shown != Some(h.selected) {
                        h.file_shown = Some(h.selected);
                        h.goto_line = step.warnings.iter().find_map(|w| w.line);
                    }
                    let goto = &mut h.goto_line;
                    egui::ScrollArea::both().id_salt(("history_text", true)).auto_shrink([false, false]).show(ui, |ui| file_view(ui, text, &step.warnings, goto));
                } else {
                    egui::ScrollArea::both().id_salt(("history_text", false)).auto_shrink([false, false]).show(ui, |ui| highlight::code_view(ui, text, Lang::Dbs));
                }
            }
        }
    });
    if h.view != HistoryView::File {
        h.file_shown = None;
    }
    h.shown_step = Some(h.selected);
    copied_one
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, tables: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sdb-history-{name}-{}", std::process::id())).join("database").join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        for (i, t) in tables.iter().enumerate() {
            let php = format!("<?php\nreturn new class extends Migration {{ public function up(): void {{ Schema::create('{t}', function (Blueprint $table) {{ $table->id(); }}); }} }};\n");
            std::fs::write(dir.join(format!("2024_01_0{}_000000_create_{t}_table.php", i + 1)), php).unwrap();
        }
        dir
    }

    #[test]
    fn opening_another_project_starts_at_its_last_step() {
        let (a, b) = (project("a", &["users", "posts", "tags"]), project("b", &["orders"]));
        let mut h = HistoryState::default();
        h.open(a.clone());
        assert_eq!((h.steps().unwrap().len(), h.selected), (3, 2));
        h.selected = 0;
        h.open(b.clone());
        assert_eq!((h.steps().unwrap().len(), h.selected), (1, 0));
        h.open(a.clone());
        assert_eq!(h.selected, 2);
        for d in [a, b] {
            std::fs::remove_dir_all(d.ancestors().nth(2).unwrap()).unwrap();
        }
    }
}
