//! Pending framework migrations: the project folder's migration files that
//! the open database hasn't run, read on the database thread
//! (`framework_history`), and the Migrate tab's view of them. Push stays off
//! for this source: the framework records what it runs in its own history
//! table, so it should be the one to run them.

use crate::{App, DiagramSource, Pending, db_title, empty_state, error_color, ok_color, warn_color, highlight::{self, Lang}};
use eframe::egui::{self, RichText};
use simple_database::db::Database;
use simple_database::format;
use simple_database::framework::Framework;
use simple_database::framework_history::{self, Exploration, FileOutcome};
use simple_database::schema::Schema;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An exploration, the framework of its files and its resulting schema as text, formatted once.
pub(crate) struct Explored {
    pub exploration: Exploration,
    pub framework: Framework,
    pub text: String,
}

/// What `find` looks for, for messages.
pub(crate) const MIGRATION_FOLDERS: &str = "database/migrations (Laravel) or db/migrate (Rails)";

/// The pending migrations of `project` against the database schema
/// `current`; `result` is `None` while they are read.
pub(crate) struct FrameworkFiles {
    project: PathBuf,
    current: Schema,
    result: Option<Result<Arc<Explored>, String>>,
}

impl App {
    /// The folder whose migrations are read: the open project's, else the schema file's.
    pub(crate) fn project_folder(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.project_dir {
            return Some(dir.clone());
        }
        let dir = self.schema_path.as_ref()?.parent()?;
        Some(if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir.to_path_buf() })
    }

    /// Whether the project folder has migration files sdb can read.
    pub(crate) fn has_framework_migrations(&self) -> bool {
        self.project_folder().is_some_and(|d| framework_history::find(&d).is_some())
    }

    /// The pending migrations, read again on the database thread when the
    /// folder or the database's schema changed. `None` while they are read,
    /// or without a database or project folder.
    pub(crate) fn pending_migrations(&mut self) -> Option<Result<Arc<Explored>, String>> {
        let project = self.project_folder()?;
        let db = self.db.as_mut()?;
        if let Some(f) = &db.framework_files
            && f.project == project
            && f.current == db.schema
        {
            return f.result.clone();
        }
        let current = db.schema.clone();
        db.framework_files = Some(FrameworkFiles { project: project.clone(), current: current.clone(), result: None });
        db.worker.run(
            "Reading pending migrations",
            move |conn| (explore(conn, &project, &current).map_err(|e| format!("{e:#}")), project, current),
            |app: &mut App, (result, project, current)| {
                if let Some(db) = &mut app.db
                    && let Some(f) = &mut db.framework_files
                    && f.project == project
                    && f.current == current
                {
                    f.result = Some(result.map(Arc::new));
                    // The diagram was laid out for the database alone.
                    if app.er_source == DiagramSource::PendingMigrations {
                        app.er.reset_layout();
                    }
                }
            },
        );
        None
    }

    /// The pending migrations as last read, without reading them.
    pub(crate) fn explored(&self) -> Option<Arc<Explored>> {
        self.db.as_ref()?.framework_files.as_ref()?.result.as_ref()?.as_ref().ok().cloned()
    }

    /// Forget what was read, so the files and the history table are read again.
    pub(crate) fn reload_pending_migrations(&mut self) {
        if let Some(db) = &mut self.db {
            db.framework_files = None;
        }
    }

    /// The Migrate tab when its source is the pending migrations.
    pub(crate) fn pending_migrations_view(&mut self, ui: &mut egui::Ui) {
        let Some(folder) = self.project_folder().filter(|d| framework_history::find(d).is_some()) else {
            let body = format!("Open the project (File → Open project…); its {MIGRATION_FOLDERS} files are read.");
            empty_state(ui, "No migrations folder", &body);
            return;
        };
        let db_name = self.db.as_ref().map(|d| db_title(&d.label)).unwrap_or_default();
        let mut reload = false;
        ui.horizontal(|ui| {
            ui.heading(format!("Pending migrations  ➡  {db_name}"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = format!("Read the migration files in {} and the database's history table again", folder.display());
                reload = ui.button("⟳ Reload").on_hover_text(hint).clicked();
            });
        });
        ui.separator();
        if reload {
            self.reload_pending_migrations();
        }
        let explored = match self.pending_migrations() {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading the migrations…");
                });
                return;
            }
            Some(Err(e)) => {
                ui.colored_label(error_color(ui), format!("✖ {e}"));
                return;
            }
            Some(Ok(explored)) => explored,
        };
        let mut into_editor = false;
        let selected = &mut self.selected_migration;
        ui.columns(2, |cols| {
            file_list(&mut cols[0], &explored.exploration, selected);
            match selected.as_ref().and_then(|id| explored.exploration.files.iter().find(|f| f.file.id == *id)) {
                Some(f) => file_detail(&mut cols[1], f, explored.framework),
                None => into_editor = resulting_schema(&mut cols[1], &explored.text),
            }
        });
        if into_editor {
            let ctx = ui.ctx().clone();
            self.request(Pending::MigrationsResultIntoEditor, &ctx);
        }
    }
}

fn explore(conn: &mut Database, folder: &Path, current: &Schema) -> anyhow::Result<Explored> {
    let project = framework_history::find(folder).ok_or_else(|| anyhow::anyhow!("{} has no {MIGRATION_FOLDERS} folder", folder.display()))?;
    let files = framework_history::read_files(&project)?;
    let applied = framework_history::applied(conn, current, project.framework)?;
    let exploration = framework_history::explore(current, project.framework, files, &applied, conn.dialect());
    let text = format::format_schema(&exploration.result);
    Ok(Explored { exploration, framework: project.framework, text })
}

/// The files, oldest first, under the resulting schema; `selected` is a file's id, `None` the resulting schema.
fn file_list(ui: &mut egui::Ui, exploration: &Exploration, selected: &mut Option<String>) {
    ui.strong("Files");
    egui::ScrollArea::vertical().id_salt("migration_files").show(ui, |ui| {
        let pending = exploration.files.iter().filter(|f| !f.applied).count();
        if ui.selectable_label(selected.is_none(), format!("Resulting schema ({pending} pending)")).clicked() {
            *selected = None;
        }
        ui.add_space(4.0);
        for f in &exploration.files {
            let (mark, color, state) = if f.applied { ("✔", ui.visuals().weak_text_color(), "Applied") } else { ("•", ok_color(ui), "Pending") };
            let mut text = format!("{mark} {}", f.file.id);
            if !f.warnings.is_empty() {
                text.push_str(&format!("  ⚠ {}", f.warnings.len()));
            }
            let is_selected = selected.as_deref() == Some(f.file.id.as_str());
            if ui.selectable_label(is_selected, RichText::new(text).monospace().color(color)).on_hover_text(state).clicked() {
                *selected = Some(f.file.id.clone());
            }
        }
    });
}

/// One file: what it changes, sdb's SQL for it, and its source.
fn file_detail(ui: &mut egui::Ui, f: &FileOutcome, framework: Framework) {
    let (history, lang) = match framework {
        Framework::Laravel => ("migrations", Lang::Php),
        Framework::Rails => ("schema_migrations", Lang::Ruby),
    };
    ui.strong(&f.file.file_name);
    egui::ScrollArea::both().id_salt("migration_file").auto_shrink([false, false]).show(ui, |ui| {
        if f.applied {
            ui.label(RichText::new(format!("Already applied: the database's {history} table lists it.")).weak());
        } else {
            if f.changes.is_empty() {
                ui.label(RichText::new("No schema changes read from this file.").weak());
            }
            for c in &f.changes {
                let color = match c.chars().next() {
                    Some('+') => ok_color(ui),
                    Some('-') => error_color(ui),
                    _ => warn_color(ui),
                };
                ui.label(RichText::new(c).monospace().color(color));
            }
            for w in &f.warnings {
                ui.colored_label(warn_color(ui), format!("⚠ {w}"));
            }
            if !f.statements.is_empty() {
                ui.add_space(8.0);
                ui.strong("SQL");
                ui.label(RichText::new(format!("sdb's SQL for the same changes; {}'s own may differ in detail.", framework.label())).weak());
                highlight::code_view(ui, &f.statements.join("\n"), Lang::Sql);
            }
        }
        ui.add_space(8.0);
        ui.strong("Source");
        highlight::code_view(ui, &f.file.source, lang);
    });
}

/// The database's schema once the pending files run; true when the user
/// asks to put it in the editor.
fn resulting_schema(ui: &mut egui::Ui, text: &str) -> bool {
    let mut into_editor = false;
    ui.horizontal(|ui| {
        ui.strong("Resulting schema");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            into_editor = ui.button("Open in Schema tab").on_hover_text("Replace the editor with this schema").clicked();
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(text.to_string());
            }
        });
    });
    ui.label(RichText::new("The database once the pending files run. Diagram → With pending migrations draws it, new tables and columns in green.").weak());
    egui::ScrollArea::both().id_salt("migration_result").auto_shrink([false, false]).show(ui, |ui| highlight::code_view(ui, text, Lang::Dbs));
    into_editor
}
