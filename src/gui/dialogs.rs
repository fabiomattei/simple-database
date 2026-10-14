//! Modal dialogs: confirmations, import, restore, scripts, help and the like.

use crate::{App, Dialog, OpenFrom, QueryOutput, Tab, Write, error_color, ok_color, warn_color, conns, grid, highlight::{self, Lang}, table_edit};
use eframe::egui::{self, RichText};
use simple_database::ddl::Dialect;
use simple_database::connections::SafeMode;
use simple_database::db::{Database, QueryResult};
use simple_database::dump;
use simple_database::import;
use simple_database::STARTER_SCHEMA;
use std::sync::atomic::Ordering;

impl App {
    pub(crate) fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.dialog else { return };
        let close = match dialog {
            Dialog::Unsaved(action) => {
                let action = *action;
                let resp = egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.heading("Unsaved changes");
                    ui.label(format!("Save changes to {} first?", self.file_label().trim_end_matches(" •")));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() && self.save_schema(false) {
                            self.dialog = None;
                            self.perform(action, ctx);
                        }
                        if ui.button("Don't save").clicked() {
                            self.dialog = None;
                            self.perform(action, ctx);
                        }
                        if ui.button("Cancel").clicked() {
                            self.dialog = None;
                        }
                    });
                });
                resp.should_close()
            }
            Dialog::ConfirmPush => {
                let resp = egui::Modal::new(egui::Id::new("push")).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.heading("Apply migration?");
                    let desired = self.desired_schema();
                    let warnings = desired.as_ref().and_then(|s| self.migration_for(s)).map(|m| m.warnings).unwrap_or_default();
                    let problems = desired.as_ref().and_then(|s| self.preflight_for(s));
                    let dialect = self.dialect_of_db();
                    if dialect == Dialect::Mysql {
                        ui.colored_label(
                            warn_color(ui),
                            "⚠ MySQL cannot roll back schema changes: if a statement fails, the ones before it stay applied.",
                        );
                    } else if warnings.is_empty() {
                        ui.label("The changes run inside a transaction and are rolled back if anything fails.");
                    }
                    if !warnings.is_empty() {
                        for w in &warnings {
                            ui.colored_label(warn_color(ui), format!("⚠ {w}"));
                        }
                    }
                    ui.add_space(4.0);
                    match &problems {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Checking the rows already there…");
                            });
                        }
                        Some(p) if p.is_empty() => {}
                        Some(p) => {
                            ui.colored_label(error_color(ui), "The migration would fail on these rows:");
                            for problem in p {
                                ui.colored_label(error_color(ui), format!("✖ {problem}"));
                            }
                        }
                    }
                    ui.add_space(8.0);
                    let label = if problems.as_ref().is_some_and(|p| !p.is_empty()) { "Apply anyway" } else { "Apply" };
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(label).strong()).clicked() {
                            self.dialog = None;
                            self.push_now();
                        }
                        if ui.button("Cancel").clicked() {
                            self.dialog = None;
                        }
                    });
                });
                resp.should_close()
            }
            Dialog::Connections(_) => {
                let Some(Dialog::Connections(mut m)) = self.dialog.take() else { unreachable!() };
                let mut action = None;
                let resp = egui::Modal::new(egui::Id::new("connections")).show(ctx, |ui| {
                    action = m.show(ui, &mut self.conns);
                });
                match action {
                    Some(conns::Action::Connect { target, label, safe_mode }) if self.shows_other_db(&label) => {
                        let launch = crate::window::Launch { target, labeled: Some((label.clone(), safe_mode)), tab: None };
                        self.open_window(Some(launch), &label);
                        return;
                    }
                    Some(conns::Action::Connect { target, label, safe_mode }) => {
                        m.message = Some((format!("Connecting to {label}…"), false));
                        self.start_open(format!("Connecting to {label}"), OpenFrom::Dialog, move || {
                            Database::open_labeled(&target, label).and_then(|mut d| d.set_safe_mode(safe_mode).map(|()| d))
                        });
                    }
                    Some(conns::Action::Close) => return,
                    None => {}
                }
                self.dialog = Some(Dialog::Connections(m));
                resp.should_close()
            }
            Dialog::Table(_) => {
                let Some(Dialog::Table(mut editor)) = self.dialog.take() else { unreachable!() };
                let schema = self.analysis.last_good.clone();
                let mut outcome = None;
                let resp = egui::Modal::new(egui::Id::new("table_edit")).show(ctx, |ui| {
                    outcome = editor.show(ui, &schema);
                });
                match outcome {
                    Some(table_edit::Outcome::Apply { schema: new, table, renamed }) => {
                        if let Some(pos) = editor.place {
                            self.er.place(&table, pos);
                        }
                        self.er.selected = Some(table);
                        self.apply_diagram_edit(&new, renamed);
                        return;
                    }
                    Some(table_edit::Outcome::Cancel) => return,
                    None => {}
                }
                self.dialog = Some(Dialog::Table(editor));
                resp.should_close()
            }
            Dialog::ConfirmWrite(_) => {
                let Some(Dialog::ConfirmWrite(c)) = self.dialog.take() else { unreachable!() };
                let mut answer = None;
                let label = self.db.as_ref().map(|d| d.label.to_string()).unwrap_or_default();
                let resp = egui::Modal::new(egui::Id::new("confirm_write")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading(format!("Change {label}?"));
                    ui.label(RichText::new("Safe mode asks before these statements run.").weak());
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        for (stmt, why) in &c.statements {
                            highlight::code_view(ui, stmt, Lang::Sql);
                            if let Some(why) = why {
                                ui.colored_label(error_color(ui), format!("⚠ {why}"));
                            }
                            ui.add_space(4.0);
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Run").strong()).clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            answer = Some(false);
                        }
                    });
                });
                match answer {
                    Some(true) => match c.write {
                        Write::Query => self.run_query(),
                        Write::SaveRows(table, changes) => self.save_rows(table, changes),
                        Write::Seed => self.seed_now(),
                    },
                    Some(false) => {}
                    None if resp.should_close() => {}
                    None => self.dialog = Some(Dialog::ConfirmWrite(c)),
                }
                return;
            }
            Dialog::Import(_) => {
                let Some(Dialog::Import(mut st)) = self.dialog.take() else { unreachable!() };
                let mut answer = None;
                let (tables, read_only, confirm) = match &self.db {
                    Some(d) => (
                        d.schema.tables.clone(),
                        d.safe_mode == SafeMode::ReadOnly,
                        (d.safe_mode == SafeMode::Confirm),
                    ),
                    None => (vec![], true, false),
                };
                let resp = egui::Modal::new(egui::Id::new("import")).show(ctx, |ui| {
                    ui.set_width(640.0);
                    ui.heading("Import rows");
                    ui.label(RichText::new(st.path.display().to_string()).monospace().small().weak());
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Into table");
                        egui::ComboBox::from_id_salt("import_table").selected_text(&st.table).show_ui(ui, |ui| {
                            for t in &tables {
                                ui.selectable_value(&mut st.table, t.name.clone(), &t.name);
                            }
                        });
                        ui.label(RichText::new(format!("{} row(s) in the file", st.scan.rows)).weak());
                    });
                    let table = tables.iter().find(|t| t.name == st.table);
                    if let Some(t) = table {
                        let plan = import::plan(t, &st.scan);
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Columns:");
                            for (_, c) in &plan.mapping {
                                ui.colored_label(ok_color(ui), format!("✔ {c}"));
                            }
                            for c in &plan.skipped {
                                ui.label(RichText::new(format!("✖ {c}")).weak()).on_hover_text("Not in the table, or empty and filled by the database: skipped");
                            }
                        });
                        if !plan.missing.is_empty() {
                            ui.colored_label(
                                warn_color(ui),
                                format!("⚠ {} need a value, but the file has no such column", plan.missing.join(", ")),
                            );
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("Preview").small().strong());
                    let preview = QueryResult {
                        columns: st.scan.columns.clone(),
                        rows: st.scan.preview.clone(),
                        changes: None,
                        truncated: false,
                    };
                    ui.allocate_ui(egui::vec2(ui.available_width(), 200.0), |ui| {
                        grid::result_grid(ui, egui::Id::new("import_preview"), &preview);
                    });
                    ui.label(RichText::new("All rows go in one transaction: if one fails, nothing is imported.").small().weak());
                    if confirm {
                        ui.colored_label(warn_color(ui), "Safe mode: importing writes to the database.");
                    }
                    if let Some(e) = &st.error {
                        ui.colored_label(error_color(ui), format!("✖ {e}"));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = table.is_some() && !read_only && st.scan.rows > 0 && !st.busy;
                        let button = egui::Button::new(RichText::new(format!("Import {} row(s)", st.scan.rows)).strong());
                        if ui.add_enabled(ok, button).on_disabled_hover_text(if read_only { "The connection is read-only" } else { "Choose a table" }).clicked() {
                            answer = Some(true);
                        }
                        if st.busy {
                            ui.spinner();
                        } else if ui.button("Cancel").clicked() {
                            answer = Some(false);
                        }
                    });
                });
                match answer {
                    Some(true) => {
                        if let Some(t) = tables.iter().find(|t| t.name == st.table).cloned() {
                            st.busy = true;
                            st.error = None;
                            let scan = st.scan.clone();
                            self.db_run(
                                "Importing rows",
                                move |conn| (import::import(conn, &t, &scan).map_err(|e| format!("{e:#}")), t.name),
                                |app, (res, table)| match res {
                                    Ok(n) => {
                                        if matches!(app.dialog, Some(Dialog::Import(_))) {
                                            app.dialog = None;
                                        }
                                        app.set_status(format!("Imported {n} row(s) into {table}"), false);
                                        app.refresh_db();
                                        app.open_table_data(table);
                                    }
                                    Err(e) => match &mut app.dialog {
                                        Some(Dialog::Import(st)) => {
                                            st.busy = false;
                                            st.error = Some(e);
                                        }
                                        _ => app.set_status(format!("Import failed: {e}"), true),
                                    },
                                },
                            );
                        }
                        self.dialog = Some(Dialog::Import(st));
                    }
                    Some(false) => {}
                    None if resp.should_close() && !st.busy => {}
                    None => self.dialog = Some(Dialog::Import(st)),
                }
                return;
            }
            Dialog::Restore(_) => {
                let Some(Dialog::Restore(mut st)) = self.dialog.take() else { unreachable!() };
                let mut answer = None;
                let (label, dialect) = match &self.db {
                    Some(d) => (d.label.to_string(), d.dialect),
                    None => (String::new(), Dialect::Sqlite),
                };
                let resp = egui::Modal::new(egui::Id::new("restore")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading(format!("Restore into {label}?"));
                    ui.label(RichText::new(st.path.display().to_string()).monospace().small().weak());
                    ui.add_space(6.0);
                    let r = &st.restore;
                    ui.label(format!("{} statement(s): {} table(s) to create, {} INSERT(s).", r.statements.len(), r.creates.len(), r.inserts()));
                    if !r.creates.is_empty() {
                        ui.label(RichText::new(r.creates.join(", ")).weak());
                    }
                    if !r.existing.is_empty() {
                        ui.add_space(4.0);
                        ui.colored_label(warn_color(ui), format!("⚠ The database already has {}.", r.existing.join(", ")));
                        ui.checkbox(&mut st.clean, "Drop these tables first (their rows are lost)");
                    }
                    ui.add_space(4.0);
                    if dialect == Dialect::Mysql {
                        ui.colored_label(warn_color(ui), "⚠ MySQL commits each CREATE and DROP: if a statement fails, the ones before it stay applied.");
                    } else {
                        ui.label(RichText::new("Everything runs in one transaction: if a statement fails, nothing is changed.").small().weak());
                    }
                    if let Some(e) = &st.error {
                        ui.colored_label(error_color(ui), format!("✖ {e}"));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = (r.existing.is_empty() || st.clean) && !st.busy;
                        if ui.add_enabled(ok, egui::Button::new(RichText::new("Restore").strong())).on_disabled_hover_text("Tick the box to drop the existing tables").clicked() {
                            answer = Some(true);
                        }
                        if st.busy {
                            ui.spinner();
                        } else if ui.button("Cancel").clicked() {
                            answer = Some(false);
                        }
                    });
                });
                match answer {
                    Some(true) => {
                        st.busy = true;
                        st.error = None;
                        let (restore, clean, path) = (st.restore.clone(), st.clean, st.path.clone());
                        self.db_run(
                            "Restoring the dump",
                            move |conn| (dump::restore(conn, &restore, clean).map_err(|e| format!("{e:#}")), path),
                            |app, (res, path)| {
                                app.refresh_db();
                                match res {
                                    Ok(n) => {
                                        if matches!(app.dialog, Some(Dialog::Restore(_))) {
                                            app.dialog = None;
                                        }
                                        app.set_status(format!("Restored {}: {n} row(s)", path.display()), false);
                                    }
                                    Err(e) => match &mut app.dialog {
                                        Some(Dialog::Restore(st)) => {
                                            st.busy = false;
                                            st.error = Some(e);
                                        }
                                        _ => app.set_status(format!("Restore failed: {e}"), true),
                                    },
                                }
                            },
                        );
                        self.dialog = Some(Dialog::Restore(st));
                    }
                    Some(false) => {}
                    None if resp.should_close() && !st.busy => {}
                    None => self.dialog = Some(Dialog::Restore(st)),
                }
                return;
            }
            Dialog::RunScript(_) => {
                let Some(Dialog::RunScript(mut st)) = self.dialog.take() else { unreachable!() };
                let mut run = false;
                let mut show_results = false;
                let mut close = false;
                let (label, dialect) = match &self.db {
                    Some(d) => (d.label.to_string(), d.dialect),
                    None => (String::new(), Dialect::Sqlite),
                };
                let resp = egui::Modal::new(egui::Id::new("run_script")).show(ctx, |ui| {
                    ui.set_width(560.0);
                    ui.heading(format!("Run SQL file on {label}"));
                    ui.label(RichText::new(st.path.display().to_string()).monospace().small().weak());
                    ui.add_space(6.0);
                    match (&st.outcome, &st.progress) {
                        (None, Some(progress)) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(format!("Running statement {} of {}…", progress.load(Ordering::Relaxed) + 1, st.statements));
                            });
                            ui.add_space(8.0);
                            self.cancel_button(ui);
                        }
                        (None, None) => {
                            ui.label(format!("{} statement(s).", st.statements));
                            ui.checkbox(&mut st.transaction, "Run in one transaction (roll back everything if a statement fails)");
                            if st.transaction && dialect == Dialect::Mysql {
                                ui.colored_label(warn_color(ui), "⚠ MySQL commits CREATE, ALTER and DROP immediately, even inside a transaction.");
                            }
                            ui.add_enabled_ui(!st.transaction, |ui| {
                                ui.checkbox(&mut st.keep_going, "Continue after a failed statement");
                            });
                            if !st.asks.is_empty() {
                                ui.add_space(4.0);
                                ui.colored_label(warn_color(ui), format!("Safe mode: {} statement(s) change the database, for example:", st.asks.len()));
                                for (stmt, why) in st.asks.iter().take(5) {
                                    let first = stmt.lines().next().unwrap_or_default();
                                    match why {
                                        Some(why) => ui.colored_label(error_color(ui), format!("{first}  ({why})")),
                                        None => ui.label(RichText::new(first).monospace()),
                                    };
                                }
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                run = ui.button(RichText::new("▶ Run").strong()).clicked();
                                close = ui.button("Cancel").clicked();
                            });
                        }
                        (Some(out), _) => {
                            let r = &out.report;
                            let summary = format!(
                                "Ran {} of {} statement(s) in {:.2?}: {} row(s) changed.",
                                r.run, r.statements, out.elapsed, r.changed
                            );
                            if out.stopped.is_none() && out.errors.is_empty() {
                                ui.colored_label(ok_color(ui), format!("✔ {summary}"));
                            } else {
                                ui.label(summary);
                            }
                            if let Some(e) = &out.stopped {
                                ui.colored_label(error_color(ui), format!("✖ Stopped at {e}"));
                            } else if !out.errors.is_empty() {
                                ui.colored_label(error_color(ui), format!("✖ {} statement(s) failed:", out.errors.len()));
                                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                                    for (line, e) in &out.errors {
                                        ui.label(RichText::new(format!("line {line}: {e}")).monospace().small());
                                    }
                                });
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if !out.results.is_empty() {
                                    show_results = ui.button(format!("Show {} result(s)", out.results.len())).clicked();
                                }
                                close = ui.button("Close").clicked();
                            });
                        }
                    }
                });
                if run {
                    self.run_script(&mut st);
                }
                let running = st.progress.is_some();
                if show_results && let Some(out) = st.outcome.take() {
                    let selected = out.results.len() - 1;
                    self.query_out = Some(QueryOutput { results: Ok(out.results), elapsed: out.elapsed, selected });
                    self.tab = Tab::Query;
                } else if running || !(close || resp.should_close()) {
                    self.dialog = Some(Dialog::RunScript(st));
                }
                return;
            }
            Dialog::Welcome { last } => {
                let last = last.clone();
                let mut pick: Option<String> = None;
                let mut new_conn = false;
                let mut open_file = false;
                let mut skip = false;
                let resp = egui::Modal::new(egui::Id::new("welcome")).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.heading("Welcome to sdb");
                    ui.label("Which database do you want to work with?");
                    ui.add_space(8.0);
                    if let Some(t) = &last {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Last session:").weak());
                            ui.label(RichText::new(t).strong());
                            if ui.button("Reopen").clicked() {
                                pick = Some(t.clone());
                            }
                        });
                        ui.add_space(6.0);
                    }
                    ui.label(RichText::new("SAVED CONNECTIONS").small().strong().weak());
                    if self.conns.connections.is_empty() {
                        ui.label(RichText::new("No saved connections yet.").weak());
                    }
                    let mut list: Vec<&simple_database::connections::Connection> = self.conns.connections.iter().collect();
                    list.sort_by_key(|c| (!c.favorite, std::cmp::Reverse(c.last_used), c.name.to_lowercase()));
                    egui::ScrollArea::vertical().id_salt("welcome_conns").max_height(260.0).show(ui, |ui| {
                        for c in list {
                            ui.horizontal(|ui| {
                                conns::color_dot(ui, c.color);
                                let star = if c.favorite { "★ " } else { "" };
                                if ui.button(format!("{star}{}", c.name)).on_hover_text(c.summary()).clicked() {
                                    pick = Some(format!("@{}", c.name));
                                }
                                ui.label(RichText::new(c.kind.label()).small().weak());
                            });
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        new_conn = ui.button(RichText::new("➕ New connection…").strong()).clicked();
                        open_file = ui.button("Open SQLite file…").clicked();
                        skip = ui.button("Continue without a database").clicked();
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("Help → Example schema shows a sample schema to start from.").small().weak());
                });
                if let Some(t) = pick {
                    self.dialog = None;
                    self.open_db(&t);
                    return;
                }
                if new_conn {
                    self.connections_dialog(true);
                    return;
                }
                if open_file {
                    self.dialog = None;
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                        .add_filter("All files", &["*"])
                        .pick_file()
                    {
                        self.open_db(&p.to_string_lossy());
                    }
                    return;
                }
                skip || resp.should_close()
            }
            Dialog::Integrity(st) => {
                let report = st.report.clone();
                let mut show = None;
                let mut again = false;
                let resp = egui::Modal::new(egui::Id::new("integrity")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading("Foreign key check");
                    ui.label(RichText::new("Rows whose foreign key points at a row that doesn't exist.").weak());
                    ui.add_space(6.0);
                    match &report {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Checking…");
                            });
                        }
                        Some(Err(e)) => {
                            ui.colored_label(error_color(ui), e);
                        }
                        Some(Ok(r)) if r.is_clean() => {
                            ui.colored_label(ok_color(ui), format!("✔ {} foreign key(s) checked: no orphaned rows.", r.checked));
                        }
                        Some(Ok(r)) => {
                            ui.label(format!("{} foreign key(s) checked, {} with problems:", r.checked, r.problems.len()));
                            ui.add_space(4.0);
                            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                                egui::Grid::new("orphans").num_columns(3).spacing([12.0, 6.0]).striped(true).show(ui, |ui| {
                                    for o in &r.problems {
                                        ui.label(RichText::new(o.describe()).monospace());
                                        match &o.error {
                                            Some(e) => {
                                                ui.colored_label(warn_color(ui), "check failed").on_hover_text(e);
                                                ui.label("");
                                            }
                                            None => {
                                                ui.colored_label(error_color(ui), format!("{} row(s)", o.count));
                                                if ui.button("Show rows").on_hover_text("Open them in the Data tab").clicked() {
                                                    show = Some((o.table.clone(), o.condition.clone()));
                                                }
                                            }
                                        }
                                        ui.end_row();
                                    }
                                });
                            });
                        }
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(report.is_some(), egui::Button::new("Check again")).clicked() {
                            again = true;
                        }
                        if ui.button("Close").clicked() {
                            self.dialog = None;
                        }
                    });
                });
                if let Some((table, condition)) = show {
                    self.dialog = None;
                    self.open_table_where(table, condition);
                    return;
                }
                if again {
                    self.check_integrity();
                    return;
                }
                resp.should_close()
            }
            Dialog::Example => {
                let resp = egui::Modal::new(egui::Id::new("example")).show(ctx, |ui| {
                    ui.set_width(620.0);
                    ui.heading("Example schema");
                    ui.label("A small blog (users, posts, tags) that uses most of the syntax.");
                    ui.add_space(6.0);
                    egui::ScrollArea::both().max_height(460.0).show(ui, |ui| {
                        highlight::code_view(ui, STARTER_SCHEMA, Lang::Dbs);
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Open in editor").strong()).on_hover_text("Replace the editor with this schema").clicked() {
                            self.dialog = None;
                            self.request(crate::Pending::ExampleSchema, ctx);
                        }
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(STARTER_SCHEMA.to_string());
                        }
                        if ui.button("Close").clicked() {
                            self.dialog = None;
                        }
                    });
                });
                if self.dialog.is_none() {
                    return;
                }
                resp.should_close()
            }
            Dialog::Syntax => {
                let resp = egui::Modal::new(egui::Id::new("syntax")).show(ctx, |ui| {
                    ui.set_width(620.0);
                    ui.heading("Schema syntax");
                    let help: String = STARTER_SCHEMA
                        .lines()
                        .take_while(|l| l.starts_with('#'))
                        .map(|l| l.trim_start_matches('#').strip_prefix(' ').unwrap_or(l.trim_start_matches('#')))
                        .collect::<Vec<_>>()
                        .join("\n");
                    ui.label(RichText::new(help).monospace());
                    ui.add_space(6.0);
                    ui.label("Example:");
                    highlight::code_view(
                        ui,
                        "table posts {\n  id         int   pk autoincrement\n  author_id  int   not null -> users.id on delete cascade\n  title      text  not null default ''\n  index (author_id)\n}",
                        Lang::Dbs,
                    );
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        self.dialog = None;
                    }
                });
                resp.should_close()
            }
        };
        if close {
            self.dialog = None;
        }
    }
}
