//! The Seed tab: fake data for the open database.

use crate::{App, ConfirmWrite, Dialog, SEEDING, Write, empty_state, error_color, ok_color, warn_color};
use eframe::egui::{self, RichText};
use simple_database::connections::SafeMode;
use simple_database::seed::{self, SeedOptions, TableReport};
use std::collections::HashMap;

impl App {
    pub(crate) fn tab_seed(&mut self, ui: &mut egui::Ui) {
        let Some(db) = &self.db else {
            self.no_database(ui, "Open a SQLite database or connect to a server to fill it with fake data.");
            return;
        };
        if db.schema.tables.is_empty() {
            empty_state(ui, "The database has no tables", "Push a schema from the Migrate tab first.");
            return;
        }
        let order: Vec<String> = db.schema.tables_in_dependency_order().iter().map(|t| t.name.clone()).collect();
        let counts: HashMap<String, i64> = db.counts.iter().cloned().collect();
        // Tables a push dropped would make seeding fail.
        self.seed.tables.retain(|t, _| order.contains(t));
        for t in &order {
            self.seed.tables.entry(t.clone()).or_insert((true, self.seed.rows));
        }

        ui.heading("Generate fake data");
        ui.label(RichText::new(
            "Fills tables with realistic values. Foreign keys point at existing rows, parents are filled first, and unique and check constraints are respected.",
        ).weak());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Rows per table:");
            let before = self.seed.rows;
            ui.add(egui::DragValue::new(&mut self.seed.rows).range(1..=100_000));
            if before != self.seed.rows {
                for (_, n) in self.seed.tables.values_mut() {
                    *n = self.seed.rows;
                }
            }
            ui.separator();
            ui.label("Seed:");
            ui.add(egui::DragValue::new(&mut self.seed.seed));
            if ui.button("🎲").on_hover_text("New random seed").clicked() {
                self.seed.seed = seed::random_seed();
            }
        });
        ui.add_space(6.0);

        egui::Grid::new("seed_tables").striped(true).num_columns(4).spacing([24.0, 6.0]).show(ui, |ui| {
            ui.strong("Table");
            ui.strong("Current rows");
            ui.strong("Insert");
            ui.strong("Result");
            ui.end_row();
            let report: HashMap<&str, &TableReport> = match &self.seed.report {
                Some(Ok(r)) => r.iter().map(|r| (r.table.as_str(), r)).collect(),
                _ => HashMap::new(),
            };
            for t in &order {
                let entry = self.seed.tables.get_mut(t).unwrap();
                ui.checkbox(&mut entry.0, t);
                ui.label(RichText::new(counts.get(t).copied().unwrap_or(0).to_string()).monospace());
                ui.add_enabled(entry.0, egui::DragValue::new(&mut entry.1).range(0..=1_000_000));
                match report.get(t.as_str()) {
                    Some(r) if r.note.is_some() => {
                        ui.colored_label(warn_color(ui), format!("{} / {}  {}", r.inserted, r.requested, r.note.as_deref().unwrap()));
                    }
                    Some(r) => {
                        ui.colored_label(ok_color(ui), format!("✔ {} inserted", r.inserted));
                    }
                    None => {
                        ui.label("");
                    }
                }
                ui.end_row();
            }
        });
        ui.add_space(10.0);
        let any = self.seed.tables.values().any(|(on, n)| *on && *n > 0);
        let read_only = self.db.as_ref().is_some_and(|d| d.safe_mode == SafeMode::ReadOnly);
        let seeding = self.db_running(SEEDING);
        let seed = ui
            .horizontal(|ui| {
                let seed = ui
                    .add_enabled(any && !read_only && !seeding, egui::Button::new(RichText::new("🌱 Seed database").strong()))
                    .on_disabled_hover_text(if seeding {
                        "Seeding…"
                    } else if read_only {
                        "The connection is read-only"
                    } else {
                        "Choose tables to fill"
                    });
                if seeding {
                    ui.spinner();
                }
                seed
            })
            .inner;
        if seed.clicked() {
            if self.db.as_ref().is_some_and(|d| d.safe_mode == SafeMode::Confirm) {
                let n = self.seed.tables.values().filter(|(on, n)| *on && *n > 0).count();
                let statements = vec![(format!("-- insert fake rows into {n} table(s)"), None)];
                self.dialog = Some(Dialog::ConfirmWrite(Box::new(ConfirmWrite { statements, write: Write::Seed })));
            } else {
                self.seed_now();
            }
        }
        if let Some(Err(e)) = &self.seed.report {
            ui.colored_label(error_color(ui), format!("✖ {e}"));
        }
    }

    pub(crate) fn seed_now(&mut self) {
        let opts = SeedOptions {
            rows: self.seed.rows,
            counts: self.seed.tables.iter().filter(|(_, (on, _))| *on).map(|(t, (_, n))| (t.clone(), *n)).collect(),
            only: self.seed.tables.iter().filter(|(_, (on, _))| *on).map(|(t, _)| t.clone()).collect(),
            seed: self.seed.seed,
        };
        self.seed.report = None;
        self.db_run(
            SEEDING,
            move |conn| (seed::seed(conn, &opts).map_err(|e| format!("{e:#}")), opts.seed),
            |app, (res, seed)| {
                if let Ok(r) = &res {
                    let total: usize = r.iter().map(|r| r.inserted).sum();
                    app.set_status(format!("Inserted {total} rows (seed {seed})"), false);
                }
                app.seed.report = Some(res);
                app.refresh_db();
            },
        );
    }
}
