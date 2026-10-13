//! Rendering query results.

use crate::db::{Cell, QueryResult};
use clap::ValueEnum;
use comfy_table::{Cell as TCell, Color, ContentArrangement, Table, presets};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Table,
    Csv,
    Json,
    Markdown,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        <Self as ValueEnum>::from_str(s, true).ok()
    }
}

pub fn render(r: &QueryResult, format: OutputFormat) -> String {
    if let Some(n) = r.changes {
        return format!("{n} row{} affected", if n == 1 { "" } else { "s" });
    }
    match format {
        OutputFormat::Table | OutputFormat::Markdown => {
            let mut t = Table::new();
            if format == OutputFormat::Markdown {
                t.load_style(presets::ASCII_MARKDOWN);
            } else {
                t.load_style(presets::UTF8_FULL_CONDENSED);
                t.set_content_arrangement(ContentArrangement::Dynamic);
            }
            t.set_header(r.columns.iter().map(|c| TCell::new(c).fg(Color::Cyan)));
            for row in &r.rows {
                t.add_row(row.iter().map(|c| match c {
                    Cell::Null if format == OutputFormat::Table => TCell::new("NULL").fg(Color::DarkGrey),
                    _ if c.is_number() => TCell::new(c.display()).set_alignment(comfy_table::CellAlignment::Right),
                    _ => TCell::new(c.display()),
                }));
            }
            let footer = if format == OutputFormat::Table {
                format!("\n({} row{})", r.rows.len(), if r.rows.len() == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            format!("{t}{footer}")
        }
        OutputFormat::Csv => {
            let mut out = csv_line(r.columns.iter().cloned());
            for row in &r.rows {
                out.push('\n');
                out.push_str(&csv_line(row.iter().map(|c| match c {
                    Cell::Null => String::new(),
                    _ => c.display(),
                })));
            }
            out
        }
        OutputFormat::Json => {
            let rows: Vec<serde_json::Value> = r
                .rows
                .iter()
                .map(|row| {
                    let obj: serde_json::Map<String, serde_json::Value> = r
                        .columns
                        .iter()
                        .cloned()
                        .zip(row.iter().map(Cell::to_json))
                        .collect();
                    serde_json::Value::Object(obj)
                })
                .collect();
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        }
    }
}

fn csv_line(fields: impl Iterator<Item = String>) -> String {
    fields
        .map(|f| {
            if f.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}
