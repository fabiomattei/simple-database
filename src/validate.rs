//! Semantic checks: errors make the schema unusable, warnings flag questionable design.

use crate::schema::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub severity: Severity,
    pub location: String,
    pub message: String,
}

pub fn validate(schema: &Schema) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut error = |loc: String, msg: String| {
        issues.push(Issue {
            severity: Severity::Error,
            location: loc,
            message: msg,
        })
    };
    let mut warnings = Vec::new();
    let mut warn = |loc: String, msg: String| {
        warnings.push(Issue {
            severity: Severity::Warning,
            location: loc,
            message: msg,
        })
    };

    for t in &schema.tables {
        if t.columns.is_empty() {
            error(t.name.clone(), "table has no columns".into());
        }

        // A rename hint has to say which table or column the old one became.
        for w in &t.was {
            if schema.tables.iter().any(|o| o.name != t.name && o.was.contains(w)) {
                error(t.name.clone(), format!("more than one table says it was '{w}'"));
            }
        }
        for c in &t.columns {
            for w in &c.was {
                if t.columns.iter().any(|o| o.name != c.name && o.was.contains(w)) {
                    error(format!("{}.{}", t.name, c.name), format!("more than one column says it was '{w}'"));
                }
            }
        }

        let inline_pks: Vec<&Column> = t.columns.iter().filter(|c| c.primary_key).collect();
        if inline_pks.len() > 1 {
            error(
                t.name.clone(),
                format!(
                    "multiple columns marked pk; declare a composite key with `pk ({})`",
                    inline_pks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
            );
        }
        if t.primary_key().is_empty() {
            warn(t.name.clone(), "table has no primary key".into());
        }
        for pk in &t.composite_pk {
            if t.column(pk).is_none() {
                error(t.name.clone(), format!("primary key references unknown column '{pk}'"));
            }
        }

        for idx in &t.indexes {
            for c in &idx.columns {
                if t.column(c).is_none() {
                    error(
                        format!("{}.{}", t.name, idx.name),
                        format!("index references unknown column '{c}'"),
                    );
                }
            }
        }

        for c in &t.columns {
            let loc = format!("{}.{}", t.name, c.name);

            if c.auto_increment {
                if !c.data_type.is_integer() {
                    error(loc.clone(), format!("autoincrement requires an integer type, not {}", c.data_type));
                }
                if !c.primary_key {
                    error(loc.clone(), "autoincrement is only supported on a primary key column".into());
                }
            }
            if !c.nullable && c.default == Some(DefaultValue::Null) {
                error(loc.clone(), "column is NOT NULL but defaults to null".into());
            }
            if let DataType::Custom(ty) = &c.data_type {
                warn(loc.clone(), format!("unknown type '{ty}' will be passed through verbatim"));
            }

            let Some(fk) = &c.references else { continue };
            let Some(target_table) = schema.table(&fk.table) else {
                error(loc.clone(), format!("references unknown table '{}'", fk.table));
                continue;
            };
            let Some(target) = target_table.column(&fk.column) else {
                error(loc.clone(), format!("references unknown column '{}.{}'", fk.table, fk.column));
                continue;
            };

            if (fk.on_delete == RefAction::SetNull || fk.on_update == RefAction::SetNull) && !c.nullable {
                error(loc.clone(), "uses SET NULL but the column is NOT NULL".into());
            }
            let target_is_key = target_table.primary_key() == [target.name.clone()]
                || target.unique
                || target_table
                    .indexes
                    .iter()
                    .any(|i| i.unique && i.columns == [target.name.clone()]);
            if !target_is_key {
                error(
                    loc.clone(),
                    format!("references {}.{} which is neither a primary key nor unique", fk.table, fk.column),
                );
            }
            if !types_compatible(&c.data_type, &target.data_type) {
                warn(
                    loc.clone(),
                    format!(
                        "type {} differs from referenced {}.{} ({})",
                        c.data_type, fk.table, fk.column, target.data_type
                    ),
                );
            }
            let indexed = t.indexes.iter().any(|i| i.columns.first() == Some(&c.name))
                || t.primary_key().first() == Some(&c.name)
                || c.unique;
            if !indexed {
                warn(
                    loc.clone(),
                    format!("foreign key is not indexed; joins and cascading deletes on '{}' will scan the table (add `index ({})`)", fk.table, c.name),
                );
            }
        }
    }

    issues.extend(warnings);
    issues
}

pub fn print_issues(issues: &[Issue]) {
    if issues.is_empty() {
        println!("\x1b[32m✓ no issues found\x1b[0m");
        return;
    }
    for i in issues {
        let label = match i.severity {
            Severity::Error => "\x1b[31merror\x1b[0m",
            Severity::Warning => "\x1b[33mwarning\x1b[0m",
        };
        println!("{label} [{}]: {}", i.location, i.message);
    }
}

fn types_compatible(a: &DataType, b: &DataType) -> bool {
    a == b || (a.is_integer() && b.is_integer()) || matches!((a, b), (DataType::Text | DataType::Varchar(_), DataType::Text | DataType::Varchar(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn messages(src: &str) -> Vec<String> {
        validate(&parse(src).unwrap())
            .into_iter()
            .map(|i| format!("{:?} {}: {}", i.severity, i.location, i.message))
            .collect()
    }

    #[test]
    fn clean_schema_has_no_issues() {
        let m = messages(
            "table a { id int pk }\n table b { id int pk\n a_id int -> a.id\n index (a_id) }",
        );
        assert!(m.is_empty(), "{m:?}");
    }

    #[test]
    fn detects_problems() {
        let m = messages(
            "table a { id int pk\n name text }\n\
             table b { x text autoincrement\n a_id int not null -> a.id on delete set null\n n_id int -> a.name\n z int -> nope.id }",
        );
        let joined = m.join("\n");
        assert!(joined.contains("Warning b: table has no primary key"), "{joined}");
        assert!(joined.contains("autoincrement requires an integer"), "{joined}");
        assert!(joined.contains("SET NULL but the column is NOT NULL"), "{joined}");
        assert!(joined.contains("neither a primary key nor unique"), "{joined}");
        assert!(joined.contains("unknown table 'nope'"), "{joined}");
        assert!(joined.contains("foreign key is not indexed"), "{joined}");
    }
}
