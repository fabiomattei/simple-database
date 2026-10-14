//! The Migrate tab: the diff from the open database to the schema, and Push.

use crate::{App, Dialog, MigrateSource, PUSHING, Pending, empty_state, error_color, ok_color, warn_color, conns, highlight::{self, Lang}, worker::Task};
use eframe::egui::{self, RichText};
use simple_database::schema::Schema;
use simple_database::connections::SafeMode;
use simple_database::db::{self, Database};
use simple_database::diff::Migration;
use simple_database::framework::{self, Framework, Generated};
use simple_database::validate::Severity;

impl App {
    pub(crate) fn tab_migrate(&mut self, ui: &mut egui::Ui) {
        if self.db.is_none() {
            self.no_database(ui, "Open or create a SQLite database, or connect to a server (File menu or sidebar), to push this schema into it.");
            return;
        }
        // Pick what to compare with: the schema file or another database.
        let mut pick: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.label("From");
            let current = match &self.migrate_source {
                MigrateSource::SchemaFile => format!("📄 {}", self.file_label()),
                MigrateSource::Database { label, .. } => format!("🗄 {label}"),
            };
            ui.menu_button(format!("{current} ⏷"), |ui| {
                if ui.button(format!("📄 Schema file ({})", self.file_label())).clicked() {
                    pick = Some(None);
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new("Another database").small().weak());
                let mut saved: Vec<_> = self.conns.connections.iter().collect();
                saved.sort_by_key(|c| c.name.to_lowercase());
                for c in saved {
                    let resp = ui.horizontal(|ui| {
                        conns::color_dot(ui, c.color);
                        ui.button(&c.name).on_hover_text(c.summary())
                    });
                    if resp.inner.clicked() {
                        pick = Some(Some(format!("@{}", c.name)));
                        ui.close();
                    }
                }
                if ui.button("SQLite file…").clicked() {
                    ui.close();
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                        .add_filter("All files", &["*"])
                        .pick_file()
                    {
                        pick = Some(Some(p.to_string_lossy().into_owned()));
                    }
                }
            });
            if let MigrateSource::Database { .. } = self.migrate_source
                && ui.button("⟳").on_hover_text("Read the source database again").clicked()
                && let MigrateSource::Database { label, .. } = &self.migrate_source
            {
                pick = Some(Some(label.clone()));
            }
        });
        if let Some(choice) = pick {
            match choice {
                None => self.migrate_source = MigrateSource::SchemaFile,
                Some(target) => {
                    let what = format!("Cannot read {}", db::redact(&target));
                    self.migrate_reading = Some(Task::spawn(what, ui.ctx(), move || {
                        Database::open(&target).and_then(|mut d| Ok((d.introspect()?, d.label().to_string()))).map_err(|e| format!("{e:#}"))
                    }));
                }
            }
        }
        if self.migrate_reading.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading the source database…");
            });
            return;
        }

        let from_file = matches!(self.migrate_source, MigrateSource::SchemaFile);
        let (schema, source_label) = match &self.migrate_source {
            MigrateSource::Database { label, schema } => (schema.clone(), label.clone()),
            MigrateSource::SchemaFile => match &self.analysis.result {
                Err(e) => {
                    empty_state(ui, "Schema has a syntax error", &e.to_string());
                    return;
                }
                Ok(s) => (s.clone(), self.file_label()),
            },
        };
        let errors = if from_file { self.analysis.issues.iter().filter(|i| i.severity == Severity::Error).count() } else { 0 };
        let Some(m) = self.migration_for(&schema) else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Comparing schemas…");
            });
            return;
        };
        let Some(db) = &self.db else { return };
        let db_name = db.label.to_string();
        let read_only = db.safe_mode == SafeMode::ReadOnly;
        let pushing = db.worker.is_running(PUSHING);

        ui.horizontal(|ui| {
            ui.heading(format!("{source_label}  ➡  {db_name}"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Pull database into editor").on_hover_text("Replace the editor contents with the open database's schema").clicked() {
                    let ctx = ui.ctx().clone();
                    self.request(Pending::PullIntoEditor, &ctx);
                }
                let can_push = !m.is_empty() && errors == 0 && !read_only && !pushing;
                let why = if pushing {
                    "Pushing…"
                } else if read_only {
                    "The connection is read-only"
                } else if errors > 0 {
                    "Fix schema errors first"
                } else {
                    "Nothing to push"
                };
                if ui
                    .add_enabled(can_push, egui::Button::new(RichText::new("⬆ Push to database").strong()))
                    .on_disabled_hover_text(why)
                    .clicked()
                {
                    self.dialog = Some(Dialog::ConfirmPush);
                }
            });
        });
        ui.separator();

        if errors > 0 {
            ui.colored_label(error_color(ui), format!("✖ The schema has {errors} error(s); see the Schema tab."));
            return;
        }
        if m.is_empty() {
            let body = if from_file { "The database already matches the schema file." } else { "Both databases have the same schema." };
            empty_state(ui, "✔ Database is up to date", body);
            return;
        }
        // The right column shows the SQL Push runs, or the same as a framework's migration.
        let mut format = self.migrate_format;
        let generated = format.and_then(|fw| self.framework_migration_for(&schema, fw));
        let migrations_dir = |fw: Framework| {
            let dir = self.schema_path.as_ref()?.parent()?.join(fw.migrations_dir());
            dir.is_dir().then_some(dir)
        };
        let save_dir = format.and_then(migrations_dir);
        let mut saved: Option<Result<std::path::PathBuf, String>> = None;
        ui.columns(2, |cols| {
            cols[0].strong("Changes");
            egui::ScrollArea::vertical().id_salt("changes").show(&mut cols[0], |ui| {
                for c in &m.changes {
                    let color = match c.chars().next() {
                        Some('+') => ok_color(ui),
                        Some('-') => error_color(ui),
                        _ => warn_color(ui),
                    };
                    ui.label(RichText::new(c).monospace().color(color));
                }
                // A framework's migration adds what it couldn't write its own way.
                let extra = generated.iter().flat_map(|g| g.warnings.iter()).filter(|w| !m.warnings.contains(w));
                let warnings: Vec<&String> = m.warnings.iter().chain(extra).collect();
                if !warnings.is_empty() {
                    ui.add_space(8.0);
                    ui.strong("Warnings");
                    for w in warnings {
                        ui.colored_label(warn_color(ui), format!("⚠ {w}"));
                    }
                }
            });
            cols[1].horizontal(|ui| {
                ui.strong("Show as");
                ui.selectable_value(&mut format, None, "SQL").on_hover_text("The SQL that Push runs");
                for fw in Framework::ALL {
                    ui.selectable_value(&mut format, Some(fw), fw.label())
                        .on_hover_text(format!("The same changes as a {} migration, with the way back", fw.label()));
                }
                if let (Some(fw), Some(g)) = (format, &generated) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Save as…").on_hover_text(&g.file_name).clicked() {
                            let ext = if fw == Framework::Laravel { "php" } else { "rb" };
                            let mut dialog = rfd::FileDialog::new().add_filter(format!("{} migration", fw.label()), &[ext]).set_file_name(&g.file_name);
                            if let Some(dir) = &save_dir {
                                dialog = dialog.set_directory(dir);
                            }
                            if let Some(path) = dialog.save_file() {
                                saved = Some(std::fs::write(&path, &g.code).map(|()| path).map_err(|e| e.to_string()));
                            }
                        }
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(g.code.clone());
                        }
                    });
                }
            });
            egui::ScrollArea::both().id_salt("migration_sql").auto_shrink([false, false]).show(&mut cols[1], |ui| match (format, &generated) {
                (None, _) => highlight::code_view(ui, &m.script(), Lang::Sql),
                (Some(fw), Some(g)) => highlight::code_view(ui, &g.code, if fw == Framework::Laravel { Lang::Php } else { Lang::Ruby }),
                (Some(_), None) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Writing the migration…");
                    });
                }
            });
        });
        self.migrate_format = format;
        match saved {
            Some(Ok(path)) => self.set_status(format!("Migration saved to {}", path.display()), false),
            Some(Err(e)) => self.set_status(format!("Cannot save the migration: {e}"), true),
            None => {}
        }
    }

    /// The migration from the open database to `desired` as a framework's
    /// migration, computed on the database thread like `migration_for`.
    fn framework_migration_for(&mut self, desired: &Schema, fw: Framework) -> Option<Generated> {
        let db = self.db.as_mut()?;
        match &db.framework_migration {
            Some((s, f, g)) if s == desired && *f == fw => return g.clone(),
            _ => {}
        }
        db.framework_migration = Some((desired.clone(), fw, None));
        let (current, target) = (db.schema.clone(), desired.clone());
        db.worker.run(
            "Writing the migration",
            move |conn| (conn.framework_migration(&current, &target, fw, &framework::Options::default()), current, target),
            move |app: &mut App, (g, current, target)| {
                if let Some(db) = &mut app.db
                    && db.schema == current
                    && let Some((desired, f, slot @ None)) = &mut db.framework_migration
                    && *desired == target
                    && *f == fw
                {
                    *slot = Some(g);
                }
            },
        );
        None
    }

    /// The schema the open database should get: the schema file's, or the
    /// source database's when comparing two databases.
    pub(crate) fn desired_schema(&self) -> Option<Schema> {
        match &self.migrate_source {
            MigrateSource::Database { schema, .. } => Some(schema.clone()),
            MigrateSource::SchemaFile => self.analysis.result.as_ref().ok().cloned(),
        }
    }

    /// The migration from the open database to `desired`, computed on the
    /// database thread (normalizing checks asks the server); `None` until ready.
    pub(crate) fn migration_for(&mut self, desired: &Schema) -> Option<Migration> {
        let db = self.db.as_mut()?;
        match &db.migration {
            Some((s, m)) if s == desired => return m.clone(),
            _ => {}
        }
        db.migration = Some((desired.clone(), None));
        let (current, target) = (db.schema.clone(), desired.clone());
        db.worker.run(
            "Comparing schemas",
            move |conn| (conn.migration(&current, &target), current, target),
            |app: &mut App, (m, current, target)| {
                // Only if neither side changed while it ran.
                if let Some(db) = &mut app.db
                    && db.schema == current
                    && let Some((desired, slot @ None)) = &mut db.migration
                    && *desired == target
                {
                    *slot = Some(m);
                }
            },
        );
        None
    }

    pub(crate) fn push_now(&mut self) {
        let Some(schema) = self.desired_schema() else { return };
        let Some(current) = self.db.as_ref().map(|d| d.schema.clone()) else { return };
        self.db_run(
            PUSHING,
            move |conn| {
                let m = conn.migration(&current, &schema);
                conn.apply_migration(&m).map(|()| m.changes.len()).map_err(|e| format!("{e:#}"))
            },
            |app, res| {
                app.refresh_db();
                match res {
                    Ok(n) => app.set_status(format!("Applied {n} change(s)"), false),
                    Err(e) => app.set_status(e, true),
                }
            },
        );
    }
}
