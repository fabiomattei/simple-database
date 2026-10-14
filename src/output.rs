//! Rendering query results.

use crate::db::{Cell, QueryResult};
use crate::ddl::{Dialect, quote, quote_table};
use clap::ValueEnum;
use comfy_table::{Cell as TCell, Color, ContentArrangement, Table, presets};

/// A color for terminal output. It is only written when the stream is a
/// terminal: not when piped or redirected, nor with `NO_COLOR` set;
/// `CLICOLOR_FORCE` turns it on regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    Red,
    Green,
    Yellow,
    Dim,
}

impl Paint {
    /// `s` in this color, for stdout.
    pub fn out(self, s: impl std::fmt::Display) -> String {
        self.paint(s, colors(Stream::Stdout))
    }

    /// `s` in this color, for stderr.
    pub fn err(self, s: impl std::fmt::Display) -> String {
        self.paint(s, colors(Stream::Stderr))
    }

    fn paint(self, s: impl std::fmt::Display, on: bool) -> String {
        if !on {
            return s.to_string();
        }
        let code = match self {
            Paint::Red => "31",
            Paint::Green => "32",
            Paint::Yellow => "33",
            Paint::Dim => "2",
        };
        format!("\x1b[{code}m{s}\x1b[0m")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Whether output to `stream` gets colors (see [`Paint`]).
pub fn colors(stream: Stream) -> bool {
    use std::io::IsTerminal;
    use std::sync::OnceLock;
    static OUT: OnceLock<bool> = OnceLock::new();
    static ERR: OnceLock<bool> = OnceLock::new();
    let tty = || match stream {
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
    };
    let decide = || colors_for(std::env::var_os("NO_COLOR"), std::env::var_os("CLICOLOR_FORCE"), tty());
    *match stream {
        Stream::Stdout => OUT.get_or_init(decide),
        Stream::Stderr => ERR.get_or_init(decide),
    }
}

/// `NO_COLOR` (any non-empty value) wins, then `CLICOLOR_FORCE` (not `0`), then the terminal.
fn colors_for(no_color: Option<std::ffi::OsString>, force: Option<std::ffi::OsString>, tty: bool) -> bool {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if force.is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    tty
}

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
            // A Markdown cell must stay on one line, and `|` would end it.
            let text = |s: String| match format {
                OutputFormat::Markdown => s.replace('|', "\\|").replace("\r\n", "<br>").replace(['\n', '\r'], "<br>"),
                _ => s,
            };
            let color = colors(Stream::Stdout);
            if color {
                // Styled even when comfy-table wouldn't (CLICOLOR_FORCE).
                t.enforce_styling();
            }
            let paint = |cell: TCell, c: Color| if color { cell.fg(c) } else { cell };
            t.set_header(r.columns.iter().map(|c| paint(TCell::new(text(c.clone())), Color::Cyan)));
            for row in &r.rows {
                t.add_row(row.iter().map(|c| match c {
                    Cell::Null if format == OutputFormat::Table => paint(TCell::new("NULL"), Color::DarkGrey),
                    _ if c.is_number() => TCell::new(c.display()).set_alignment(comfy_table::CellAlignment::Right),
                    _ => TCell::new(text(c.display())),
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
            let mut out = csv_line(r.columns.iter().cloned().map(Some));
            for row in &r.rows {
                out.push('\n');
                out.push_str(&csv_line(row.iter().map(|c| match c {
                    Cell::Null => None,
                    _ => Some(c.display()),
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
            let head = format!("INSERT INTO {} ({}) VALUES", quote_table(table, d), cols.join(", "));
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

/// One CSV line. NULL is an empty field and an empty string is `""`, as `import` reads them.
fn csv_line(fields: impl Iterator<Item = Option<String>>) -> String {
    fields
        .map(|f| match f {
            None => String::new(),
            Some(f) if f.is_empty() || f.contains([',', '"', '\n', '\r']) => format!("\"{}\"", f.replace('"', "\"\"")),
            Some(f) => f,
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_choice() {
        let v = |s: &str| Some(std::ffi::OsString::from(s));
        assert!(colors_for(None, None, true));
        assert!(!colors_for(None, None, false));
        assert!(!colors_for(v("1"), None, true), "NO_COLOR");
        assert!(colors_for(v(""), None, true), "an empty NO_COLOR doesn't count");
        assert!(colors_for(None, v("1"), false), "CLICOLOR_FORCE");
        assert!(!colors_for(None, v("0"), false));
        assert!(!colors_for(v("1"), v("1"), true), "NO_COLOR wins");
        assert_eq!(Paint::Red.paint("x", true), "\x1b[31mx\x1b[0m");
        assert_eq!(Paint::Red.paint("x", false), "x");
    }

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

    fn result(columns: &[&str], rows: Vec<Vec<Cell>>) -> QueryResult {
        QueryResult { columns: columns.iter().map(|c| c.to_string()).collect(), rows, changes: None, truncated: false }
    }

    #[test]
    fn renders_formats() {
        let r = result(&["n", "s"], vec![vec![Cell::Real(1.5), Cell::Text("x".into())], vec![Cell::Null, Cell::Text("é".into())]]);
        let table = render(&r, OutputFormat::Table);
        assert!(table.contains("NULL") && table.ends_with("(2 rows)"), "{table}");
        assert_eq!(render(&r, OutputFormat::Csv), "n,s\n1.5,x\n,é");
        assert_eq!(render(&r, OutputFormat::Json), "[\n  {\n    \"n\": 1.5,\n    \"s\": \"x\"\n  },\n  {\n    \"n\": null,\n    \"s\": \"é\"\n  }\n]");
        assert_eq!(render(&result(&["a"], vec![]), OutputFormat::Json), "[]");
        assert_eq!(render(&result(&["a"], vec![]), OutputFormat::Csv), "a");
        let changed = QueryResult { changes: Some(1), ..result(&[], vec![]) };
        assert_eq!(render(&changed, OutputFormat::Json), "1 row affected");
    }

    #[test]
    fn csv_tells_empty_text_from_null() {
        // `import` reads an empty field as NULL and "" as an empty string.
        let r = result(&["a", "b"], vec![vec![Cell::Text(String::new()), Cell::Null]]);
        assert_eq!(render(&r, OutputFormat::Csv), "a,b\n\"\",");
        let rows = crate::import::scan_text(&render(&r, OutputFormat::Csv), crate::import::ImportFormat::Csv).unwrap();
        assert_eq!(rows.preview, [[Cell::Text(String::new()), Cell::Null]]);
    }

    #[test]
    fn markdown_cells_stay_in_their_column() {
        let r = result(&["a|b"], vec![vec![Cell::Text("x | y\nz".into())]]);
        let md = render(&r, OutputFormat::Markdown);
        let lines: Vec<&str> = md.lines().collect();
        assert_eq!(lines.len(), 3, "{md}");
        assert!(lines.iter().all(|l| l.matches('|').count() - l.matches("\\|").count() == 2), "{md}");
    }
}
