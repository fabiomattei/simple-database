//! Fill a database with realistic fake data.
//!
//! Values are chosen from the column's type, its name (`email`, `price`,
//! `created_at`, ...) and simple check constraints (`x in (...)`, `x >= 0`).
//! Foreign keys point at existing parent rows, tables are filled parents
//! first, and rows rejected by a constraint are regenerated.

use crate::db::{Cell as Value, Database};
use crate::ddl::{Dialect, quote};
use crate::output::Paint;
use crate::schema::*;
use anyhow::Result;
use std::collections::HashMap;

pub struct SeedOptions {
    /// Rows per table unless overridden in `counts`.
    pub rows: usize,
    pub counts: HashMap<String, usize>,
    /// Only seed these tables (empty = all).
    pub only: Vec<String>,
    pub seed: u64,
}

pub struct TableReport {
    pub table: String,
    pub requested: usize,
    pub inserted: usize,
    pub note: Option<String>,
}

pub fn random_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) % 1_000_000
}

pub fn print_reports(reports: &[TableReport]) {
    if reports.is_empty() {
        println!("(no tables)");
    }
    for r in reports {
        let note = r.note.as_deref().map_or(String::new(), |n| format!("  {}", Paint::Yellow.out(n)));
        println!("{:<30} {:>6} / {:<6}{note}", r.table, r.inserted, r.requested);
    }
}

/// Give up on a table after this many inserts in a row are rejected.
const MAX_CONSECUTIVE_FAILURES: usize = 50;

/// Rows per INSERT.
const BATCH: usize = 100;

pub fn seed(db: &mut Database, opts: &SeedOptions) -> Result<Vec<TableReport>> {
    db.ensure_writable()?;
    let schema = db.introspect()?;
    for name in opts.only.iter().chain(opts.counts.keys()) {
        anyhow::ensure!(schema.table(name).is_some(), "no such table: {name}");
    }

    db.execute("BEGIN")?;
    let result = seed_tables(db, &schema, opts);
    db.execute(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    result
}

fn seed_tables(db: &mut Database, schema: &Schema, opts: &SeedOptions) -> Result<Vec<TableReport>> {
    let mut g = Generator {
        rng: Rng(opts.seed),
        counter: 0,
        person: (FIRST[0], LAST[0]),
    };
    let mut reports = Vec::new();
    for t in schema.tables_in_dependency_order() {
        if !opts.only.is_empty() && !opts.only.contains(&t.name) {
            continue;
        }
        let requested = opts.counts.get(&t.name).copied().unwrap_or(opts.rows);
        reports.push(seed_table(db, t, requested, &mut g)?);
    }
    Ok(reports)
}

fn seed_table(db: &mut Database, t: &Table, requested: usize, g: &mut Generator) -> Result<TableReport> {
    let d = db.dialect();
    let mut report = TableReport {
        table: t.name.clone(),
        requested,
        inserted: 0,
        note: None,
    };

    let columns: Vec<&Column> = t.columns.iter().filter(|c| !c.auto_increment).collect();
    let insert = |rows: &[Vec<Value>]| {
        if columns.is_empty() {
            return match d {
                Dialect::Mysql => format!("INSERT INTO {} () VALUES ()", quote(&t.name, d)),
                _ => format!("INSERT INTO {} DEFAULT VALUES", quote(&t.name, d)),
            };
        }
        let rows: Vec<String> = rows
            .iter()
            .map(|values| format!("({})", values.iter().zip(&columns).map(|(v, c)| literal(v, &c.data_type, d)).collect::<Vec<_>>().join(", ")))
            .collect();
        format!(
            "INSERT INTO {} ({}) VALUES {}",
            quote(&t.name, d),
            columns.iter().map(|c| quote(&c.name, d)).collect::<Vec<_>>().join(", "),
            rows.join(", ")
        )
    };

    // Candidate values for each foreign key column, from the parent table.
    let mut parents: HashMap<String, Vec<Value>> = HashMap::new();
    for c in &columns {
        let Some(fk) = &c.references else { continue };
        let values = existing_values(db, &fk.table, &fk.column)?;
        if values.is_empty() && !c.nullable && fk.table != t.name {
            report.note = Some(format!("skipped: referenced table '{}' has no rows", fk.table));
            return Ok(report);
        }
        parents.insert(c.name.clone(), values);
    }
    // Whole parent rows for each multi-column foreign key.
    let mut parent_rows: Vec<(&TableForeignKey, Vec<Vec<Value>>)> = Vec::new();
    for fk in &t.foreign_keys {
        let rows = existing_rows(db, &fk.table, &fk.ref_columns)?;
        let nullable = fk.columns.iter().any(|c| t.column(c).is_some_and(|c| c.nullable));
        if rows.is_empty() && !nullable && fk.table != t.name {
            report.note = Some(format!("skipped: referenced table '{}' has no rows", fk.table));
            return Ok(report);
        }
        parent_rows.push((fk, rows));
    }

    // Single-column integer keys without autoincrement continue from the current max.
    let pk = t.primary_key();
    let mut next_int_key: i64 = match pk.as_slice() {
        [only] if t.column(only).is_some_and(|c| c.data_type.is_integer() && !c.auto_increment) => {
            let sql = format!("SELECT COALESCE(MAX({}), 0) + 1 FROM {}", quote(only, d), quote(&t.name, d));
            match db.query_value(&sql)? {
                Value::Int(n) => n,
                other => other.display().parse().unwrap_or(1),
            }
        }
        _ => 1,
    };
    let self_refs: Vec<&Column> = columns
        .iter()
        .copied()
        .filter(|c| c.references.as_ref().is_some_and(|fk| fk.table == t.name))
        .collect();

    let row = |g: &mut Generator, parents: &HashMap<String, Vec<Value>>, key: i64| -> Vec<Value> {
        g.person = (g.rng.pick(FIRST), g.rng.pick(LAST));
        let mut chosen: HashMap<&str, Value> = HashMap::new();
        for (fk, rows) in &parent_rows {
            let row = if rows.is_empty() { None } else { Some(g.rng.pick(rows)) };
            for (i, c) in fk.columns.iter().enumerate() {
                chosen.insert(c, row.map_or(Value::Null, |r| r[i].clone()));
            }
        }
        columns
            .iter()
            .map(|c| {
                if let Some(v) = chosen.get(c.name.as_str()) {
                    return v.clone();
                }
                if let Some(candidates) = parents.get(&c.name) {
                    if candidates.is_empty() || (c.nullable && g.rng.chance(0.1)) {
                        return Value::Null;
                    }
                    return g.rng.pick(candidates).clone();
                }
                if pk.len() == 1 && pk[0] == c.name && c.data_type.is_integer() {
                    return Value::Int(key);
                }
                g.value(t, c, pk.contains(&c.name))
            })
            .collect()
    };
    // Rows go in `BATCH` at a time. Self-referencing rows point at the rows
    // inserted just before them, and DEFAULT VALUES takes one row: those one by one.
    let batch = if self_refs.is_empty() && !columns.is_empty() { BATCH } else { 1 };

    let mut failures = 0;
    'fill: while report.inserted < requested {
        let n = batch.min(requested - report.inserted);
        let rows: Vec<Vec<Value>> = (0..n).map(|i| row(g, &parents, next_int_key + i as i64)).collect();
        next_int_key += n as i64;
        anyhow::ensure!(!db.cancel_requested(), "cancelled");
        if n > 1 {
            match try_insert(db, &insert(&rows)) {
                Ok(()) => {
                    report.inserted += n;
                    failures = 0;
                    continue;
                }
                Err(e) if db.cancel_requested() => return Err(e.context("cancelled")),
                // A constraint rejected one of them: keep the others.
                Err(_) => {}
            }
        }
        for values in &rows {
            match try_insert(db, &insert(std::slice::from_ref(values))) {
                Ok(()) => {
                    report.inserted += 1;
                    failures = 0;
                    // Later rows may reference rows inserted earlier in this run.
                    for c in &self_refs {
                        let fk = c.references.as_ref().unwrap();
                        let v = match columns.iter().position(|col| col.name == fk.column) {
                            Some(i) => values[i].clone(),
                            None => db.last_insert_id()?,
                        };
                        parents.entry(c.name.clone()).or_default().push(v);
                    }
                }
                Err(e) if db.cancel_requested() => return Err(e.context("cancelled")),
                Err(e) => {
                    failures += 1;
                    if failures >= MAX_CONSECUTIVE_FAILURES {
                        report.note = Some(format!("stopped early: {e}"));
                        break 'fill;
                    }
                }
            }
        }
    }
    Ok(report)
}

/// Run one INSERT. A failed statement aborts a whole Postgres transaction
/// unless it ran inside a savepoint.
fn try_insert(db: &mut Database, sql: &str) -> Result<()> {
    match db.dialect() {
        Dialect::Postgres => db.execute(&format!("SAVEPOINT sdb_row; {sql}; RELEASE SAVEPOINT sdb_row")).inspect_err(|_| {
            let _ = db.execute("ROLLBACK TO SAVEPOINT sdb_row; RELEASE SAVEPOINT sdb_row");
        }),
        _ => db.execute(sql),
    }
}

fn existing_values(db: &mut Database, table: &str, column: &str) -> Result<Vec<Value>> {
    let rows = existing_rows(db, table, &[column.to_string()])?;
    Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).collect())
}

/// Distinct values of `columns` in `table`, without NULLs.
fn existing_rows(db: &mut Database, table: &str, columns: &[String]) -> Result<Vec<Vec<Value>>> {
    let d = db.dialect();
    let cols: Vec<String> = columns.iter().map(|c| quote(c, d)).collect();
    let sql = format!(
        "SELECT DISTINCT {} FROM {} WHERE {} LIMIT 10000",
        cols.join(", "),
        quote(table, d),
        cols.iter().map(|c| format!("{c} IS NOT NULL")).collect::<Vec<_>>().join(" AND ")
    );
    let mut results = db.run(&sql)?;
    Ok(results.pop().map_or(vec![], |r| r.rows))
}

/// `v` as a SQL literal for a column of type `ty`.
fn literal(v: &Value, ty: &DataType, d: Dialect) -> String {
    let boolean = |b: bool| match d {
        Dialect::Postgres => if b { "TRUE" } else { "FALSE" }.to_string(),
        _ => if b { "1" } else { "0" }.to_string(),
    };
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => boolean(*b),
        Value::Int(i) if *ty == DataType::Bool => boolean(*i != 0),
        Value::Int(i) => i.to_string(),
        Value::Real(f) if f.is_finite() => f.to_string(),
        Value::Real(_) => "NULL".into(),
        Value::Text(s) => {
            let s = s.replace('\'', "''");
            // MySQL treats backslashes in strings as escapes by default.
            if d == Dialect::Mysql { format!("'{}'", s.replace('\\', "\\\\")) } else { format!("'{s}'") }
        }
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            match d {
                Dialect::Postgres => format!("'\\x{hex}'::bytea"),
                _ => format!("X'{hex}'"),
            }
        }
    }
}

// ---------------------------------------------------------------- randomness

/// SplitMix64: tiny, fast and good enough for fake data. Seedable so runs are reproducible.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `lo..=hi`.
    pub(crate) fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo + 1) as u64) as i64
    }

    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(crate) fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    pub(crate) fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.range(0, xs.len() as i64 - 1) as usize]
    }
}

// ---------------------------------------------------------------- generation

const FIRST: &[&str] = &[
    "Ada", "Alan", "Grace", "Linus", "Margaret", "Dennis", "Barbara", "Ken", "Frances", "Edsger",
    "Radia", "Guido", "Katherine", "Bjarne", "Hedy", "Donald", "Sophie", "Tim", "Annie", "John",
    "Mary", "Carlos", "Yuki", "Priya", "Fatima", "Lars", "Chiara", "Mateo", "Amara", "Wei",
];
const LAST: &[&str] = &[
    "Lovelace", "Turing", "Hopper", "Torvalds", "Hamilton", "Ritchie", "Liskov", "Thompson",
    "Allen", "Dijkstra", "Perlman", "Rossum", "Johnson", "Stroustrup", "Lamarr", "Knuth", "Wilson",
    "Berners-Lee", "Easley", "Backus", "Rossi", "Garcia", "Tanaka", "Sharma", "Haddad", "Nilsson",
    "Bianchi", "Fernandez", "Okafor", "Zhang",
];
const WORDS: &[&str] = &[
    "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do",
    "eiusmod", "tempor", "incididunt", "labore", "dolore", "magna", "aliqua", "enim", "minim",
    "veniam", "quis", "nostrud", "exercitation", "ullamco", "laboris", "nisi", "aliquip", "commodo",
    "consequat", "duis", "aute", "irure", "voluptate", "velit", "esse", "cillum", "fugiat", "nulla",
];
const CITIES: &[&str] = &[
    "Rome", "Milan", "Berlin", "Paris", "Lisbon", "Tokyo", "Toronto", "Austin", "Nairobi", "Seoul",
    "Oslo", "Lima", "Sydney", "Mumbai", "Dublin",
];
const COUNTRIES: &[&str] = &[
    "Italy", "Germany", "France", "Portugal", "Japan", "Canada", "United States", "Kenya",
    "South Korea", "Norway", "Peru", "Australia", "India", "Ireland",
];
const DOMAINS: &[&str] = &["example.com", "example.org", "mail.test", "acme.dev"];
const PERSON_TABLES: &[&str] = &[
    "user", "person", "people", "customer", "author", "employee", "member", "contact", "client",
    "student", "teacher", "patient", "account",
];

struct Generator {
    rng: Rng,
    /// Incremented per value; mixed into unique values to avoid collisions.
    counter: u64,
    /// The person this row describes, so name, email and username agree.
    person: (&'static str, &'static str),
}

/// What a column's CHECK constraint tells us about acceptable values.
#[derive(Default)]
struct CheckHint {
    choices: Vec<Value>,
    min: Option<f64>,
    max: Option<f64>,
}

fn check_hint(c: &Column) -> CheckHint {
    let mut hint = CheckHint::default();
    let Some(expr) = &c.check else { return hint };
    let lower = expr.to_ascii_lowercase();

    // `x in ('a', 'b')`, or Postgres' `x = ANY (ARRAY['a', 'b'])`.
    let list = match (lower.find(" in ("), lower.find("array[")) {
        (Some(pos), _) => lower[pos..].find('(').map(|o| (pos + o, ')')),
        (None, Some(pos)) => Some((pos + "array".len(), ']')),
        _ => None,
    };
    if let Some((open, closer)) = list
        && let Some(close) = lower[open..].find(closer).map(|c| open + c)
    {
        for item in expr[open + 1..close].split(',') {
            let item = item.trim();
            let item = match item.rfind("::") {
                Some(p) if !item[p..].contains('\'') => &item[..p],
                _ => item,
            };
            if let Some(s) = item.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
                hint.choices.push(Value::Text(s.replace("''", "'")));
            } else if let Ok(i) = item.parse::<i64>() {
                hint.choices.push(Value::Int(i));
            } else if let Ok(f) = item.parse::<f64>() {
                hint.choices.push(Value::Real(f));
            }
        }
    }

    let number_after = |op: &str| -> Option<f64> {
        let pos = lower.find(op)? + op.len();
        let rest = lower[pos..].trim_start();
        let end = rest
            .find(|ch: char| !(ch.is_ascii_digit() || ch == '.' || ch == '-'))
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    };
    hint.min = number_after(">=").or_else(|| number_after(">").map(|n| n + 1.0));
    hint.max = number_after("<=").or_else(|| number_after("<").map(|n| n - 1.0));
    if let Some(pos) = lower.find(" between ") {
        let parts: Vec<&str> = lower[pos + 9..].split(" and ").collect();
        if parts.len() >= 2 {
            hint.min = parts[0].trim().parse().ok();
            hint.max = parts[1].split_whitespace().next().and_then(|s| s.parse().ok());
        }
    }
    hint
}

impl Generator {
    fn value(&mut self, t: &Table, c: &Column, part_of_pk: bool) -> Value {
        self.counter += 1;
        let hint = check_hint(c);
        if !hint.choices.is_empty() {
            return self.rng.pick(&hint.choices).clone();
        }
        if let DataType::Enum(values) = &c.data_type
            && !values.is_empty()
        {
            if c.nullable && c.default.is_none() && self.rng.chance(0.1) {
                return Value::Null;
            }
            return Value::Text(self.rng.pick(values).clone());
        }
        let must_be_unique = c.unique
            || part_of_pk
            || t.indexes.iter().any(|i| i.unique && i.columns.contains(&c.name));
        if c.nullable && !must_be_unique && c.default.is_none() && self.rng.chance(0.1) {
            return Value::Null;
        }

        let words: Vec<String> = c.name.to_ascii_lowercase().split('_').map(String::from).collect();
        let has = |w: &[&str]| words.iter().any(|x| w.contains(&x.as_str()));

        match &c.data_type {
            DataType::Int | DataType::BigInt | DataType::SmallInt => {
                let (lo, hi) = if has(&["age"]) {
                    (18, 90)
                } else if has(&["year"]) {
                    (1990, 2026)
                } else if has(&["rating", "stars"]) {
                    (1, 5)
                } else if has(&["score", "percent", "percentage", "progress"]) {
                    (0, 100)
                } else if has(&["count", "views", "likes", "quantity", "qty", "stock", "votes"]) {
                    (0, 1000)
                } else if has(&["price", "amount", "cost", "total", "balance", "salary"]) {
                    (1, 10_000)
                } else {
                    (1, 1000)
                };
                let lo = hint.min.map_or(lo, |m| m as i64);
                let hi = hint.max.map_or(hi.max(lo), |m| m as i64);
                let mut n = self.rng.range(lo, hi);
                if must_be_unique && hint.max.is_none() {
                    n = lo + self.counter as i64;
                }
                if c.data_type == DataType::SmallInt {
                    n = n.clamp(-32768, 32767);
                }
                Value::Int(n)
            }
            DataType::Real | DataType::Decimal(..) => {
                let (lo, hi) = if has(&["lat", "latitude"]) {
                    (-90.0, 90.0)
                } else if has(&["lng", "lon", "longitude"]) {
                    (-180.0, 180.0)
                } else if has(&["rating"]) {
                    (1.0, 5.0)
                } else {
                    (0.0, 1000.0)
                };
                let lo = hint.min.unwrap_or(lo);
                let mut hi = hint.max.unwrap_or(hi);
                let mut scale = 2;
                if let DataType::Decimal(p, s) = c.data_type {
                    scale = s as i32;
                    hi = hi.min(10f64.powi(p as i32 - s as i32) - 1.0);
                }
                let x = lo + self.rng.f64() * (hi - lo);
                let f = 10f64.powi(scale);
                Value::Real((x * f).round() / f)
            }
            DataType::Bool => Value::Bool(self.rng.range(0, 1) == 1),
            DataType::Date => Value::Text(self.date()),
            DataType::Time => Value::Text(self.time()),
            DataType::Timestamp => Value::Text(format!("{} {}", self.date(), self.time())),
            DataType::Uuid => Value::Text(self.uuid()),
            DataType::Json => Value::Text(format!(
                "{{\"{}\": {}, \"tags\": [\"{}\", \"{}\"]}}",
                self.rng.pick(WORDS),
                self.rng.range(1, 100),
                self.rng.pick(WORDS),
                self.rng.pick(WORDS)
            )),
            DataType::Blob => Value::Blob((0..16).map(|_| self.rng.next_u64() as u8).collect()),
            DataType::Text | DataType::Varchar(_) | DataType::Custom(_) | DataType::Enum(_) => {
                let mut s = self.text(t, &words);
                if must_be_unique && !has(&["email", "username", "login", "handle", "slug"]) {
                    s = format!("{s} {}", self.counter);
                }
                if let DataType::Varchar(n) = c.data_type {
                    s = s.chars().take(n as usize).collect();
                }
                Value::Text(s)
            }
        }
    }

    fn text(&mut self, t: &Table, words: &[String]) -> String {
        let has = |w: &[&str]| words.iter().any(|x| w.contains(&x.as_str()));
        let (first, last) = self.person;
        let n = self.counter;

        if has(&["email", "mail"]) {
            let domain = self.rng.pick(DOMAINS);
            return format!("{}.{}{n}@{domain}", first.to_lowercase(), last.to_lowercase().replace('-', ""));
        }
        if has(&["username", "login", "handle", "nickname"]) {
            return format!("{}{}{n}", first.to_lowercase(), last[..1].to_lowercase());
        }
        if words.iter().any(|w| w == "first" || w == "firstname" || w == "given") {
            return first.into();
        }
        if words.iter().any(|w| w == "last" || w == "lastname" || w == "surname" || w == "family") {
            return last.into();
        }
        if has(&["name", "fullname"]) {
            let table = t.name.to_ascii_lowercase();
            return if PERSON_TABLES.iter().any(|p| table.contains(p)) {
                format!("{first} {last}")
            } else {
                capitalize(&self.words(1, 2))
            };
        }
        if has(&["title", "subject", "headline", "caption", "label"]) {
            return capitalize(&self.words(3, 6));
        }
        if has(&["body", "content", "description", "bio", "text", "comment", "message", "notes", "note", "summary", "about"]) {
            let sentences = self.rng.range(1, 3);
            return (0..sentences)
                .map(|_| format!("{}.", capitalize(&self.words(6, 14))))
                .collect::<Vec<_>>()
                .join(" ");
        }
        if has(&["url", "website", "link", "homepage"]) {
            return format!("https://{}/{}", self.rng.pick(DOMAINS), self.rng.pick(WORDS));
        }
        if has(&["avatar", "image", "photo", "picture"]) {
            return format!("https://{}/img/{n}.png", self.rng.pick(DOMAINS));
        }
        if has(&["phone", "mobile", "tel"]) {
            return format!("+1-555-{:03}-{:04}", self.rng.range(100, 999), self.rng.range(0, 9999));
        }
        if has(&["city", "town"]) {
            return (*self.rng.pick(CITIES)).into();
        }
        if has(&["country"]) {
            return (*self.rng.pick(COUNTRIES)).into();
        }
        if has(&["address", "street"]) {
            return format!("{} {} Street", self.rng.range(1, 999), capitalize(self.rng.pick(WORDS)));
        }
        if has(&["zip", "postal", "postcode"]) {
            return format!("{:05}", self.rng.range(0, 99999));
        }
        if has(&["slug"]) {
            return format!("{}-{n}", self.words(2, 3).replace(' ', "-"));
        }
        if has(&["password", "hash", "token", "secret", "salt"]) {
            return format!("{:016x}{:016x}", self.rng.next_u64(), self.rng.next_u64());
        }
        if has(&["color", "colour"]) {
            return format!("#{:06x}", self.rng.next_u64() & 0xFF_FFFF);
        }
        if has(&["currency"]) {
            return (*self.rng.pick(&["USD", "EUR", "GBP", "JPY"])).into();
        }
        if has(&["status", "state"]) {
            return (*self.rng.pick(&["active", "inactive", "pending"])).into();
        }
        if has(&["ip"]) {
            return format!(
                "{}.{}.{}.{}",
                self.rng.range(1, 254),
                self.rng.range(0, 255),
                self.rng.range(0, 255),
                self.rng.range(1, 254)
            );
        }
        if has(&["code", "sku", "ref", "reference"]) {
            return format!("{}-{:05}", self.rng.pick(WORDS).chars().take(3).collect::<String>().to_uppercase(), self.rng.range(0, 99999));
        }
        self.words(1, 3)
    }

    fn words(&mut self, min: i64, max: i64) -> String {
        let n = self.rng.range(min, max);
        (0..n).map(|_| *self.rng.pick(WORDS)).collect::<Vec<_>>().join(" ")
    }

    fn date(&mut self) -> String {
        format!(
            "{}-{:02}-{:02}",
            self.rng.range(2020, 2026),
            self.rng.range(1, 12),
            self.rng.range(1, 28)
        )
    }

    fn time(&mut self) -> String {
        format!(
            "{:02}:{:02}:{:02}",
            self.rng.range(0, 23),
            self.rng.range(0, 59),
            self.rng.range(0, 59)
        )
    }

    fn uuid(&mut self) -> String {
        let a = self.rng.next_u64();
        let b = self.rng.next_u64();
        // Version 4, variant 1.
        format!(
            "{:08x}-{:04x}-4{:03x}-{:x}{:03x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xFFFF,
            a & 0xFFF,
            8 | (b >> 62),
            (b >> 48) & 0xFFF,
            b & 0xFFFF_FFFF_FFFF
        )
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::schema_sql;
    use crate::parser::parse;

    const SRC: &str = "
        table users {
          id     int pk autoincrement
          email  varchar(255) unique not null
          name   text not null
          role   text not null check (role in ('author', 'admin'))
          age    int check (age >= 18)
          boss_id int -> users.id
        }
        table posts {
          id        int pk autoincrement
          author_id int not null -> users.id on delete cascade
          title     varchar(20) not null
          price     decimal(6,2)
        }
        table tags { id int pk\n name text unique not null }
        table post_tags {
          post_id int not null -> posts.id
          tag_id  int not null -> tags.id
          pk (post_id, tag_id)
        }";

    fn setup() -> Database {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(&schema_sql(&parse(SRC).unwrap(), Dialect::Sqlite)).unwrap();
        Database::from_sqlite(conn, "test")
    }

    fn opts(rows: usize, seed: u64) -> SeedOptions {
        SeedOptions {
            rows,
            counts: HashMap::new(),
            only: vec![],
            seed,
        }
    }

    fn count(db: &mut Database, sql: &str) -> i64 {
        match db.query_value(sql).unwrap() {
            Value::Int(n) => n,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fills_all_tables_respecting_constraints() {
        let mut db = setup();
        let reports = seed(&mut db, &opts(25, 7)).unwrap();
        for r in &reports {
            assert_eq!(r.inserted, 25, "{}: {:?}", r.table, r.note);
        }
        // Parents seeded before children, so every FK resolves.
        assert_eq!(count(&mut db, "SELECT count(*) FROM pragma_foreign_key_check"), 0);
        assert_eq!(count(&mut db, "SELECT count(*) FROM users WHERE role NOT IN ('author','admin')"), 0);
        assert_eq!(count(&mut db, "SELECT count(*) FROM users WHERE age < 18"), 0);
        assert_eq!(count(&mut db, "SELECT count(*) FROM users WHERE email NOT LIKE '%@%'"), 0);
        assert!(count(&mut db, "SELECT max(length(title)) FROM posts") <= 20);
        assert!(count(&mut db, "SELECT count(*) FROM users WHERE boss_id IS NOT NULL") > 0);
    }

    #[test]
    fn multi_column_keys_get_whole_parent_rows() {
        let src = "table pairs { a int not null\n b int not null\n mood enum('ok', 'it''s') not null\n pk (a, b) }\n\
                   table links { id int pk\n x int not null\n y int not null\n fk (x, y) -> pairs (a, b) }";
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(&schema_sql(&parse(src).unwrap(), Dialect::Sqlite)).unwrap();
        let mut db = Database::from_sqlite(conn, "test");
        for r in seed(&mut db, &opts(20, 3)).unwrap() {
            assert_eq!(r.inserted, 20, "{}: {:?}", r.table, r.note);
        }
        assert_eq!(count(&mut db, "SELECT count(*) FROM links JOIN pairs ON a = x AND b = y"), 20);
        assert_eq!(count(&mut db, "SELECT count(DISTINCT mood) FROM pairs"), 2);
    }

    #[test]
    fn same_seed_is_reproducible() {
        let dump = |seed| {
            let mut db = setup();
            super::seed(&mut db, &opts(5, seed)).unwrap();
            db.query_value("SELECT group_concat(email || name) FROM users").unwrap()
        };
        assert_eq!(dump(1), dump(1));
        assert_ne!(dump(1), dump(2));
    }

    #[test]
    fn stops_when_constraints_cannot_be_met() {
        let mut db = setup();
        let mut o = opts(2, 1);
        o.only = vec!["users".into(), "posts".into(), "tags".into(), "post_tags".into()];
        seed(&mut db, &o).unwrap();
        // Only 2 posts × 2 tags = 4 distinct pairs exist.
        let mut o = opts(10, 2);
        o.only = vec!["post_tags".into()];
        let r = seed(&mut db, &o).unwrap();
        assert!(r[0].inserted <= 2, "{}", r[0].inserted);
        assert!(r[0].note.as_deref().unwrap_or("").contains("stopped early"));
    }

    #[test]
    fn a_rejected_batch_keeps_the_rows_that_fit() {
        let mut db = setup();
        let mut o = opts(15, 1);
        o.only = vec!["users".into(), "posts".into(), "tags".into()];
        seed(&mut db, &o).unwrap();
        // 225 pairs: a batch of 150 random ones repeats some, so it goes in row by row.
        let mut o = opts(150, 4);
        o.only = vec!["post_tags".into()];
        let r = seed(&mut db, &o).unwrap();
        assert_eq!((r[0].inserted, r[0].note.as_deref()), (150, None));
        assert_eq!(db.query_value("SELECT count(*) FROM post_tags").unwrap(), Value::Int(150));
    }

    #[test]
    fn skips_children_of_empty_parents() {
        let mut db = setup();
        let mut o = opts(3, 1);
        o.only = vec!["posts".into()];
        let r = seed(&mut db, &o).unwrap();
        assert_eq!(r[0].inserted, 0);
        assert!(r[0].note.as_deref().unwrap().contains("'users' has no rows"));
    }
}
