//! Pretty-print a schema back to the `.dbs` DSL.

use crate::schema::*;

pub fn format_schema(schema: &Schema) -> String {
    schema
        .tables
        .iter()
        .map(format_table)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_table(t: &Table) -> String {
    let name_w = t.columns.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let types: Vec<String> = t.columns.iter().map(|c| c.data_type.to_string()).collect();
    let type_w = types.iter().map(|s| s.len()).max().unwrap_or(0);

    let mut out = format!("table {} {{\n", t.name);
    for (c, ty) in t.columns.iter().zip(&types) {
        let mods = modifiers(c);
        let line = format!("  {:name_w$}  {:type_w$}  {}", c.name, ty, mods.join(" "));
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if !t.composite_pk.is_empty() || !t.indexes.is_empty() {
        out.push('\n');
    }
    if !t.composite_pk.is_empty() {
        out.push_str(&format!("  pk ({})\n", t.composite_pk.join(", ")));
    }
    for idx in &t.indexes {
        let default_name = format!(
            "{}_{}_{}",
            t.name,
            idx.columns.join("_"),
            if idx.unique { "key" } else { "idx" }
        );
        let name = if idx.name == default_name { String::new() } else { format!("{} ", idx.name) };
        out.push_str(&format!(
            "  {}index {name}({})\n",
            if idx.unique { "unique " } else { "" },
            idx.columns.join(", ")
        ));
    }
    out.push_str("}\n");
    out
}

fn modifiers(c: &Column) -> Vec<String> {
    let mut m = Vec::new();
    if c.primary_key {
        m.push("pk".to_string());
    }
    if c.auto_increment {
        m.push("autoincrement".to_string());
    }
    if c.unique {
        m.push("unique".to_string());
    }
    if !c.nullable && !c.primary_key {
        m.push("not null".to_string());
    }
    if let Some(d) = &c.default {
        m.push(format!("default {d}"));
    }
    if let Some(chk) = &c.check {
        m.push(format!("check ({chk})"));
    }
    if let Some(fk) = &c.references {
        m.push(format!("-> {}.{}", fk.table, fk.column));
        if fk.on_delete != RefAction::NoAction {
            m.push(format!("on delete {}", fk.on_delete.dsl()));
        }
        if fk.on_update != RefAction::NoAction {
            m.push(format!("on update {}", fk.on_update.dsl()));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn round_trips() {
        let src = "table users { id int pk autoincrement\n email varchar(255) unique not null\n \
                   role text default 'it''s' check (role <> '')\n boss_id int -> users.id on delete set null\n \
                   created timestamp default now\n index (boss_id)\n unique index by_role (role, email) }\n\
                   table tags { a int\n b text\n pk (a, b) }";
        let schema = parse(src).unwrap();
        let printed = format_schema(&schema);
        assert_eq!(parse(&printed).unwrap(), schema, "{printed}");
    }
}
