//! The Query tab: SQL editor, results, history and saved queries.

use crate::{App, ConfirmWrite, Dialog, MAX_QUERY_ROWS, QueryOutput, RUNNING_QUERY, Write, error_color, from_table, warn_color, editor, grid, highlight::{self, Lang}};
use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText};
use simple_database::ddl::Dialect;
use simple_database::history;
use simple_database::output::{self, ExportFormat};
use std::time::Instant;

impl App {
    pub(crate) fn tab_query(&mut self, ui: &mut egui::Ui) {
        if self.db.is_none() {
            self.no_database(ui, "Open a SQLite database or connect to a server to run queries.");
            return;
        }
        let run_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
        let running = self.db_running(RUNNING_QUERY);
        let mut run = ui.input_mut(|i| i.consume_shortcut(&run_shortcut));
        let mut explain = false;
        ui.horizontal(|ui| {
            let button = egui::Button::new(RichText::new("▶ Run").strong());
            if ui.add_enabled(!running, button).on_hover_text(ui.ctx().format_shortcut(&run_shortcut)).clicked() {
                run = true;
            }
            let explain_hint = match self.db.as_ref().map(|d| d.dialect) {
                Some(Dialect::Sqlite) => "Show the query plan (EXPLAIN QUERY PLAN) without running the query",
                _ => "Run with EXPLAIN ANALYZE in a transaction that is rolled back: real timings, no changes kept",
            };
            explain = ui.add_enabled(!running, egui::Button::new("Explain")).on_hover_text(explain_hint).clicked();
            if running {
                ui.spinner();
                self.cancel_button(ui);
            }
            ui.separator();
            if ui.button("📂 Open…").on_hover_text("Load a .sql file into the editor").clicked()
                && let Some((path, sql)) = Self::pick_sql_file()
            {
                self.query = sql;
                self.set_status(format!("Loaded {}", path.display()), false);
            }
            ui.toggle_value(&mut self.show_history, "🕘 History").on_hover_text("Past queries and saved queries");
            if ui.button("💾 Save query…").clicked() {
                self.save_name = Some(String::new());
            }
            ui.label(RichText::new(format!("{} runs all statements", ui.ctx().format_shortcut(&run_shortcut))).weak());
        });
        if let Some(name) = &mut self.save_name {
            let mut save = false;
            let mut close = false;
            ui.horizontal(|ui| {
                ui.label("Save as");
                let edit = ui.add(egui::TextEdit::singleline(name).hint_text("query name").desired_width(220.0));
                if !edit.has_focus() && name.is_empty() {
                    edit.request_focus();
                }
                save = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                save |= ui.add_enabled(!name.trim().is_empty(), egui::Button::new("Save")).clicked();
                close = ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape));
                if self.log.get(name.trim()).is_some() {
                    ui.colored_label(warn_color(ui), "replaces the saved query with this name");
                }
            });
            if save && !name.trim().is_empty() {
                let name = name.trim().to_string();
                match self.log.save_query(&name, &self.query) {
                    Ok(()) => {
                        self.set_status(format!("Saved query \"{name}\""), false);
                        self.show_history = true;
                        self.show_saved = true;
                        self.save_name = None;
                    }
                    Err(e) => self.set_status(format!("{e:#}"), true),
                }
            } else if close {
                self.save_name = None;
            }
        }
        if self.show_history {
            egui::Panel::left("query_history").resizable(true).default_size(250.0).min_size(160.0).show(ui, |ui| {
                if let Some((sql, now)) = self.history_panel(ui) {
                    self.query = sql;
                    run |= now;
                }
            });
        }
        egui::Panel::top("query_editor")
            .resizable(true)
            .default_size(170.0)
            .min_size(80.0)
            .show(ui, |ui| {
                let vocab = self.db.as_ref().map(|d| editor::Vocabulary::from_schema(&d.schema)).unwrap_or_default();
                egui::ScrollArea::both().id_salt("query_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    editor::code_editor(ui, &mut self.query, Lang::Sql, "query_editor", 8, &vocab, &[]);
                });
            });

        if running {
            // One query at a time; its result replaces the shown one.
        } else if run {
            self.request_run();
        } else if explain {
            self.explain_query();
        }

        let d = self.dialect_of_db();
        let source = from_table(&self.query).unwrap_or_else(|| "result".into());
        let mut status = None;
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(out) = &mut self.query_out else {
                ui.label(RichText::new(if running { "Running…" } else { "Results appear here." }).weak());
                return;
            };
            match &out.results {
                Err(e) => {
                    ui.colored_label(error_color(ui), format!("✖ {e}"));
                }
                Ok(results) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{:.2} ms", out.elapsed.as_secs_f64() * 1000.0)).weak());
                        if results.len() > 1 {
                            ui.separator();
                            for (i, r) in results.iter().enumerate() {
                                let label = match r.changes {
                                    Some(n) => format!("#{}: {n} changed", i + 1),
                                    None => format!("#{}: {} rows", i + 1, r.rows.len()),
                                };
                                ui.selectable_value(&mut out.selected, i, label);
                            }
                        }
                        if let Some(r) = results.get(out.selected).filter(|r| r.changes.is_none()) {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.menu_button("Export ⏷", |ui| {
                                    for f in ExportFormat::ALL {
                                        if ui.button(format!("Save as {}…", f.label())).clicked() {
                                            ui.close();
                                            if let Some(p) = rfd::FileDialog::new()
                                                .add_filter(f.label(), &[f.extension()])
                                                .set_file_name(format!("{source}.{}", f.extension()))
                                                .save_file()
                                            {
                                                status = Some(match std::fs::write(&p, output::export(r, f, &source, d)) {
                                                    Ok(()) => (format!("Exported {} row(s) to {}", r.rows.len(), p.display()), false),
                                                    Err(e) => (format!("Cannot write {}: {e}", p.display()), true),
                                                });
                                            }
                                        }
                                    }
                                    ui.separator();
                                    for f in [ExportFormat::Csv, ExportFormat::Tsv, ExportFormat::Markdown, ExportFormat::Json, ExportFormat::Sql] {
                                        if ui.button(format!("Copy as {}", f.label())).clicked() {
                                            ui.ctx().copy_text(output::export(r, f, &source, d));
                                            status = Some((format!("Copied {} row(s) as {}", r.rows.len(), f.label()), false));
                                            ui.close();
                                        }
                                    }
                                });
                            });
                        }
                    });
                    if let Some(r) = results.get(out.selected) {
                        if r.truncated {
                            ui.colored_label(warn_color(ui), format!("⚠ showing the first {MAX_QUERY_ROWS} rows"));
                        } else if r.changes.is_none() {
                            ui.label(RichText::new(format!("{} rows", r.rows.len())).weak());
                        }
                        grid::result_grid(ui, egui::Id::new(("query", out.selected)), r);
                    }
                }
            }
        });
        if let Some((msg, err)) = status {
            self.set_status(msg, err);
        }
    }

    /// History or saved queries. Returns SQL to load, and whether to run it.
    fn history_panel(&mut self, ui: &mut egui::Ui) -> Option<(String, bool)> {
        let mut picked = None;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.show_saved, false, "History");
            ui.selectable_value(&mut self.show_saved, true, format!("Saved ({})", self.log.saved.len()));
        });
        ui.add(egui::TextEdit::singleline(&mut self.history_search).hint_text("🔍 Search").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let query = self.history_search.trim().to_lowercase();
        let mut remove = None;
        let mut clear = false;
        egui::ScrollArea::vertical().id_salt("history_scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if self.show_saved {
                if self.log.saved.is_empty() {
                    ui.label(RichText::new("No saved queries. Use 💾 Save query… to keep the editor's SQL here.").weak());
                }
                for q in self.log.saved.iter().filter(|q| query.is_empty() || q.name.to_lowercase().contains(&query) || q.sql.to_lowercase().contains(&query)) {
                    let resp = ui.selectable_label(false, RichText::new(&q.name).strong()).on_hover_ui(|ui| {
                        highlight::code_view(ui, &q.sql, Lang::Sql);
                    });
                    if resp.double_clicked() {
                        picked = Some((q.sql.clone(), true));
                    } else if resp.clicked() {
                        picked = Some((q.sql.clone(), false));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Delete").clicked() {
                            remove = Some(q.name.clone());
                            ui.close();
                        }
                    });
                }
            } else {
                let now = history::now();
                let entries = self.log.search(&query);
                if entries.is_empty() {
                    ui.label(RichText::new("Queries you run are listed here.").weak());
                }
                for e in entries.into_iter().take(300) {
                    let first: String = e.sql.trim().lines().next().unwrap_or_default().chars().take(80).collect();
                    let resp = ui
                        .vertical(|ui| {
                            let r = ui.add(egui::Label::new(RichText::new(first).monospace()).truncate().sense(egui::Sense::click()));
                            ui.horizontal(|ui| {
                                if e.error.is_some() {
                                    ui.colored_label(error_color(ui), RichText::new("✖").small());
                                }
                                ui.label(RichText::new(format!("{} · {}", e.database, history::ago(e.at, now))).small().weak());
                            });
                            r
                        })
                        .inner
                        .on_hover_ui(|ui| {
                            highlight::code_view(ui, e.sql.trim(), Lang::Sql);
                            if let Some(err) = &e.error {
                                ui.colored_label(error_color(ui), err);
                            }
                        });
                    if resp.double_clicked() {
                        picked = Some((e.sql.trim().to_string(), true));
                    } else if resp.clicked() {
                        picked = Some((e.sql.trim().to_string(), false));
                    }
                    ui.separator();
                }
                if !self.log.history.is_empty() && ui.small_button("Clear history").clicked() {
                    clear = true;
                }
            }
        });
        if let Some(name) = remove
            && let Err(e) = self.log.remove_query(&name)
        {
            self.set_status(format!("{e:#}"), true);
        }
        if clear && let Err(e) = self.log.clear_history() {
            self.set_status(format!("{e:#}"), true);
        }
        ui.label(RichText::new("Click to load · double-click to run").small().weak());
        picked
    }

    /// Run the editor's SQL, asking first if the connection's safe mode wants it.
    fn request_run(&mut self) {
        let Some(db) = &self.db else { return };
        let asks = db.safe_mode.confirmations(&self.query, &db.writing_functions);
        if asks.is_empty() {
            self.run_query();
        } else {
            let statements = asks
                .into_iter()
                .map(|(s, k)| (s, if let simple_database::safety::Kind::Dangerous(why) = k { Some(why) } else { None }))
                .collect();
            self.dialog = Some(Dialog::ConfirmWrite(Box::new(ConfirmWrite { statements, write: Write::Query })));
        }
    }

    fn explain_query(&mut self) {
        let sql = self.query.clone();
        self.db_run(
            RUNNING_QUERY,
            move |conn| {
                let start = Instant::now();
                let results = conn.explain(&sql).map_err(|e| format!("{e:#}"));
                (results, start.elapsed())
            },
            |app, (results, elapsed)| app.query_out = Some(QueryOutput { results, elapsed, selected: 0 }),
        );
    }

    pub(crate) fn run_query(&mut self) {
        let sql = self.query.clone();
        self.db_run(
            RUNNING_QUERY,
            move |conn| {
                let start = Instant::now();
                let results = conn.run_limited(&sql, MAX_QUERY_ROWS).map_err(|e| format!("{e:#}"));
                let elapsed = start.elapsed();
                let entry = history::Entry {
                    sql: sql.trim().to_string(),
                    database: conn.label().to_string(),
                    at: history::now(),
                    elapsed_ms: elapsed.as_secs_f64() * 1000.0,
                    error: results.as_ref().err().cloned(),
                };
                (results, elapsed, entry, conn.writing_functions().clone())
            },
            |app, (results, elapsed, entry, writing_functions)| {
                if let Some(db) = &mut app.db {
                    db.writing_functions = writing_functions;
                }
                let wrote = results.as_ref().is_ok_and(|rs| rs.iter().any(|r| r.changes.is_some()));
                // Show the last statement that returned rows, else the last one.
                let selected = results.as_ref().map_or(0, |rs| {
                    rs.iter().rposition(|r| r.changes.is_none()).unwrap_or(rs.len().saturating_sub(1))
                });
                app.query_out = Some(QueryOutput { results, elapsed, selected });
                if let Err(e) = app.log.record(entry) {
                    app.set_status(format!("Cannot save query history: {e:#}"), true);
                }
                if wrote {
                    app.refresh_db();
                }
            },
        );
    }
}
