//! The Data tab: browsing and editing rows, import and export.

use crate::{App, ConfirmWrite, Dialog, ImportState, Tab, Write, empty_state, data_view};
use eframe::egui;
use simple_database::ddl::Dialect;
use simple_database::connections::SafeMode;
use simple_database::import::{self, ImportFormat};
use simple_database::output::{self, ExportFormat};

impl App {
    pub(crate) fn tab_data(&mut self, ui: &mut egui::Ui) {
        let Some(db) = &self.db else {
            self.no_database(ui, "Open a SQLite database or connect to a server to browse its data.");
            return;
        };
        if db.counts.is_empty() {
            empty_state(ui, "The database has no tables", "Push a schema from the Migrate tab first.");
            return;
        }
        let counts = db.counts.clone();
        self.data.bar(ui, &counts);
        ui.separator();
        let Some(view) = self.data.current() else {
            empty_state(ui, "No table open", "Pick one with ➕ above or in the sidebar.");
            return;
        };
        if view.out.is_none() && self.data_loading.is_none() {
            self.load_data();
        }
        let Some(db) = &self.db else { return };
        let Some(view) = self.data.views.get_mut(self.data.active) else { return };
        let name = view.table.clone().unwrap_or_default();
        let table = db.schema.table(&name).cloned();
        let read_only = db.safe_mode == SafeMode::ReadOnly;
        let (d, confirm) = (db.dialect, db.safe_mode == SafeMode::Confirm);
        // Each table keeps its own grid scroll position.
        let action = ui.push_id(("data_view", &name), |ui| view.ui(ui, &counts, table.as_ref(), d, read_only)).inner;
        match action {
            Some(data_view::DataAction::Load) => self.load_data(),
            Some(data_view::DataAction::Save(_)) if self.db_running("Saving changes") => {}
            Some(data_view::DataAction::Save(changes)) => {
                if confirm {
                    let statements = match &table {
                        Some(t) => changes.iter().map(|c| (c.sql(t, d).unwrap_or_default(), None)).collect(),
                        None => vec![],
                    };
                    self.dialog = Some(Dialog::ConfirmWrite(Box::new(ConfirmWrite { statements, write: Write::SaveRows(name, changes) })));
                } else {
                    self.save_rows(name, changes);
                }
            }
            Some(data_view::DataAction::Query(sql)) => {
                self.query = sql;
                self.tab = Tab::Query;
            }
            Some(data_view::DataAction::Import) => self.pick_import(),
            Some(data_view::DataAction::Export(f)) => self.export_table(f),
            None => {}
        }
    }

    /// Switch to the Data tab showing `table`, in its own tab.
    pub(crate) fn open_table_data(&mut self, table: String) {
        self.tab = Tab::Data;
        self.data.show(table);
    }

    pub(crate) fn dialect_of_db(&self) -> Dialect {
        self.db.as_ref().map_or(Dialect::Sqlite, |d| d.dialect)
    }

    /// The count and page queries of a Data tab, for its table, filters and page.
    fn data_queries(&self, view: &data_view::DataView) -> Result<(String, String), String> {
        let db = self.db.as_ref().ok_or("no database open")?;
        let t = view.table.as_ref().and_then(|t| db.schema.table(t)).ok_or("no such table")?;
        let offset = view.page * data_view::PAGE_SIZE;
        let count = view.view.count_sql(t, db.dialect).map_err(|e| format!("{e:#}"))?;
        let page = view.view.page_sql(t, data_view::PAGE_SIZE, offset, db.dialect).map_err(|e| format!("{e:#}"))?;
        Ok((count, page))
    }

    /// Load the shown Data tab's page and its row count.
    fn load_data(&mut self) {
        let Some(view) = self.data.current() else { return };
        let queries = match self.data_queries(view) {
            Ok(q) => q,
            Err(e) => {
                if let Some(v) = self.data.current_mut() {
                    v.out = Some(Err(e));
                }
                return;
            }
        };
        let table = view.table.clone().unwrap_or_default();
        self.data_loading = Some(queries.clone());
        let (count, page) = queries.clone();
        self.db_run(
            "Loading rows",
            move |conn| {
                let total = conn.query_value(&count)?.display().parse().unwrap_or(0);
                let page = conn.run(&page)?.pop().ok_or_else(|| anyhow::anyhow!("no result"))?;
                anyhow::Ok((page, total))
            },
            move |app, res| {
                if app.data_loading.as_ref() != Some(&queries) {
                    return; // a newer load is on its way
                }
                app.data_loading = None;
                // The tab was closed, or its filters or page changed meanwhile: it loads again when shown.
                let Some(i) = app.data.views.iter().position(|v| v.table.as_ref() == Some(&table)) else { return };
                let view = &app.data.views[i];
                if view.out.is_some() || app.data_queries(view).as_ref() != Ok(&queries) {
                    return;
                }
                let view = &mut app.data.views[i];
                match res {
                    Ok((page, total)) => {
                        view.total = total;
                        view.out = Some(Ok(page));
                    }
                    Err(e) => view.out = Some(Err(format!("{e:#}"))),
                }
            },
        );
    }

    pub(crate) fn save_rows(&mut self, table: String, changes: Vec<simple_database::data::RowChange>) {
        let Some(t) = self.db.as_ref().and_then(|db| db.schema.table(&table)).cloned() else { return };
        self.db_run(
            "Saving changes",
            move |conn| conn.apply_changes(&t, &changes).map(|n| (changes.len(), n)).map_err(|e| format!("{e:#}")),
            move |app, res| match res {
                Ok((changes, n)) => {
                    app.set_status(format!("Saved {changes} change(s), {n} row(s) affected"), false);
                    if let Some(v) = app.data.view_mut(&table) {
                        v.reset();
                    }
                    app.refresh_db();
                }
                Err(e) => match app.data.view_mut(&table) {
                    Some(v) => v.error = Some(e),
                    None => app.set_status(format!("Saving {table} failed: {e}"), true),
                },
            },
        );
    }

    /// Export every row matching the Data tab's filters.
    fn export_table(&mut self, f: ExportFormat) {
        let Some(db) = &self.db else { return };
        let Some(view) = self.data.current() else { return };
        let Some(t) = view.table.as_ref().and_then(|t| db.schema.table(t)) else { return };
        let Some(path) = rfd::FileDialog::new()
            .add_filter(f.label(), &[f.extension()])
            .set_file_name(format!("{}.{}", t.name, f.extension()))
            .save_file()
        else {
            return;
        };
        let d = db.dialect;
        let name = t.name.clone();
        let sql = match view.view.all_sql(t, d) {
            Ok(sql) => sql,
            Err(e) => return self.set_status(format!("Export failed: {e:#}"), true),
        };
        self.db_run(
            &format!("Exporting {name}"),
            move |conn| {
                let r = conn.run(&sql)?.pop().ok_or_else(|| anyhow::anyhow!("no result"))?;
                std::fs::write(&path, output::export(&r, f, &name, d))?;
                anyhow::Ok((r.rows.len(), path))
            },
            |app, res| match res {
                Ok((n, path)) => app.set_status(format!("Exported {n} row(s) to {}", path.display()), false),
                Err(e) => app.set_status(format!("Export failed: {e:#}"), true),
            },
        );
    }

    /// Choose a file and open the Import dialog for it.
    fn pick_import(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Data files", &["csv", "tsv", "tab", "json", "jsonl", "ndjson"])
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return;
        };
        let format = ImportFormat::from_path(&path).unwrap_or(ImportFormat::Csv);
        // A big file takes a while to read through, so it goes on the worker, where it can be cancelled.
        self.db_run(
            "Reading the file",
            move |conn| {
                let res = import::scan(import::Source::File(path.clone()), format, &|| conn.cancel_requested());
                (res.map_err(|e| format!("{e:#}")), path)
            },
            |app, (res, path)| match res {
                Ok(scan) => {
                    let stem = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                    let tables = app.db.as_ref().map(|d| d.schema.tables.iter().map(|t| t.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
                    let table = tables.iter().find(|t| t.to_lowercase() == stem).cloned().or_else(|| app.data.table().map(str::to_owned)).unwrap_or_default();
                    app.dialog = Some(Dialog::Import(Box::new(ImportState { path, scan, table, error: None, busy: false })));
                }
                Err(e) => app.set_status(e, true),
            },
        );
    }
}
