//! Semantic checks: errors make the schema unusable, warnings flag questionable design.

use crate::output::Paint;
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

            if let DataType::Enum(values) = &c.data_type {
                if let Some(v) = values.iter().enumerate().find(|(i, v)| values[..*i].contains(v)).map(|(_, v)| v) {
                    error(loc.clone(), format!("enum value '{v}' is listed twice"));
                }
                if let Some(DefaultValue::String(v)) = &c.default
                    && !values.contains(v)
                {
                    error(loc.clone(), format!("default '{v}' is not one of the enum's values"));
                }
            }
        }

        for fk in t.all_foreign_keys() {
            let single = fk.columns.len() == 1;
            // A column's own key is reported at the column, a multi-column one at the table.
            let loc = if single { format!("{}.{}", t.name, fk.columns[0]) } else { t.name.clone() };
            let what = if single { String::new() } else { format!("foreign key ({}) ", fk.columns.join(", ")) };
            let cols: Vec<&Column> = fk.columns.iter().filter_map(|c| t.column(c)).collect();
            if let Some(missing) = fk.columns.iter().find(|c| t.column(c).is_none()) {
                error(loc.clone(), format!("{what}uses unknown column '{missing}'"));
                continue;
            }
            if fk.columns.len() != fk.ref_columns.len() {
                error(loc.clone(), format!("{what}has {} columns but references {}", fk.columns.len(), fk.ref_columns.len()));
                continue;
            }
            let Some(target_table) = schema.table(&fk.table) else {
                error(loc.clone(), format!("{what}references unknown table '{}'", fk.table));
                continue;
            };
            let Some(targets) = fk.ref_columns.iter().map(|c| target_table.column(c)).collect::<Option<Vec<&Column>>>() else {
                let missing = fk.ref_columns.iter().find(|c| target_table.column(c).is_none()).expect("one is missing");
                error(loc.clone(), format!("{what}references unknown column '{}.{missing}'", fk.table));
                continue;
            };

            let set_null = fk.on_delete == RefAction::SetNull || fk.on_update == RefAction::SetNull;
            if set_null && cols.iter().any(|c| !c.nullable) {
                error(loc.clone(), format!("{what}uses SET NULL but the column is NOT NULL"));
            }
            let same = |a: &[String]| a.len() == fk.ref_columns.len() && a.iter().all(|c| fk.ref_columns.contains(c));
            let target_is_key = same(&target_table.primary_key())
                || (single && targets[0].unique)
                || target_table.indexes.iter().any(|i| i.unique && same(&i.columns));
            let shown = if single { format!("{}.{}", fk.table, fk.ref_columns[0]) } else { format!("{} ({})", fk.table, fk.ref_columns.join(", ")) };
            if !target_is_key {
                error(loc.clone(), format!("{what}references {shown} which is neither a primary key nor unique"));
            }
            for (c, target) in cols.iter().zip(&targets) {
                if !types_compatible(&c.data_type, &target.data_type) {
                    let place = if single { String::new() } else { format!("{} ", c.name) };
                    warn(
                        loc.clone(),
                        format!("{place}type {} differs from referenced {}.{} ({})", c.data_type, fk.table, target.name, target.data_type),
                    );
                }
            }
            // An index whose first columns are the key's, in any order.
            let leads = |idx: &[String]| idx.len() >= fk.columns.len() && fk.columns.iter().all(|c| idx[..fk.columns.len()].contains(c));
            let indexed = t.indexes.iter().any(|i| leads(&i.columns)) || leads(&t.primary_key()) || (single && cols[0].unique);
            if !indexed {
                warn(
                    loc.clone(),
                    format!(
                        "{}is not indexed; joins and cascading deletes on '{}' will scan the table (add `index ({})`)",
                        if single { "foreign key " } else { &what },
                        fk.table,
                        fk.columns.join(", ")
                    ),
                );
            }
        }
    }

    issues.extend(warnings);
    issues
}

pub fn print_issues(issues: &[Issue]) {
    if issues.is_empty() {
        println!("{}", Paint::Green.out("✓ no issues found"));
        return;
    }
    for i in issues {
        let label = match i.severity {
            Severity::Error => Paint::Red.out("error"),
            Severity::Warning => Paint::Yellow.out("warning"),
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
    fn checks_multi_column_keys_and_enums() {
        let m = messages(
            "table a { x int\n y int\n s enum('on', 'off', 'on') default 'maybe'\n pk (x, y) }\n\
             table b { id int pk\n x int not null\n y int\n fk (x, y) -> a (y, x) on delete set null }\n\
             table c { id int pk\n x int\n y int\n fk (x, y) -> a (x, s)\n fk (y, z) -> a (x, y)\n index (x, y) }",
        );
        let joined = m.join("\n");
        assert!(joined.contains("Error a.s: enum value 'on' is listed twice"), "{joined}");
        assert!(joined.contains("Error a.s: default 'maybe' is not one of the enum's values"), "{joined}");
        assert!(joined.contains("Error b: foreign key (x, y) uses SET NULL but the column is NOT NULL"), "{joined}");
        assert!(joined.contains("Warning b: foreign key (x, y) is not indexed"), "{joined}");
        assert!(joined.contains("Error c: foreign key (x, y) references a (x, s) which is neither a primary key nor unique"), "{joined}");
        assert!(joined.contains("Error c: foreign key (y, z) uses unknown column 'z'"), "{joined}");
        assert!(!joined.contains("Error b: foreign key (x, y) references"), "the key's columns in another order: {joined}");
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
