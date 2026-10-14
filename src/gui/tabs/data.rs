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
        if self.data.table.is_none() {
            self.data.open(db.counts.first().map(|(t, _)| t.clone()));
        }
        if self.data.out.is_none() && self.data_loading.is_none() {
            self.load_data();
        }
        let Some(db) = &self.db else { return };
        let table = self.data.table.as_ref().and_then(|t| db.schema.table(t)).cloned();
        let read_only = db.safe_mode == SafeMode::ReadOnly;
        let action = self.data.ui(ui, &db.counts.clone(), table.as_ref(), db.dialect, read_only);
        match action {
            Some(data_view::DataAction::Load) => self.load_data(),
            Some(data_view::DataAction::Save(_)) if self.db_running("Saving changes") => {}
            Some(data_view::DataAction::Save(changes)) => {
                if self.db.as_ref().is_some_and(|d| d.safe_mode == SafeMode::Confirm) {
                    let statements = match &table {
                        Some(t) => changes.iter().map(|c| (c.sql(t, self.dialect_of_db()).unwrap_or_default(), None)).collect(),
                        None => vec![],
                    };
                    self.dialog = Some(Dialog::ConfirmWrite(Box::new(ConfirmWrite { statements, write: Write::SaveRows(changes) })));
                } else {
                    self.save_rows(changes);
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

    pub(crate) fn dialect_of_db(&self) -> Dialect {
        self.db.as_ref().map_or(Dialect::Sqlite, |d| d.dialect)
    }

    /// The Data tab's count and page queries, for its table, filters and page.
    fn data_queries(&self) -> Result<(String, String), String> {
        let db = self.db.as_ref().ok_or("no database open")?;
        let t = self.data.table.as_ref().and_then(|t| db.schema.table(t)).ok_or("no such table")?;
        let view = &self.data.view;
        let offset = self.data.page * data_view::PAGE_SIZE;
        let count = view.count_sql(t, db.dialect).map_err(|e| format!("{e:#}"))?;
        let page = view.page_sql(t, data_view::PAGE_SIZE, offset, db.dialect).map_err(|e| format!("{e:#}"))?;
        Ok((count, page))
    }

    /// Load the Data tab's page and its row count.
    fn load_data(&mut self) {
        let queries = match self.data_queries() {
            Ok(q) => q,
            Err(e) => {
                self.data.out = Some(Err(e));
                return;
            }
        };
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
                // The table, filters or page changed meanwhile: the next frame loads again.
                if app.data.out.is_some() || app.data_queries().as_ref() != Ok(&queries) {
                    return;
                }
                match res {
                    Ok((page, total)) => {
                        app.data.total = total;
                        app.data.out = Some(Ok(page));
                    }
                    Err(e) => app.data.out = Some(Err(format!("{e:#}"))),
                }
            },
        );
    }

    pub(crate) fn save_rows(&mut self, changes: Vec<simple_database::data::RowChange>) {
        let Some(t) = self.db.as_ref().and_then(|db| self.data.table.as_ref().and_then(|t| db.schema.table(t))).cloned() else { return };
        self.db_run(
            "Saving changes",
            move |conn| conn.apply_changes(&t, &changes).map(|n| (changes.len(), n)).map_err(|e| format!("{e:#}")),
            |app, res| match res {
                Ok((changes, n)) => {
                    app.set_status(format!("Saved {changes} change(s), {n} row(s) affected"), false);
                    app.data.reset();
                    app.refresh_db();
                }
                Err(e) => app.data.error = Some(e),
            },
        );
    }

    /// Export every row matching the Data tab's filters.
    fn export_table(&mut self, f: ExportFormat) {
        let Some(db) = &self.db else { return };
        let Some(t) = self.data.table.as_ref().and_then(|t| db.schema.table(t)) else { return };
        let Some(path) = rfd::FileDialog::new()
            .add_filter(f.label(), &[f.extension()])
            .set_file_name(format!("{}.{}", t.name, f.extension()))
            .save_file()
        else {
            return;
        };
        let d = db.dialect;
        let name = t.name.clone();
        let sql = match self.data.view.all_sql(t, d) {
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
        match import::read_file(&path, Some(format)) {
            Ok(rows) => {
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                let tables = self.db.as_ref().map(|d| d.schema.tables.iter().map(|t| t.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
                let table = tables.iter().find(|t| t.to_lowercase() == stem).cloned().or_else(|| self.data.table.clone()).unwrap_or_default();
                self.dialog = Some(Dialog::Import(Box::new(ImportState { path, rows, table, error: None, busy: false })));
            }
            Err(e) => self.set_status(format!("{e:#}"), true),
        }
    }
}
