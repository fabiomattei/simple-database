//! Virtualized grid for query results.

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use simple_database::db::{Cell, QueryResult};

pub fn result_grid(ui: &mut egui::Ui, id: egui::Id, r: &QueryResult) {
    if let Some(n) = r.changes {
        ui.label(format!("✔ {n} row{} affected", if n == 1 { "" } else { "s" }));
        return;
    }
    if r.columns.is_empty() {
        return;
    }
    let null_color = ui.visuals().weak_text_color();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 6.0;

    ui.push_id(id, |ui| {
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .auto_shrink([false, true])
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(44.0)); // row number
        for _ in &r.columns {
            table = table.column(Column::auto_with_initial_suggestion(120.0).at_least(40.0).at_most(600.0).clip(true));
        }
        table
            .header(row_h + 2.0, |mut header| {
                header.col(|ui| {
                    ui.label(RichText::new("#").weak());
                });
                for c in &r.columns {
                    header.col(|ui| {
                        ui.strong(c);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, r.rows.len(), |mut row| {
                    let i = row.index();
                    row.col(|ui| {
                        ui.label(RichText::new((i + 1).to_string()).weak().monospace());
                    });
                    for cell in &r.rows[i] {
                        row.col(|ui| {
                            let text = match cell {
                                Cell::Null => RichText::new("NULL").italics().color(null_color),
                                _ if cell.is_number() => RichText::new(cell.display()).monospace().color(number_color(ui)),
                                _ => RichText::new(cell.display().replace('\n', "⏎")).monospace(),
                            };
                            ui.add(egui::Label::new(text).truncate()).on_hover_text(cell.display());
                        });
                    }
                });
            });
    });
}

pub fn number_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0xb5, 0xce, 0xa8)
    } else {
        Color32::from_rgb(0x09, 0x86, 0x58)
    }
}
