//! What existing rows can make a migration fail: a column becoming NOT NULL,
//! a narrower or different type, a new unique key, check or foreign key.
//! `type_risk` explains a type change for the migration's warnings;
//! `row_checks` gives each risky change a query that counts the rows in its
//! way, which `Database::preflight` runs before a push.

use crate::ddl::*;
use crate::diff::{Plan, Rename};
use crate::schema::*;

/// Rows of an existing table that would make a migration fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowCheck {
    /// The table as the database has it now (before renames).
    pub table: String,
    /// What those rows have, e.g. `have NULL in slug, which becomes NOT NULL`.
    pub what: String,
    /// Counts the rows: one row, one column.
    pub sql: String,
}

impl RowCheck {
    pub fn message(&self, rows: u64) -> String {
        format!("{rows} row(s) of {} {}", self.table, self.what)
    }
}

/// Why changing a column's type from `o` to `n` can fail or lose data in
/// dialect `d`, if it can. SQLite keeps values as they are, whatever the
/// declared type; only its enums (a check) refuse values.
pub fn type_risk(o: &DataType, n: &DataType, d: Dialect) -> Option<String> {
    use DataType::*;
    if o == n {
        return None;
    }
    if let Enum(values) = n {
        return match o {
            Enum(old) => {
                let gone: Vec<&str> = old.iter().filter(|v| !values.contains(v)).map(String::as_str).collect();
                (!gone.is_empty()).then(|| format!("rows with {} fail", gone.iter().map(|v| format!("'{v}'")).collect::<Vec<_>>().join(", ")))
            }
            _ => Some("values not in the list fail".into()),
        };
    }
    if d == Dialect::Sqlite {
        return None;
    }
    match (o, n) {
        (_, Text) | (Enum(_), Varchar(_)) | (Date, Timestamp) => None,
        (Varchar(a), Varchar(b)) if b >= a => None,
        (Text | Varchar(_) | Custom(_), Varchar(b)) => Some(format!("values longer than {b} characters fail")),
        (SmallInt | Int | BigInt, SmallInt | Int | BigInt) if int_digits(o) <= int_digits(n) => None,
        (SmallInt | Int | BigInt, SmallInt | Int | BigInt) => Some(format!("values outside the {n} range fail")),
        (Real | Decimal(..), SmallInt | Int | BigInt) => Some(format!("fractions are rounded, and values outside the {n} range fail")),
        (_, Decimal(p, s)) if is_numeric(o) => {
            let mut why = Vec::new();
            if int_digits(o).is_none_or(|x| x > p - s) {
                why.push(format!("values with more than {} digit(s) before the point fail", p - s));
            }
            if matches!(o, Real) || matches!(o, Decimal(_, s0) if s0 > s) {
                why.push(format!("values are rounded to {s} decimal place(s)"));
            }
            (!why.is_empty()).then(|| why.join("; "))
        }
        (SmallInt | Int | BigInt | Decimal(..), Real) => None,
        (Timestamp, Date) => Some("the time of day is dropped".into()),
        (Timestamp, Time) => Some("the date is dropped".into()),
        _ => Some(format!("values that don't convert to {n} fail")),
    }
}

fn is_int(t: &DataType) -> bool {
    matches!(t, DataType::SmallInt | DataType::Int | DataType::BigInt)
}

fn is_numeric(t: &DataType) -> bool {
    is_int(t) || matches!(t, DataType::Real | DataType::Decimal(..))
}

/// How many digits before the point a type always fits, if bounded.
fn int_digits(t: &DataType) -> Option<u32> {
    match t {
        DataType::SmallInt => Some(4),
        DataType::Int => Some(9),
        DataType::BigInt => Some(18),
        DataType::Decimal(p, s) => Some(p - s),
        _ => None,
    }
}

fn int_range(t: &DataType) -> Option<(i64, i64)> {
    match t {
        DataType::SmallInt => Some((i16::MIN.into(), i16::MAX.into())),
        DataType::Int => Some((i32::MIN.into(), i32::MAX.into())),
        DataType::BigInt => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

/// Maps the names of a plan back to the database's current ones.
struct Names<'a>(&'a [Rename]);

impl Names<'_> {
    fn table<'a>(&'a self, name: &'a str) -> &'a str {
        self.0.iter().find(|r| r.table.is_none() && r.to == name).map_or(name, |r| r.from.as_str())
    }

    fn column<'a>(&'a self, table: &str, name: &'a str) -> &'a str {
        self.0.iter().find(|r| r.table.as_deref() == Some(table) && r.to == name).map_or(name, |r| r.from.as_str())
    }

    /// A check expression of `table`, which names columns as they will be,
    /// with the current names.
    fn expr(&self, table: &str, expr: &str, d: Dialect) -> String {
        let renamed = self.0.iter().filter(|r| r.table.as_deref() == Some(table));
        let renamed: Vec<&Rename> = renamed.collect();
        crate::diff::rename_idents(expr, |word, quoted| {
            let r = renamed.iter().find(|r| if quoted { r.to == word } else { r.to.eq_ignore_ascii_case(word) })?;
            Some(if quoted { r.from.clone() } else { quote(&r.from, d) })
        })
    }
}

/// The checks for the tables `p` alters, against the rows there now.
pub fn row_checks(p: &Plan) -> Vec<RowCheck> {
    let d = p.dialect;
    let names = Names(&p.renames);
    let mut out = Vec::new();
    for name in &p.altered {
        let Some(td) = p.table_diff(name) else { continue };
        let table = names.table(name).to_string();
        let from = quote(&table, d);
        let col = |c: &str| format!("{from}.{}", quote(names.column(name, c), d));
        let existing = |cols: &[String]| cols.iter().all(|c| td.old.column(c).is_some());
        let mut check = |what: String, sql: String| out.push(RowCheck { table: table.clone(), what, sql });
        let count = |cond: String| format!("SELECT count(*) FROM {from} WHERE {cond}");
        let duplicates = |cols: &[String]| {
            let cs: Vec<String> = cols.iter().map(|c| col(c)).collect();
            let set: Vec<String> = cs.iter().map(|c| format!("{c} IS NOT NULL")).collect();
            format!(
                "SELECT coalesce(sum(n), 0) FROM (SELECT count(*) AS n FROM {from} WHERE {} GROUP BY {} HAVING count(*) > 1) sdb_dup",
                set.join(" AND "),
                cs.join(", ")
            )
        };
        let orphans = |fk: &TableForeignKey| {
            let mut conds: Vec<String> = fk.columns.iter().map(|c| format!("{} IS NOT NULL", col(c))).collect();
            // A parent made by this migration has no rows yet.
            if !p.created.contains(&fk.table) {
                let parent = quote(names.table(&fk.table), d);
                let matches: Vec<String> = fk
                    .columns
                    .iter()
                    .zip(&fk.ref_columns)
                    .map(|(c, r)| format!("sdb_parent.{} = {}", quote(names.column(&fk.table, r), d), col(c)))
                    .collect();
                conds.push(format!("NOT EXISTS (SELECT 1 FROM {parent} sdb_parent WHERE {})", matches.join(" AND ")));
            }
            count(conds.join(" AND "))
        };

        for c in &td.added {
            // MySQL gives existing rows the type's zero value instead.
            if !c.nullable && c.default.is_none() && !c.auto_increment && d != Dialect::Mysql {
                check(format!("exist, and the new column {} is NOT NULL without a default", c.name), format!("SELECT count(*) FROM {from}"));
            }
        }
        for (o, n) in &td.altered {
            let c = col(&n.name);
            if o.nullable && !n.nullable {
                check(format!("have NULL in {}, which becomes NOT NULL", n.name), count(format!("{c} IS NULL")));
            }
            if let Some((cond, what)) = type_violation(&o.data_type, &n.data_type, &c, d) {
                check(format!("have {} {what}", n.name), count(cond));
            }
            if n.unique && !o.unique {
                check(format!("repeat a value of {}, which becomes unique", n.name), duplicates(std::slice::from_ref(&n.name)));
            }
            if let Some(expr) = &n.check
                && o.check != n.check
            {
                check(format!("fail the new check on {}: {expr}", n.name), count(format!("NOT ({})", names.expr(name, expr, d))));
            }
            if n.references.is_some()
                && o.references != n.references
                && let Some(fk) = TableForeignKey::of_column(n)
            {
                check(format!("have {} pointing at no row of {}", n.name, fk.table), orphans(&fk));
            }
        }
        for i in td.added_indexes.iter().filter(|i| i.unique && existing(&i.columns)) {
            check(format!("repeat a value of ({}), which becomes unique", i.columns.join(", ")), duplicates(&i.columns));
        }
        let pk = td.new.primary_key();
        if td.pk_changed && !pk.is_empty() && existing(&pk) {
            check(format!("repeat a value of the new primary key ({})", pk.join(", ")), duplicates(&pk));
        }
        for fk in td.added_fks.iter().filter(|fk| existing(&fk.columns)) {
            check(format!("have ({}) pointing at no row of {}", fk.columns.join(", "), fk.table), orphans(fk));
        }
    }
    out
}

/// The condition on column `c` (SQL) that a value fails the change from type
/// `o` to `n`, and what it says about those rows.
fn type_violation(o: &DataType, n: &DataType, c: &str, d: Dialect) -> Option<(String, String)> {
    use DataType::*;
    type_risk(o, n, d)?;
    let text = if d == Dialect::Mysql { "CHAR" } else { "TEXT" };
    match n {
        Enum(values) => Some((
            format!("CAST({c} AS {text}) NOT IN ({})", values.iter().map(|v| string_sql(v, d)).collect::<Vec<_>>().join(", ")),
            format!("not in ({})", values.join(", ")),
        )),
        _ if d == Dialect::Sqlite => None,
        Varchar(b) => Some((format!("char_length(CAST({c} AS {text})) > {b}"), format!("longer than {b} characters"))),
        SmallInt | Int | BigInt if is_numeric(o) => {
            let (lo, hi) = int_range(n)?;
            Some((format!("({c} < {lo} OR {c} > {hi})"), format!("outside the {n} range")))
        }
        Decimal(p, s) if is_numeric(o) => {
            (int_digits(o).is_none_or(|x| x > p - s)).then(|| (format!("abs({c}) >= 1{}", "0".repeat((p - s) as usize)), format!("too large for {n}")))
        }
        // MySQL (strict) refuses text that isn't a number, or is one out of range.
        _ if d == Dialect::Mysql && matches!(o, Text | Varchar(_) | Custom(_)) && (is_numeric(n) || *n == Bool) => {
            let digits = "[0-9]+([.][0-9]*)?|[.][0-9]+";
            let pattern = if is_numeric(n) && !is_int(n) { format!("({digits})([eE][-+]?[0-9]+)?") } else { "[0-9]+".into() };
            let mut cond = format!("NOT ({c} REGEXP '^[[:space:]]*[-+]?({pattern})[[:space:]]*$')");
            let number = format!("CAST({c} AS DECIMAL(65, 30))");
            match n {
                Bool => cond.push_str(&format!(" OR {number} NOT BETWEEN -128 AND 127")),
                Decimal(p, s) => cond.push_str(&format!(" OR abs({number}) >= 1{}", "0".repeat((p - s) as usize))),
                _ => {
                    if let Some((lo, hi)) = int_range(n) {
                        cond.push_str(&format!(" OR {number} NOT BETWEEN {lo} AND {hi}"));
                    }
                }
            }
            Some((format!("({cond})"), format!("that isn't a number that fits {n}")))
        }
        // Postgres 16+ can tell whether text converts; older servers fail the check, which is skipped.
        _ if d == Dialect::Postgres && matches!(o, Text | Varchar(_) | Custom(_)) && !matches!(n, Custom(_)) => Some((
            format!("NOT pg_input_is_valid(CAST({c} AS TEXT), {})", string_sql(&type_sql(n, d).to_lowercase(), d)),
            format!("that doesn't convert to {n}"),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use DataType::*;

    #[test]
    fn type_risks() {
        let pg = Dialect::Postgres;
        for (o, n) in [
            (Int, BigInt),
            (Varchar(10), Varchar(20)),
            (Varchar(10), Text),
            (Int, Text),
            (Date, Timestamp),
            (Int, Decimal(12, 2)),
            (Decimal(5, 2), Decimal(8, 3)),
            (Int, Real),
            (Enum(vec!["a".into()]), Enum(vec!["a".into(), "b".into()])),
            (Enum(vec!["a".into()]), Varchar(5)),
        ] {
            assert_eq!(type_risk(&o, &n, pg), None, "{o} -> {n}");
        }
        for (o, n, why) in [
            (Text, Varchar(10), "values longer than 10 characters fail"),
            (BigInt, Int, "values outside the int range fail"),
            (Real, Int, "fractions are rounded, and values outside the int range fail"),
            (Int, Decimal(6, 2), "values with more than 4 digit(s) before the point fail"),
            (Decimal(8, 3), Decimal(8, 2), "values are rounded to 2 decimal place(s)"),
            (Timestamp, Date, "the time of day is dropped"),
            (Text, Int, "values that don't convert to int fail"),
            (Enum(vec!["a".into(), "b".into()]), Enum(vec!["a".into()]), "rows with 'b' fail"),
            (Text, Enum(vec!["a".into()]), "values not in the list fail"),
        ] {
            assert_eq!(type_risk(&o, &n, pg).as_deref(), Some(why), "{o} -> {n}");
        }
        // SQLite doesn't enforce declared types, but enums are a check.
        assert_eq!(type_risk(&Text, &Int, Dialect::Sqlite), None);
        assert!(type_risk(&Text, &Enum(vec!["a".into()]), Dialect::Sqlite).is_some());
    }

    #[test]
    fn checks_use_the_current_names() {
        let old = parse("table p { id int pk }\ntable t { id int pk\n a text\n b int\n c int }").unwrap();
        let new = parse(
            "table parents was p { id int pk }\ntable things was t { id int pk\n a varchar(5) not null\n bee int unique was b check (bee > 0)\n \
             c int -> parents.id check (c > 0) }",
        )
        .unwrap();
        let p = crate::diff::plan(&old, &new, Dialect::Postgres);
        let checks: Vec<(String, String)> = row_checks(&p).into_iter().map(|c| (c.what, c.sql)).collect();
        assert_eq!(
            checks,
            [
                ("have NULL in a, which becomes NOT NULL".into(), "SELECT count(*) FROM t WHERE t.a IS NULL".into()),
                ("have a longer than 5 characters".into(), "SELECT count(*) FROM t WHERE char_length(CAST(t.a AS TEXT)) > 5".into()),
                (
                    "repeat a value of bee, which becomes unique".into(),
                    "SELECT coalesce(sum(n), 0) FROM (SELECT count(*) AS n FROM t WHERE t.b IS NOT NULL GROUP BY t.b HAVING count(*) > 1) sdb_dup".into()
                ),
                ("fail the new check on bee: bee > 0".into(), "SELECT count(*) FROM t WHERE NOT (b > 0)".into()),
                ("fail the new check on c: c > 0".into(), "SELECT count(*) FROM t WHERE NOT (c > 0)".into()),
                (
                    "have c pointing at no row of parents".into(),
                    "SELECT count(*) FROM t WHERE t.c IS NOT NULL AND NOT EXISTS (SELECT 1 FROM p sdb_parent WHERE sdb_parent.id = t.c)".into()
                ),
            ]
        );
        assert!(row_checks(&p).iter().all(|c| c.table == "t"));
    }

    #[test]
    fn mysql_checks_that_text_is_a_number() {
        let my = Dialect::Mysql;
        let (cond, what) = type_violation(&Text, &Int, "t.a", my).unwrap();
        assert_eq!(what, "that isn't a number that fits int");
        assert_eq!(
            cond,
            "(NOT (t.a REGEXP '^[[:space:]]*[-+]?([0-9]+)[[:space:]]*$') OR CAST(t.a AS DECIMAL(65, 30)) NOT BETWEEN -2147483648 AND 2147483647)"
        );
        let (cond, _) = type_violation(&Varchar(9), &Decimal(6, 2), "t.a", my).unwrap();
        assert!(cond.contains("[eE]") && cond.ends_with(" OR abs(CAST(t.a AS DECIMAL(65, 30))) >= 10000)"), "{cond}");
        assert!(type_violation(&Text, &Real, "t.a", my).is_some());
        assert!(type_violation(&Text, &Date, "t.a", my).is_none());
    }
}
