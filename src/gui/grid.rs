//! Virtualized grid for query results.

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use simple_database::db::{Cell, QueryResult};

/// Widest a column starts out; drag its edge for more.
const MAX_WIDTH: f32 = 360.0;
/// Rows measured for the starting widths.
const SAMPLE_ROWS: usize = 200;

/// Starting widths of a grid's columns: each as wide as its header and the
/// widest value in the first rows, up to [`MAX_WIDTH`]. Cells are truncated
/// labels, which egui's auto sizing can't measure (they take the width they
/// get), so this measures the text. Cached under `key`.
pub fn fitted_widths(ui: &egui::Ui, key: egui::Id, headers: &[String], rows: &[Vec<Cell>]) -> Vec<f32> {
    let key = key.with("fitted_widths");
    if let Some(w) = ui.data(|d| d.get_temp::<Vec<f32>>(key)).filter(|w| w.len() == headers.len()) {
        return w;
    }
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    let body = egui::TextStyle::Body.resolve(ui.style());
    let char_w = ui.fonts_mut(|f| f.glyph_width(&mono, '0'));
    let pad = ui.spacing().item_spacing.x * 2.0 + 6.0;
    let widths: Vec<f32> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let header = ui.fonts_mut(|f| f.layout_no_wrap(h.clone(), body.clone(), Color32::WHITE).size().x);
            let chars = rows
                .iter()
                .take(SAMPLE_ROWS)
                .filter_map(|r| r.get(i))
                .map(|c| if *c == Cell::Null { 4 } else { c.display().chars().take(80).count() })
                .max()
                .unwrap_or(0);
            (header.max(chars as f32 * char_w) + pad).clamp(40.0, MAX_WIDTH)
        })
        .collect();
    ui.data_mut(|d| d.insert_temp(key, widths.clone()));
    widths
}

/// How many times "Fit columns to content" was asked for in grid `key`; part
/// of the table's id, so egui forgets widths the user dragged.
pub fn fit_generation(ui: &egui::Ui, key: egui::Id) -> u32 {
    ui.data(|d| d.get_temp(key.with("fit_generation"))).unwrap_or(0)
}

/// The header's "Fit columns to content" item, for its context menu.
pub fn fit_columns_button(ui: &mut egui::Ui, key: egui::Id) {
    if ui.button("Fit columns to content").clicked() {
        ui.data_mut(|d| *d.get_temp_mut_or_default::<u32>(key.with("fit_generation")) += 1);
        ui.close();
    }
}

pub fn result_grid(ui: &mut egui::Ui, id: egui::Id, r: &QueryResult) {
    if let Some(n) = r.changes {
        ui.label(format!("✔ {n} row{} affected", if n == 1 { "" } else { "s" }));
        return;
    }
    if r.columns.is_empty() {
        return;
    }
    let null_color = ui.visuals().weak_text_color();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 8.0;

    // Per result shape, so a different query starts from its own widths.
    let key = id.with((&r.columns, r.rows.len()));
    let generation = fit_generation(ui, key);
    let widths = fitted_widths(ui, key.with(generation), &r.columns, &r.rows);
    ui.push_id(key.with(generation), |ui| {
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .auto_shrink([false, true])
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(44.0)); // row number
        for w in widths {
            table = table.column(Column::initial(w).at_least(40.0).clip(true));
        }
        table
            .header(row_h + 2.0, |mut header| {
                header.col(|ui| {
                    crate::theme::header_cell(ui);
                    crate::theme::row_number(ui, RichText::new("#").weak());
                });
                for c in &r.columns {
                    header.col(|ui| {
                        crate::theme::header_cell(ui);
                        ui.add(egui::Label::new(crate::theme::header_text(c)).sense(egui::Sense::click()))
                            .on_hover_text("Right-click to fit the columns to their content")
                            .context_menu(|ui| fit_columns_button(ui, key));
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, r.rows.len(), |mut row| {
                    let i = row.index();
                    row.col(|ui| {
                        crate::theme::row_number(ui, RichText::new((i + 1).to_string()).weak().monospace());
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

/// Numbers in a grid: the text color, as in a spreadsheet.
pub fn number_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().text_color()
}
