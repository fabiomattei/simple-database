//! File menu actions on the open database: dump, restore, running SQL files
//! and checking foreign keys.

use crate::{App, Dialog, IntegrityState, MAX_QUERY_ROWS, MAX_SCRIPT_RESULTS, RestoreState, ScriptOutcome, ScriptState};
use simple_database::db;
use simple_database::dump;
use simple_database::integrity;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

impl App {
    /// Look for rows whose foreign key points at a missing parent row, showing
    /// the outcome in the Integrity dialog.
    pub(crate) fn check_integrity(&mut self) {
        let Some(db) = &self.db else { return };
        let (label, schema) = (db.label.clone(), db.schema.clone());
        self.dialog = Some(Dialog::Integrity(Box::new(IntegrityState { db: label.clone(), report: None })));
        self.db_run(
            "Checking foreign keys",
            move |conn| integrity::check_foreign_keys(conn, &schema, 0).map_err(|e| format!("{e:#}")),
            move |app, res| {
                if let Some(Dialog::Integrity(st)) = &mut app.dialog
                    && st.db == label
                {
                    st.report = Some(res);
                }
            },
        );
    }

    /// Open `table` in the Data tab showing only the rows matching `condition`.
    pub(crate) fn open_table_where(&mut self, table: String, condition: String) {
        self.open_table_data(table.clone());
        if !self.data.current_mut().is_some_and(|v| v.filter_where(condition)) {
            self.set_status(format!("{table} has unsaved changes; save or discard them to filter it"), true);
        }
    }

    /// Save the open database as a SQL dump in its own dialect.
    pub(crate) fn dump_db(&mut self) {
        let Some(db) = &self.db else { return };
        let stem = Path::new(db.label.rsplit(['/', '\\']).next().unwrap_or("dump"))
            .file_stem()
            .map(|s| s.to_string_lossy().trim_start_matches('@').to_string())
            .unwrap_or_else(|| "dump".into());
        let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).set_file_name(format!("{stem}.sql")).save_file() else {
            return;
        };
        self.db_run(
            "Dumping the database",
            move |conn| {
                let f = std::fs::File::create(&path)?;
                let mut out = std::io::BufWriter::new(f);
                let report = dump::dump(conn, &dump::DumpOptions::default(), &mut out)?;
                std::io::Write::flush(&mut out)?;
                anyhow::Ok((report, path))
            },
            |app, res| match res {
                Ok((r, path)) => app.set_status(format!("Dumped {} table(s), {} row(s) to {}", r.tables, r.rows, path.display()), false),
                Err(e) => app.set_status(format!("Dump failed: {e:#}"), true),
            },
        );
    }

    /// Choose a dump file and open the Restore dialog for it.
    pub(crate) fn pick_restore(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql"]).add_filter("All files", &["*"]).pick_file() else {
            return;
        };
        self.db_run(
            "Reading the dump",
            move |conn| {
                let res = std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|sql| dump::prepare(conn, &sql));
                (res.map_err(|e| format!("{e:#}")), path)
            },
            |app, (res, path)| match res {
                Ok(restore) => app.dialog = Some(Dialog::Restore(Box::new(RestoreState { path, restore, clean: false, error: None, busy: false }))),
                Err(e) => app.set_status(format!("{}: {e}", path.display()), true),
            },
        );
    }

    pub(crate) fn pick_sql_file() -> Option<(PathBuf, String)> {
        let path = rfd::FileDialog::new().add_filter("SQL", &["sql"]).add_filter("All files", &["*"]).pick_file()?;
        let sql = std::fs::read_to_string(&path).ok()?;
        Some((path, sql))
    }

    /// Choose a SQL file and open the Run SQL file dialog for it.
    pub(crate) fn pick_script(&mut self) {
        let Some((path, sql)) = Self::pick_sql_file() else { return };
        let Some(db) = &self.db else { return };
        let statements = db::split_statements(&sql).len();
        if statements == 0 {
            self.set_status(format!("{} has no SQL statements", path.display()), true);
            return;
        }
        let asks = db.safe_mode.confirmations(&sql, &db.writing_functions).into_iter().map(|(s, k)| (s, if let simple_database::safety::Kind::Dangerous(why) = k { Some(why) } else { None }))
            .collect();
        self.dialog = Some(Dialog::RunScript(Box::new(ScriptState {
            path,
            sql,
            statements,
            asks,
            transaction: false,
            keep_going: false,
            progress: None,
            outcome: None,
        })));
    }

    /// Run the dialog's script on the database thread; the outcome goes back
    /// into the dialog (or to the status bar if it was closed).
    pub(crate) fn run_script(&mut self, st: &mut ScriptState) {
        let opts = db::ScriptOptions { transaction: st.transaction, keep_going: st.keep_going, max_rows: MAX_QUERY_ROWS };
        let progress = Arc::new(AtomicUsize::new(0));
        st.progress = Some(progress.clone());
        let (sql, statements, path) = (st.sql.clone(), st.statements, st.path.clone());
        self.db_run(
            "Running the SQL file",
            move |conn| {
                let started = Instant::now();
                let mut errors = Vec::new();
                let mut results = Vec::new();
                // What ran, also when the script stops with an error.
                let mut done = db::ScriptReport { statements, ..Default::default() };
                let res = conn.run_script(&sql, &opts, &mut |step| {
                    done.run += 1;
                    progress.store(done.run, Ordering::Relaxed);
                    match step.result {
                        Ok(rs) => {
                            done.changed += rs.iter().filter_map(|r| r.changes).sum::<u64>();
                            results.extend(rs.iter().filter(|r| r.changes.is_none()).cloned());
                        }
                        Err(e) => errors.push((step.line, format!("{}\n  {e:#}", step.sql.lines().next().unwrap_or_default()))),
                    }
                });
                if results.len() > MAX_SCRIPT_RESULTS {
                    results.drain(..results.len() - MAX_SCRIPT_RESULTS);
                }
                let (report, stopped) = match res {
                    Ok(r) => (r, None),
                    Err(e) => (done, Some(format!("{e:#}"))),
                };
                ScriptOutcome { report, errors, stopped, elapsed: started.elapsed(), results }
            },
            move |app, out| {
                app.refresh_db();
                let failed = out.stopped.is_some() || !out.errors.is_empty();
                let label = app.db.as_ref().map(|d| d.label.clone()).unwrap_or_default();
                app.set_status(format!("Ran {} on {label}{}", path.display(), if failed { " with errors" } else { "" }), failed);
                if let Some(Dialog::RunScript(st)) = &mut app.dialog
                    && st.path == path
                {
                    st.progress = None;
                    st.outcome = Some(out);
                }
            },
        );
    }
}
