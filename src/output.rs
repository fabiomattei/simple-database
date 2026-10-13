//! Rendering query results.

use crate::db::{Cell, QueryResult};
use crate::ddl::{Dialect, quote};
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

/// File formats for exporting a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    Jsonl,
    Markdown,
    Html,
    /// INSERT statements.
    Sql,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 7] = [
        ExportFormat::Csv,
        ExportFormat::Tsv,
        ExportFormat::Json,
        ExportFormat::Jsonl,
        ExportFormat::Markdown,
        ExportFormat::Html,
        ExportFormat::Sql,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::Jsonl => "JSON Lines",
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Html => "HTML",
            ExportFormat::Sql => "SQL INSERT",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::Jsonl => "jsonl",
            ExportFormat::Markdown => "md",
            ExportFormat::Html => "html",
            ExportFormat::Sql => "sql",
        }
    }
}

/// `r` as a file. `table` names the target of SQL INSERTs and the HTML caption.
pub fn export(r: &QueryResult, format: ExportFormat, table: &str, d: Dialect) -> String {
    let text = |c: &Cell| match c {
        Cell::Null => String::new(),
        _ => c.display(),
    };
    let mut out = match format {
        ExportFormat::Csv => render(r, OutputFormat::Csv),
        ExportFormat::Json => render(r, OutputFormat::Json),
        ExportFormat::Markdown => render(r, OutputFormat::Markdown),
        ExportFormat::Tsv => {
            let field = |s: String| s.replace(['\t', '\n', '\r'], " ");
            let mut lines = vec![r.columns.iter().cloned().map(field).collect::<Vec<_>>().join("\t")];
            lines.extend(r.rows.iter().map(|row| row.iter().map(|c| field(text(c))).collect::<Vec<_>>().join("\t")));
            lines.join("\n")
        }
        ExportFormat::Jsonl => r
            .rows
            .iter()
            .map(|row| {
                let obj: serde_json::Map<String, serde_json::Value> =
                    r.columns.iter().cloned().zip(row.iter().map(Cell::to_json)).collect();
                serde_json::Value::Object(obj).to_string()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ExportFormat::Html => {
            let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
            let mut h = format!(
                "<!doctype html>\n<meta charset=\"utf-8\">\n<title>{t}</title>\n\
                 <style>body{{font-family:sans-serif}}table{{border-collapse:collapse}}\
                 th,td{{border:1px solid #ccc;padding:4px 8px;text-align:left}}td.n{{text-align:right}}td.null{{color:#999}}</style>\n\
                 <table>\n<caption>{t}</caption>\n<tr>",
                t = esc(table)
            );
            for c in &r.columns {
                h += &format!("<th>{}</th>", esc(c));
            }
            h += "</tr>\n";
            for row in &r.rows {
                h += "<tr>";
                for c in row {
                    h += &match c {
                        Cell::Null => "<td class=\"null\">NULL</td>".to_string(),
                        _ if c.is_number() => format!("<td class=\"n\">{}</td>", esc(&c.display())),
                        _ => format!("<td>{}</td>", esc(&c.display())),
                    };
                }
                h += "</tr>\n";
            }
            h + "</table>"
        }
        ExportFormat::Sql => {
            let cols: Vec<String> = r.columns.iter().map(|c| quote(c, d)).collect();
            let head = format!("INSERT INTO {} ({}) VALUES", quote(table, d), cols.join(", "));
            r.rows
                .iter()
                .map(|row| {
                    let vals: Vec<String> = row
                        .iter()
                        .map(|c| crate::data::literal(c, None, d).unwrap_or_else(|_| "NULL".into()))
                        .collect();
                    format!("{head} ({});", vals.join(", "))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    out.push('\n');
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_formats() {
        let r = QueryResult {
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![Cell::Int(1), Cell::Text("a<b>, \"c\"".into())], vec![Cell::Int(2), Cell::Null]],
            changes: None,
            truncated: false,
        };
        assert_eq!(export(&r, ExportFormat::Csv, "t", Dialect::Sqlite), "id,name\n1,\"a<b>, \"\"c\"\"\"\n2,\n");
        assert_eq!(export(&r, ExportFormat::Tsv, "t", Dialect::Sqlite), "id\tname\n1\ta<b>, \"c\"\n2\t\n");
        assert_eq!(export(&r, ExportFormat::Jsonl, "t", Dialect::Sqlite), "{\"id\":1,\"name\":\"a<b>, \\\"c\\\"\"}\n{\"id\":2,\"name\":null}\n");
        assert!(export(&r, ExportFormat::Html, "t", Dialect::Sqlite).contains("<td>a&lt;b&gt;, &quot;c&quot;</td>"));
        assert_eq!(
            export(&r, ExportFormat::Sql, "order", Dialect::Mysql),
            "INSERT INTO `order` (id, name) VALUES (1, 'a<b>, \"c\"');\nINSERT INTO `order` (id, name) VALUES (2, NULL);\n"
        );
    }
}
