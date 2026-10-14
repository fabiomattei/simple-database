//! The Data tab: a table page by page, with filters and sorting, edited in
//! place. Edits, new rows and deletions are staged until Save, which applies
//! them in one transaction.

use crate::grid::number_color;
use eframe::egui::{self, Color32, Key, RichText};
use egui_extras::{Column, TableBuilder};
use simple_database::data::{self, Filter, Op, RowChange, View};
use simple_database::db::{Cell, QueryResult};
use simple_database::ddl::Dialect;
use simple_database::output::ExportFormat;
use simple_database::schema::{Schema, Table};
use serde::{Deserialize, Serialize};
use simple_database::connections::config_dir;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub const PAGE_SIZE: usize = 200;

/// Changes not saved yet. Rows are indexes into the loaded page; new rows
/// come after them.
#[derive(Default)]
pub struct Staged {
    /// (row, column) → new value.
    pub edits: BTreeMap<(usize, usize), Cell>,
    pub deleted: BTreeSet<usize>,
    /// Per column, `None` = the column's default.
    pub inserted: Vec<Vec<Option<Cell>>>,
}

impl Staged {
    pub fn count(&self) -> usize {
        let edited_rows: BTreeSet<usize> = self.edits.keys().map(|(r, _)| *r).filter(|r| !self.deleted.contains(r)).collect();
        edited_rows.len() + self.deleted.len() + self.inserted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Deletes, then updates, then inserts.
    pub fn changes(&self, t: &Table, r: &QueryResult) -> Result<Vec<RowChange>, String> {
        let key = |row: usize| {
            data::row_key(t, &r.columns, &r.rows[row]).ok_or_else(|| format!("{} has no primary key; rows can't be changed", t.name))
        };
        let mut out = Vec::new();
        for &row in &self.deleted {
            out.push(RowChange::Delete { key: key(row)? });
        }
        let mut updates: BTreeMap<usize, Vec<(String, Cell)>> = BTreeMap::new();
        for (&(row, col), v) in &self.edits {
            if !self.deleted.contains(&row) && row < r.rows.len() {
                updates.entry(row).or_default().push((r.columns[col].clone(), v.clone()));
            }
        }
        for (row, values) in updates {
            out.push(RowChange::Update { key: key(row)?, values });
        }
        for new in &self.inserted {
            let values = new.iter().enumerate().filter_map(|(c, v)| Some((r.columns[c].clone(), v.clone()?))).collect();
            out.push(RowChange::Insert { values });
        }
        Ok(out)
    }
}

struct Editing {
    row: usize,
    col: usize,
    text: String,
    focus: bool,
}

/// What the tab asks the app to do.
pub enum DataAction {
    /// Load the page again (table, page or view changed, or Refresh).
    Load,
    Save(Vec<RowChange>),
    Query(String),
    Import,
    Export(ExportFormat),
}

/// The tables open in the Data tab for one database, kept between sessions.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedTabs {
    /// The database's label: a file path, a URL without password, or `@name`.
    pub db: String,
    pub tables: Vec<String>,
    pub active: usize,
}

/// Remember `s`, most recent first, for the last `MAX_SAVED_TABS` databases.
pub fn remember(list: &mut Vec<SavedTabs>, s: SavedTabs) {
    list.retain(|o| o.db != s.db);
    list.insert(0, s);
    list.truncate(MAX_SAVED_TABS);
}

const MAX_SAVED_TABS: usize = 20;

/// Every window shares the saved tabs through this file, and merges in only its own database.
fn saved_tabs_file() -> PathBuf {
    config_dir().join("data_tabs.json")
}

/// The saved tabs of all databases; `None` while there is no file yet.
pub fn load_saved() -> Option<Vec<SavedTabs>> {
    let text = std::fs::read_to_string(saved_tabs_file()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Remember `s` in the shared file (`list` without one), and return the list now saved.
pub fn store_saved(list: &[SavedTabs], s: SavedTabs) -> anyhow::Result<Vec<SavedTabs>> {
    let mut fresh = load_saved().unwrap_or_else(|| list.to_vec());
    if fresh.first() == Some(&s) {
        return Ok(fresh);
    }
    remember(&mut fresh, s);
    let path = saved_tabs_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&fresh)?)?;
    Ok(fresh)
}

/// The Data tab's open tables, one `DataView` each, so every table keeps its
/// page, filters and staged changes while another one is shown.
#[derive(Default)]
pub struct DataTabs {
    pub views: Vec<DataView>,
    pub active: usize,
}

impl DataTabs {
    pub fn current(&self) -> Option<&DataView> {
        self.views.get(self.active)
    }

    pub fn current_mut(&mut self) -> Option<&mut DataView> {
        self.views.get_mut(self.active)
    }

    /// The shown table.
    pub fn table(&self) -> Option<&str> {
        self.current().and_then(|v| v.table.as_deref())
    }

    pub fn view_mut(&mut self, table: &str) -> Option<&mut DataView> {
        self.views.iter_mut().find(|v| v.table.as_deref() == Some(table))
    }

    /// Show `table`, in a new tab unless one already has it.
    pub fn show(&mut self, table: String) {
        match self.views.iter().position(|v| v.table.as_ref() == Some(&table)) {
            Some(i) => self.active = i,
            None => {
                let mut v = DataView::default();
                v.open(Some(table));
                self.views.push(v);
                self.active = self.views.len() - 1;
            }
        }
    }

    /// Only `table` open, or nothing: another database.
    pub fn start(&mut self, table: Option<String>) {
        *self = DataTabs::default();
        if let Some(t) = table {
            self.show(t);
        }
    }

    /// The open tables of database `db`, to restore next time.
    pub fn saved(&self, db: &str) -> SavedTabs {
        SavedTabs { db: db.to_owned(), tables: self.views.iter().filter_map(|v| v.table.clone()).collect(), active: self.active }
    }

    /// Open the saved tables that still exist; without any, the first table.
    pub fn restore(&mut self, saved: Option<&SavedTabs>, schema: &Schema) {
        *self = DataTabs::default();
        let Some(s) = saved else { return self.refresh(schema) };
        let shown = s.tables.get(s.active);
        for t in s.tables.iter().filter(|t| schema.table(t).is_some()) {
            self.show(t.clone());
        }
        self.active = shown.and_then(|t| self.views.iter().position(|v| v.table.as_ref() == Some(t))).unwrap_or(0);
        if self.views.is_empty() {
            self.refresh(schema);
        }
    }

    pub fn close(&mut self, i: usize) {
        if i >= self.views.len() {
            return;
        }
        self.views.remove(i);
        if self.active > i || self.active >= self.views.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    /// After the database was read again: close the tabs of tables that are
    /// gone, reload the others unless they have changes waiting. With no tab
    /// left, the first table opens.
    pub fn refresh(&mut self, schema: &Schema) {
        let shown = self.table().map(str::to_owned);
        self.views.retain(|v| v.table.as_deref().is_some_and(|t| schema.table(t).is_some()));
        if self.views.is_empty() {
            return self.start(schema.tables.first().map(|t| t.name.clone()));
        }
        self.active = shown.and_then(|t| self.views.iter().position(|v| v.table.as_ref() == Some(&t))).unwrap_or(0);
        for v in &mut self.views {
            if v.pending() == 0 {
                v.reset();
            }
        }
    }

    /// The tab strip: one tab per open table, ✖ to close, ➕ to open another.
    /// `tables`: (name, rows).
    pub fn bar(&mut self, ui: &mut egui::Ui, tables: &[(String, i64)]) {
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("data_tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, v) in self.views.iter().enumerate() {
                    let name = v.table.as_deref().unwrap_or_default();
                    let pending = v.pending() > 0;
                    let text = if pending { format!("• {name}") } else { name.to_owned() };
                    let tab = ui.selectable_label(i == self.active, text);
                    if tab.clicked() {
                        self.active = i;
                    }
                    if tab.middle_clicked() && !pending {
                        close = Some(i);
                    }
                    let x = ui
                        .add_enabled(!pending, egui::Button::new(RichText::new("✖").small()).frame(false))
                        .on_hover_text("Close")
                        .on_disabled_hover_text("Save or discard the changes to this table first");
                    if x.clicked() {
                        close = Some(i);
                    }
                    ui.separator();
                }
                ui.menu_button("➕", |ui| {
                    egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                        for (t, n) in tables {
                            if ui.button(format!("{t}  ({n})")).clicked() {
                                self.show(t.clone());
                                ui.close();
                            }
                        }
                    });
                })
                .response
                .on_hover_text("Open another table");
            });
        });
        if let Some(i) = close {
            self.close(i);
        }
    }
}

#[derive(Default)]
pub struct DataView {
    pub table: Option<String>,
    pub page: usize,
    /// The filters being edited; `view` is what the page was loaded with.
    draft: View,
    pub view: View,
    show_filters: bool,
    pub out: Option<Result<QueryResult, String>>,
    /// Rows matching the view.
    pub total: usize,
    pub staged: Staged,
    editing: Option<Editing>,
    selected: BTreeSet<usize>,
    show_sql: bool,
    pub error: Option<String>,
}

impl DataView {
    /// Show `table` from its first page, without filters.
    pub fn open(&mut self, table: Option<String>) {
        *self = DataView { show_filters: self.show_filters, ..DataView::default() };
        self.table = table;
    }

    /// Forget the loaded page and staged changes; the next frame reloads.
    pub fn reset(&mut self) {
        self.out = None;
        self.staged = Staged::default();
        self.editing = None;
        self.selected.clear();
        self.error = None;
    }

    pub fn pending(&self) -> usize {
        self.staged.count()
    }

    /// `tables`: (name, rows); `table`: the shown table's definition.
    pub fn ui(&mut self, ui: &mut egui::Ui, tables: &[(String, i64)], table: Option<&Table>, d: Dialect, read_only: bool) -> Option<DataAction> {
        let mut action = None;
        let current = self.table.clone().unwrap_or_default();
        let pending = !self.staged.is_empty();
        let locked = "Save or discard your changes first";
        let pages = self.total.div_ceil(PAGE_SIZE).max(1);
        let has_key = table.is_some_and(|t| {
            let pk = t.primary_key();
            !pk.is_empty() && self.result().is_some_and(|r| pk.iter().all(|k| r.columns.contains(k)))
        });
        let can_edit = !read_only && has_key;

        ui.horizontal(|ui| {
            ui.add_enabled_ui(!pending, |ui| {
                if ui.add_enabled(self.page > 0, egui::Button::new("◀")).clicked() {
                    self.page -= 1;
                    self.reset();
                    action = Some(DataAction::Load);
                }
                ui.label(format!("page {} of {pages}", self.page + 1));
                if ui.add_enabled(self.page + 1 < pages, egui::Button::new("▶")).clicked() {
                    self.page += 1;
                    self.reset();
                    action = Some(DataAction::Load);
                }
                let all = tables.iter().find(|(t, _)| *t == current).map_or(0, |(_, n)| *n as usize);
                let count = if self.view.filters.is_empty() && self.view.where_sql.trim().is_empty() {
                    format!("{} rows", self.total)
                } else {
                    format!("{} of {all} rows", self.total)
                };
                ui.label(RichText::new(count).weak());
                ui.separator();
                if ui.button("⟳").on_hover_text("Reload").clicked() {
                    self.reset();
                    action = Some(DataAction::Load);
                }
            })
            .response
            .on_disabled_hover_text(locked);
            let n = self.view.filters.len() + usize::from(!self.view.where_sql.trim().is_empty());
            let label = if n > 0 { format!("🔍 Filter ({n})") } else { "🔍 Filter".into() };
            ui.toggle_value(&mut self.show_filters, label);
            ui.separator();
            let add = ui
                .add_enabled(!read_only && table.is_some() && self.result().is_some(), egui::Button::new("➕ Row"))
                .on_hover_text("Add a row; empty cells get their default")
                .on_disabled_hover_text(if read_only { "The connection is read-only" } else { "" });
            if add.clicked()
                && let Some((cols, rows)) = self.result().map(|r| (r.columns.len(), r.rows.len()))
            {
                self.staged.inserted.push(vec![None; cols]);
                // Start in the first column the database doesn't fill itself.
                let first = self.result().and_then(|r| {
                    r.columns.iter().position(|c| table.and_then(|t| t.column(c)).is_none_or(|c| !c.auto_increment))
                });
                self.start_edit(rows + self.staged.inserted.len() - 1, first.unwrap_or(0));
            }
            let sel = self.selected.len();
            let del_label = if sel > 0 { format!("🗑 Delete {sel}") } else { "🗑 Delete".into() };
            if ui
                .add_enabled(can_edit && sel > 0, egui::Button::new(del_label))
                .on_hover_text("Mark the selected rows for deletion (click row numbers to select)")
                .clicked()
            {
                for row in std::mem::take(&mut self.selected) {
                    self.toggle_delete(row);
                }
            }
            ui.separator();
            if ui.add_enabled(!read_only, egui::Button::new("Import…")).on_hover_text("Insert rows from CSV, TSV, JSON or JSON Lines").clicked() {
                action = Some(DataAction::Import);
            }
            ui.menu_button("Export ⏷", |ui| {
                ui.label(RichText::new("All matching rows").small().weak());
                for f in ExportFormat::ALL {
                    if ui.button(format!("{}…", f.label())).clicked() {
                        action = Some(DataAction::Export(f));
                        ui.close();
                    }
                }
            });
            if ui.button("Query this table").clicked()
                && let Some(t) = table
                && let Ok(sql) = self.view.page_sql(t, 100, 0, d)
            {
                action = Some(DataAction::Query(format!("{sql};")));
            }
        });

        if self.show_filters
            && let Some(t) = table
            && let Some(a) = self.filter_bar(ui, t, pending)
        {
            action = Some(a);
        }

        if pending {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().warn_fg_color, format!("• {} pending change(s)", self.pending()));
                ui.toggle_value(&mut self.show_sql, "Show SQL");
                if ui.button("Discard").clicked() {
                    self.staged = Staged::default();
                    self.editing = None;
                    self.error = None;
                }
                let save = ui.button(RichText::new("💾 Save changes").strong()).on_hover_text("Apply all changes in one transaction");
                if save.clicked()
                    && let (Some(t), Some(r)) = (table, self.result())
                {
                    match self.staged.changes(t, r).and_then(|c| {
                        // Check every value before anything runs.
                        c.iter().try_for_each(|c| c.sql(t, d).map(drop)).map_err(|e| format!("{e:#}"))?;
                        Ok(c)
                    }) {
                        Ok(c) => action = Some(DataAction::Save(c)),
                        Err(e) => self.error = Some(e),
                    }
                }
            });
            if self.show_sql
                && let (Some(t), Some(r)) = (table, self.result())
            {
                let sql = match self.staged.changes(t, r) {
                    Ok(changes) => changes
                        .iter()
                        .map(|c| c.sql(t, d).map_or_else(|e| format!("-- {e:#}"), |s| format!("{s};")))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => format!("-- {e}"),
                };
                egui::ScrollArea::both().id_salt("staged_sql").max_height(140.0).show(ui, |ui| {
                    crate::highlight::code_view(ui, &sql, crate::highlight::Lang::Sql);
                });
            }
        }
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
        }
        if read_only {
            ui.label(RichText::new("🔒 Read-only connection: editing is off").small().weak());
        } else if !has_key && table.is_some() && self.result().is_some() {
            ui.label(RichText::new("This table has no primary key: you can add rows, but not edit or delete them").small().weak());
        } else if self.result().is_some() {
            ui.label(RichText::new("Double-click a cell to edit · right-click for more · click row numbers to select").small().weak());
        }
        ui.separator();

        match &self.out {
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
            }
            Some(Ok(_)) => {
                if let Some(a) = self.grid(ui, table, d, can_edit, !read_only) {
                    action = Some(a);
                }
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Loading…").weak());
                });
            }
        }
        action
    }

    fn result(&self) -> Option<&QueryResult> {
        self.out.as_ref().and_then(|r| r.as_ref().ok())
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui, t: &Table, pending: bool) -> Option<DataAction> {
        let mut apply = false;
        let mut remove = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            for (i, f) in self.draft.filters.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt(("filter_col", i)).selected_text(&f.column).width(140.0).show_ui(ui, |ui| {
                        for c in &t.columns {
                            ui.selectable_value(&mut f.column, c.name.clone(), &c.name);
                        }
                    });
                    egui::ComboBox::from_id_salt(("filter_op", i)).selected_text(f.op.label()).width(110.0).show_ui(ui, |ui| {
                        for op in Op::ALL {
                            ui.selectable_value(&mut f.op, op, op.label());
                        }
                    });
                    if f.op.takes_value() {
                        let hint = if f.op == Op::In { "a, b, c" } else { "value" };
                        let r = ui.add(egui::TextEdit::singleline(&mut f.value).hint_text(hint).desired_width(220.0));
                        apply |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    }
                    if ui.small_button("✖").clicked() {
                        remove = Some(i);
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui.button("➕ Filter").clicked()
                    && let Some(c) = t.columns.first()
                {
                    self.draft.filters.push(Filter { column: c.name.clone(), op: Op::Eq, value: String::new() });
                }
                ui.label("WHERE");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.draft.where_sql)
                        .hint_text("any SQL condition")
                        .code_editor()
                        .desired_width(300.0),
                );
                apply |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_enabled_ui(!pending, |ui| {
                    apply |= ui.button(RichText::new("Apply").strong()).clicked();
                    if ui.button("Clear").clicked() {
                        self.draft = View { sort: self.view.sort.clone(), ..View::default() };
                        apply = true;
                    }
                });
            });
        });
        if let Some(i) = remove {
            self.draft.filters.remove(i);
        }
        if apply && !pending {
            self.draft.sort = self.view.sort.clone();
            self.view = self.draft.clone();
            self.page = 0;
            self.reset();
            return Some(DataAction::Load);
        }
        None
    }

    fn start_edit(&mut self, row: usize, col: usize) {
        let text = match self.value(row, col) {
            Some(Cell::Null) | None => String::new(),
            Some(c) => c.display(),
        };
        self.editing = Some(Editing { row, col, text, focus: true });
    }

    /// The value shown at (row, col); `None` = DEFAULT (new rows).
    fn value(&self, row: usize, col: usize) -> Option<Cell> {
        let r = self.result()?;
        if row < r.rows.len() {
            return Some(self.staged.edits.get(&(row, col)).unwrap_or(&r.rows[row][col]).clone());
        }
        self.staged.inserted.get(row - r.rows.len())?.get(col)?.clone()
    }

    /// Stage `v` at (row, col); `None` = DEFAULT, for new rows only.
    fn set(&mut self, row: usize, col: usize, v: Option<Cell>) {
        let Some(r) = self.out.as_ref().and_then(|r| r.as_ref().ok()) else { return };
        let n = r.rows.len();
        if row < n {
            let Some(v) = v else { return };
            if r.rows[row][col] == v {
                self.staged.edits.remove(&(row, col));
            } else {
                self.staged.edits.insert((row, col), v);
            }
        } else if let Some(cell) = self.staged.inserted.get_mut(row - n).and_then(|r| r.get_mut(col)) {
            *cell = v;
        }
    }

    fn toggle_delete(&mut self, row: usize) {
        let n = self.result().map_or(0, |r| r.rows.len());
        if row >= n {
            if row - n < self.staged.inserted.len() {
                self.staged.inserted.remove(row - n);
            }
        } else if !self.staged.deleted.remove(&row) {
            self.staged.deleted.insert(row);
        }
    }

    /// Commit the cell being edited, if its text fits the column's type.
    fn commit(&mut self, table: Option<&Table>, d: Dialect) {
        let Some(e) = self.editing.take() else { return };
        let Some(r) = self.result() else { return };
        let ty = table.and_then(|t| t.column(&r.columns[e.col])).map(|c| &c.data_type);
        let original = self.value(e.row, e.col);
        // Leaving an untouched NULL or DEFAULT cell keeps it.
        let unchanged = e.text.is_empty() && matches!(original, None | Some(Cell::Null));
        if unchanged {
            self.error = None;
            return;
        }
        let v = Cell::Text(e.text.clone());
        match data::literal(&v, ty, d) {
            Ok(_) => {
                // Keep the original typed value when the text didn't change.
                let same = original.as_ref().is_some_and(|o| *o != Cell::Null && o.display() == e.text);
                if !same {
                    self.set(e.row, e.col, Some(v));
                }
                self.error = None;
            }
            Err(err) => {
                self.error = Some(format!("{}: {err:#}", r.columns[e.col]));
                self.editing = Some(Editing { focus: true, ..e });
            }
        }
    }

    fn grid(&mut self, ui: &mut egui::Ui, table: Option<&Table>, d: Dialect, can_edit: bool, can_insert: bool) -> Option<DataAction> {
        let r = self.result()?.clone();
        if r.columns.is_empty() {
            return None;
        }
        let n = r.rows.len();
        let total_rows = n + self.staged.inserted.len();
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 6.0;
        let weak = ui.visuals().weak_text_color();
        let edited_bg = ui.visuals().warn_fg_color.gamma_multiply(0.18);
        let deleted_bg = ui.visuals().error_fg_color.gamma_multiply(0.15);
        let new_bg = Color32::from_rgb(0x3f, 0xb9, 0x50).gamma_multiply(0.15);
        let mut sort_by = None;
        let mut start_edit = None;
        let mut commit = false;
        let mut cancel = false;
        let mut set: Option<(usize, usize, Option<Cell>)> = None;
        let mut delete = None;
        let mut duplicate = None;
        let mut select: Option<(usize, bool)> = None;

        let pk = table.map(|t| t.primary_key()).unwrap_or_default();
        let sort = self.view.sort.clone();
        egui::ScrollArea::horizontal().id_salt("data_grid_h").show(ui, |ui| {
            let mut tb = TableBuilder::new(ui)
                .id_salt(("data_grid", self.table.clone(), self.page))
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .auto_shrink([false, true])
                .min_scrolled_height(0.0)
                .max_scroll_height(f32::INFINITY)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(52.0));
            for _ in &r.columns {
                tb = tb.column(Column::auto_with_initial_suggestion(130.0).at_least(40.0).at_most(600.0).clip(true));
            }
            tb.header(row_h + 2.0, |mut header| {
                header.col(|ui| {
                    ui.label(RichText::new("#").weak());
                });
                for c in &r.columns {
                    header.col(|ui| {
                        let arrow = match &sort {
                            Some((s, desc)) if s == c => if *desc { " ⏷" } else { " ⏶" },
                            _ => "",
                        };
                        let key = if pk.contains(c) { "🔑" } else { "" };
                        let label = egui::Label::new(RichText::new(format!("{key}{c}{arrow}")).strong()).sense(egui::Sense::click());
                        if ui.add(label).on_hover_text("Click to sort").clicked() {
                            sort_by = Some(c.clone());
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, total_rows, |mut row| {
                    let i = row.index();
                    let is_new = i >= n;
                    let deleted = self.staged.deleted.contains(&i);
                    row.set_selected(self.selected.contains(&i));
                    row.col(|ui| {
                        let text = if is_new {
                            RichText::new("+").strong().color(Color32::from_rgb(0x3f, 0xb9, 0x50))
                        } else {
                            RichText::new((self.page * PAGE_SIZE + i + 1).to_string()).weak().monospace()
                        };
                        let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                        if resp.clicked() {
                            select = Some((i, ui.input(|inp| inp.modifiers.command || inp.modifiers.shift)));
                        }
                    });
                    for col in 0..r.columns.len() {
                        let (_, resp) = row.col(|ui| {
                            let bg = if deleted {
                                Some(deleted_bg)
                            } else if is_new {
                                Some(new_bg)
                            } else if self.staged.edits.contains_key(&(i, col)) {
                                Some(edited_bg)
                            } else {
                                None
                            };
                            if let Some(bg) = bg {
                                ui.painter().rect_filled(ui.max_rect().expand2(egui::vec2(4.0, 2.0)), 0.0, bg);
                            }
                            if let Some(e) = self.editing.as_mut().filter(|e| e.row == i && e.col == col) {
                                let te = ui.add(egui::TextEdit::singleline(&mut e.text).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
                                if std::mem::take(&mut e.focus) {
                                    te.request_focus();
                                }
                                if ui.input(|inp| inp.key_pressed(Key::Escape)) {
                                    cancel = true;
                                } else if te.lost_focus() {
                                    commit = true;
                                }
                                return;
                            }
                            let value = if is_new {
                                self.staged.inserted[i - n][col].clone()
                            } else {
                                Some(self.staged.edits.get(&(i, col)).unwrap_or(&r.rows[i][col]).clone())
                            };
                            let mut text = match &value {
                                None => RichText::new("DEFAULT").italics().color(weak),
                                Some(Cell::Null) => RichText::new("NULL").italics().color(weak),
                                Some(c) if c.is_number() => RichText::new(c.display()).monospace().color(number_color(ui)),
                                Some(c) => RichText::new(c.display().replace('\n', "⏎")).monospace(),
                            };
                            if deleted {
                                text = text.strikethrough();
                            }
                            let hover = value.as_ref().map_or_else(|| "DEFAULT".to_string(), Cell::display);
                            ui.add(egui::Label::new(text).truncate().selectable(false)).on_hover_text(hover);
                        });
                        let editable = (can_edit && !deleted) || is_new;
                        if resp.double_clicked() && editable {
                            start_edit = Some((i, col));
                        }
                        resp.context_menu(|ui| {
                            let original = (!is_new).then(|| r.rows[i][col].clone());
                            if ui.add_enabled(editable, egui::Button::new("Edit")).clicked() {
                                start_edit = Some((i, col));
                                ui.close();
                            }
                            if ui.add_enabled(editable, egui::Button::new("Set NULL")).clicked() {
                                set = Some((i, col, Some(Cell::Null)));
                                ui.close();
                            }
                            if is_new && ui.button("Set DEFAULT").clicked() {
                                set = Some((i, col, None));
                                ui.close();
                            }
                            if let Some(o) = original
                                && self.staged.edits.contains_key(&(i, col))
                                && ui.button("Revert cell").clicked()
                            {
                                set = Some((i, col, Some(o)));
                                ui.close();
                            }
                            if ui.button("Copy value").clicked() {
                                let v = if is_new { self.staged.inserted[i - n][col].clone() } else { Some(r.rows[i][col].clone()) };
                                ui.ctx().copy_text(v.map_or_else(String::new, |c| c.display()));
                                ui.close();
                            }
                            ui.separator();
                            if can_insert && ui.button("Duplicate row").clicked() {
                                duplicate = Some(i);
                                ui.close();
                            }
                            let del = if is_new { "Remove new row" } else if deleted { "Restore row" } else { "Delete row" };
                            if ui.add_enabled(can_edit || is_new, egui::Button::new(del)).clicked() {
                                delete = Some(i);
                                ui.close();
                            }
                        });
                    }
                });
            });
        });

        if cancel {
            self.editing = None;
            self.error = None;
        } else if commit {
            self.commit(table, d);
        }
        if let Some((row, col)) = start_edit {
            self.commit(table, d);
            if self.editing.is_none() {
                self.start_edit(row, col);
            }
        }
        if let Some((row, col, v)) = set {
            self.set(row, col, v);
        }
        if let Some(row) = delete {
            self.toggle_delete(row);
            self.selected.remove(&row);
        }
        if let Some(row) = duplicate {
            // Copy the values, leaving auto-increment columns to the database.
            let copy: Vec<Option<Cell>> = (0..r.columns.len())
                .map(|c| {
                    let auto = table.and_then(|t| t.column(&r.columns[c])).is_some_and(|col| col.auto_increment);
                    if auto { None } else { self.value(row, c) }
                })
                .collect();
            self.staged.inserted.push(copy);
        }
        if let Some((row, add)) = select {
            if !add {
                let only = self.selected.len() == 1 && self.selected.contains(&row);
                self.selected.clear();
                if !only {
                    self.selected.insert(row);
                }
            } else if !self.selected.remove(&row) {
                self.selected.insert(row);
            }
        }
        if self.editing.is_none()
            && can_edit
            && !self.selected.is_empty()
            && ui.input(|i| i.key_pressed(Key::Delete))
            && !ui.ctx().egui_wants_keyboard_input()
        {
            for row in std::mem::take(&mut self.selected) {
                self.toggle_delete(row);
            }
        }
        if let Some(col) = sort_by {
            if !self.staged.is_empty() {
                self.error = Some("Save or discard your changes before sorting".into());
                return None;
            }
            self.view.sort = match &self.view.sort {
                Some((c, false)) if *c == col => Some((col, true)),
                Some((c, true)) if *c == col => None,
                _ => Some((col, false)),
            };
            self.page = 0;
            self.reset();
            return Some(DataAction::Load);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_database::parser::parse;

    #[test]
    fn tabs_open_close_and_refresh() {
        let mut tabs = DataTabs::default();
        tabs.start(Some("a".into()));
        tabs.show("b".into());
        tabs.show("c".into());
        assert_eq!(tabs.table(), Some("c"));
        tabs.show("a".into());
        assert_eq!((tabs.views.len(), tabs.table()), (3, Some("a")));

        // Closing a tab before the shown one keeps showing the same table.
        tabs.active = 2;
        tabs.close(0);
        assert_eq!(tabs.table(), Some("c"));
        tabs.close(1);
        assert_eq!(tabs.table(), Some("b"));

        tabs.show("c".into());
        tabs.views[0].out = Some(Err("x".into()));
        tabs.views[0].staged.deleted.insert(0);
        tabs.views[1].out = Some(Err("x".into()));
        let schema = parse("table a { id int pk }\ntable b { id int pk }").unwrap();
        tabs.refresh(&schema);
        assert_eq!((tabs.views.len(), tabs.table()), (1, Some("b")));
        assert!(tabs.views[0].out.is_some(), "changes waiting: not reloaded");
        tabs.views[0].staged = Staged::default();
        tabs.refresh(&schema);
        assert!(tabs.views[0].out.is_none());

        tabs.close(0);
        assert_eq!(tabs.table(), None);
        tabs.refresh(&schema);
        assert_eq!(tabs.table(), Some("a"));
    }

    #[test]
    fn tabs_are_saved_and_restored() {
        let schema = parse("table a { id int pk }\ntable b { id int pk }\ntable c { id int pk }").unwrap();
        let mut tabs = DataTabs::default();
        tabs.restore(None, &schema);
        assert_eq!(tabs.table(), Some("a"));
        tabs.show("c".into());
        tabs.show("b".into());
        let saved = tabs.saved("app.db");
        assert_eq!((saved.tables.clone(), saved.active), (vec!["a".to_string(), "c".into(), "b".into()], 2));

        // A table dropped since: its tab is gone, the shown one stays shown.
        let fewer = parse("table a { id int pk }\ntable b { id int pk }").unwrap();
        tabs.restore(Some(&saved), &fewer);
        assert_eq!((tabs.views.len(), tabs.table()), (2, Some("b")));
        tabs.restore(Some(&SavedTabs { db: "x".into(), tables: vec!["gone".into()], active: 0 }), &fewer);
        assert_eq!((tabs.views.len(), tabs.table()), (1, Some("a")));

        let mut list = Vec::new();
        for i in 0..=MAX_SAVED_TABS {
            remember(&mut list, SavedTabs { db: i.to_string(), tables: vec![], active: 0 });
        }
        remember(&mut list, SavedTabs { db: "5".into(), tables: vec!["a".into()], active: 0 });
        assert_eq!(list.len(), MAX_SAVED_TABS);
        assert_eq!((list[0].db.as_str(), list[0].tables.len()), ("5", 1));
        assert_eq!(list.iter().filter(|s| s.db == "5").count(), 1);
    }

    #[test]
    fn staged_changes_become_row_changes() {
        let t = parse("table t { id int pk\n name text\n n int default 0 }").unwrap().tables.remove(0);
        let r = QueryResult {
            columns: vec!["id".into(), "name".into(), "n".into()],
            rows: vec![
                vec![Cell::Int(1), Cell::Text("a".into()), Cell::Int(0)],
                vec![Cell::Int(2), Cell::Text("b".into()), Cell::Int(0)],
            ],
            changes: None,
            truncated: false,
        };
        let mut v = DataView { out: Some(Ok(r.clone())), ..DataView::default() };
        v.set(0, 1, Some(Cell::Text("A".into())));
        v.set(1, 1, Some(Cell::Text("B".into())));
        v.set(1, 1, Some(Cell::Text("b".into()))); // back to the original: no change
        v.toggle_delete(1);
        v.staged.inserted.push(vec![Some(Cell::Text("3".into())), None, None]);
        assert_eq!(v.pending(), 3);
        let changes = v.staged.changes(&t, &r).unwrap();
        assert_eq!(
            changes,
            [
                RowChange::Delete { key: vec![("id".into(), Cell::Int(2))] },
                RowChange::Update { key: vec![("id".into(), Cell::Int(1))], values: vec![("name".into(), Cell::Text("A".into()))] },
                RowChange::Insert { values: vec![("id".into(), Cell::Text("3".into()))] },
            ]
        );
        // Committing text that doesn't fit the type keeps the cell open.
        v.editing = Some(Editing { row: 0, col: 2, text: "many".into(), focus: false });
        v.commit(Some(&t), Dialect::Sqlite);
        assert!(v.editing.is_some() && v.error.is_some());
        v.editing.as_mut().unwrap().text = "7".into();
        v.commit(Some(&t), Dialect::Sqlite);
        assert_eq!(v.staged.edits.get(&(0, 2)), Some(&Cell::Text("7".into())));
    }
}
