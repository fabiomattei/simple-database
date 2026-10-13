//! Entity-relationship diagrams.

use crate::schema::*;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DiagramFormat {
    /// Mermaid erDiagram (renders on GitHub, GitLab, Notion, ...)
    Mermaid,
    /// Graphviz DOT (render with `dot -Tsvg`)
    Dot,
}

pub fn render(schema: &Schema, format: DiagramFormat) -> String {
    match format {
        DiagramFormat::Mermaid => mermaid(schema),
        DiagramFormat::Dot => dot(schema),
    }
}

/// Mermaid attribute types can't contain parentheses or commas.
fn short_type(t: &DataType) -> String {
    let s = t.to_string();
    let base = s.split('(').next().unwrap_or(&s);
    base.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

fn mermaid(schema: &Schema) -> String {
    let mut out = String::from("erDiagram\n");
    for t in &schema.tables {
        let pk = t.primary_key();
        out.push_str(&format!("    {} {{\n", t.name));
        for c in &t.columns {
            let mut keys = Vec::new();
            if pk.contains(&c.name) {
                keys.push("PK");
            }
            if c.references.is_some() {
                keys.push("FK");
            }
            if c.unique {
                keys.push("UK");
            }
            let comment = if c.nullable || pk.contains(&c.name) { "" } else { " \"not null\"" };
            out.push_str(&format!(
                "        {} {}{}{}\n",
                short_type(&c.data_type),
                c.name,
                if keys.is_empty() { String::new() } else { format!(" {}", keys.join(",")) },
                comment
            ));
        }
        out.push_str("    }\n");
    }
    for t in &schema.tables {
        for c in &t.columns {
            let Some(fk) = &c.references else { continue };
            if schema.table(&fk.table).is_none() {
                continue;
            }
            let parent = if c.nullable { "|o" } else { "||" };
            let child = if c.unique { "o|" } else { "o{" };
            out.push_str(&format!(
                "    {} {parent}--{child} {} : \"{}\"\n",
                fk.table, t.name, c.name
            ));
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn dot(schema: &Schema) -> String {
    let mut out = String::from(
        "digraph schema {\n    graph [rankdir=LR, fontname=\"Helvetica\"];\n    node [shape=plain, fontname=\"Helvetica\", fontsize=11];\n    edge [arrowhead=crow, arrowtail=none, fontname=\"Helvetica\", fontsize=9];\n\n",
    );
    for t in &schema.tables {
        let pk = t.primary_key();
        out.push_str(&format!(
            "    \"{}\" [label=<<table border=\"0\" cellborder=\"1\" cellspacing=\"0\" cellpadding=\"4\">\n        <tr><td colspan=\"2\" bgcolor=\"#d0e0ff\"><b>{}</b></td></tr>\n",
            t.name,
            html_escape(&t.name)
        ));
        for c in &t.columns {
            let mut name = html_escape(&c.name);
            if pk.contains(&c.name) {
                name = format!("<u>{name}</u>");
            }
            if !c.nullable {
                name = format!("<b>{name}</b>");
            }
            out.push_str(&format!(
                "        <tr><td port=\"{}\" align=\"left\">{name}</td><td align=\"left\">{}</td></tr>\n",
                html_escape(&c.name),
                html_escape(&c.data_type.to_string())
            ));
        }
        out.push_str("    </table>>];\n");
    }
    out.push('\n');
    for t in &schema.tables {
        for c in &t.columns {
            if let Some(fk) = &c.references
                && schema.table(&fk.table).is_some()
            {
                out.push_str(&format!(
                    "    \"{}\":\"{}\" -> \"{}\":\"{}\";\n",
                    fk.table, fk.column, t.name, c.name
                ));
            }
        }
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn mermaid_relationships() {
        let s = parse(
            "table users { id int pk\n email varchar(255) unique not null }\n\
             table posts { id int pk\n author_id int not null -> users.id\n editor_id int -> users.id }",
        )
        .unwrap();
        let m = render(&s, DiagramFormat::Mermaid);
        assert!(m.contains("varchar email UK \"not null\""));
        assert!(m.contains("users ||--o{ posts : \"author_id\""));
        assert!(m.contains("users |o--o{ posts : \"editor_id\""));
        let d = render(&s, DiagramFormat::Dot);
        assert!(d.contains("\"users\":\"id\" -> \"posts\":\"author_id\";"));
    }

    #[test]
    fn skips_relationships_to_missing_tables() {
        let mut s = parse("table users { id int pk }\ntable posts { id int pk\n author_id int -> users.id }").unwrap();
        s.tables.retain(|t| t.name == "posts");
        assert!(!render(&s, DiagramFormat::Mermaid).contains("users"));
        assert!(!render(&s, DiagramFormat::Dot).contains("users"));
    }
}
