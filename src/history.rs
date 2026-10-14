//! Query history and saved queries, kept in `history.json` and `queries.json`
//! in the config directory (see [`crate::connections::config_dir`]).

use crate::connections::{config_dir, write_file};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// History entries kept; older ones are dropped.
pub const MAX_HISTORY: usize = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Entry {
    pub sql: String,
    /// The database label (`@name`, file path or URL without password).
    pub database: String,
    /// Unix seconds.
    pub at: u64,
    pub elapsed_ms: f64,
    /// The error message, if it failed.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SavedQuery {
    pub name: String,
    pub sql: String,
}

#[derive(Serialize, Deserialize, Default)]
struct HistoryFile {
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Default)]
struct QueriesFile {
    queries: Vec<SavedQuery>,
}

#[derive(Debug, Default)]
pub struct QueryLog {
    dir: PathBuf,
    /// Oldest first.
    pub history: Vec<Entry>,
    /// Sorted by name.
    pub saved: Vec<SavedQuery>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl QueryLog {
    pub fn load() -> Result<QueryLog> {
        QueryLog::load_from(&config_dir())
    }

    /// Missing files are empty.
    pub fn load_from(dir: &Path) -> Result<QueryLog> {
        fn read<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
            match std::fs::read_to_string(path) {
                Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            }
        }
        let history: HistoryFile = read(&dir.join("history.json"))?;
        let queries: QueriesFile = read(&dir.join("queries.json"))?;
        Ok(QueryLog { dir: dir.to_path_buf(), history: history.entries, saved: queries.queries })
    }

    /// Re-read the files before a change, so what other windows saved meanwhile is kept.
    fn reload(&mut self) {
        if !self.dir.as_os_str().is_empty()
            && let Ok(fresh) = QueryLog::load_from(&self.dir)
        {
            self.history = fresh.history;
            self.saved = fresh.saved;
        }
    }

    fn save_history(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let file = HistoryFile { entries: self.history.clone() };
        write_file(&self.dir.join("history.json"), &serde_json::to_string_pretty(&file)?, true)
    }

    fn save_queries(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let file = QueriesFile { queries: self.saved.clone() };
        write_file(&self.dir.join("queries.json"), &serde_json::to_string_pretty(&file)?, false)
    }

    /// Add an entry and save. Running the same SQL on the same database again
    /// moves it to the end instead of adding a copy.
    pub fn record(&mut self, entry: Entry) -> Result<()> {
        let sql = entry.sql.trim();
        if sql.is_empty() {
            return Ok(());
        }
        self.reload();
        self.history.retain(|e| !(e.sql.trim() == sql && e.database == entry.database));
        self.history.push(entry);
        let excess = self.history.len().saturating_sub(MAX_HISTORY);
        self.history.drain(..excess);
        self.save_history()
    }

    pub fn clear_history(&mut self) -> Result<()> {
        self.reload();
        self.history.clear();
        self.save_history()
    }

    /// Entries whose SQL or database contains `query` (any case), newest first.
    pub fn search(&self, query: &str) -> Vec<&Entry> {
        let q = query.trim().to_lowercase();
        self.history
            .iter()
            .rev()
            .filter(|e| q.is_empty() || e.sql.to_lowercase().contains(&q) || e.database.to_lowercase().contains(&q))
            .collect()
    }

    /// Save `sql` as `name`, replacing a query with that name.
    pub fn save_query(&mut self, name: &str, sql: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("the query needs a name");
        }
        self.reload();
        self.saved.retain(|q| q.name != name);
        self.saved.push(SavedQuery { name: name.to_string(), sql: sql.trim().to_string() });
        self.saved.sort_by_key(|q| q.name.to_lowercase());
        self.save_queries()
    }

    pub fn remove_query(&mut self, name: &str) -> Result<()> {
        self.reload();
        self.saved.retain(|q| q.name != name);
        self.save_queries()
    }

    pub fn get(&self, name: &str) -> Option<&SavedQuery> {
        self.saved.iter().find(|q| q.name == name)
    }
}

/// A short relative time like "5 min ago".
pub fn ago(at: u64, now: u64) -> String {
    let s = now.saturating_sub(at);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_saves() {
        let dir = std::env::temp_dir().join(format!("sdb-history-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = QueryLog::load_from(&dir).unwrap();
        let entry = |sql: &str, db: &str| Entry { sql: sql.into(), database: db.into(), at: 1, ..Default::default() };
        log.record(entry("SELECT 1", "a.db")).unwrap();
        log.record(entry("SELECT 2", "a.db")).unwrap();
        log.record(entry(" SELECT 1", "a.db")).unwrap();
        log.record(entry("SELECT 1", "@prod")).unwrap();
        log.record(entry("  ", "a.db")).unwrap();
        let sqls: Vec<_> = log.search("").iter().map(|e| (e.sql.trim().to_string(), e.database.clone())).collect();
        assert_eq!(sqls, [("SELECT 1".into(), "@prod".into()), ("SELECT 1".into(), "a.db".into()), ("SELECT 2".to_string(), "a.db".to_string())]);
        assert_eq!(log.search("prod").len(), 1);

        log.save_query("b", "SELECT b").unwrap();
        log.save_query("A", "SELECT a").unwrap();
        log.save_query("b", "SELECT bb").unwrap();
        assert!(log.save_query(" ", "x").is_err());
        let log = QueryLog::load_from(&dir).unwrap();
        assert_eq!(log.history.len(), 3);
        assert_eq!(log.saved.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(), ["A", "b"]);
        assert_eq!(log.get("b").unwrap().sql, "SELECT bb");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(ago(100, 100 + 7200), "2 h ago");
    }

    #[test]
    fn caps_clears_and_keeps_history_private() {
        let dir = std::env::temp_dir().join(format!("sdb-history-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = QueryLog::load_from(&dir).unwrap();
        let entry = |i: usize| Entry { sql: format!("SELECT {i}"), database: "a.db".into(), at: i as u64, ..Default::default() };
        log.history = (0..MAX_HISTORY + 19).map(entry).collect();
        log.save_history().unwrap();
        log.record(entry(MAX_HISTORY + 19)).unwrap();
        let log2 = QueryLog::load_from(&dir).unwrap();
        assert_eq!(log2.history.len(), MAX_HISTORY);
        assert_eq!(log2.history[0].sql, "SELECT 20", "the oldest go first");
        assert_eq!(log2.search("select 519")[0].at, 519);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("history.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "history can hold secrets");
        }
        log.clear_history().unwrap();
        assert!(QueryLog::load_from(&dir).unwrap().history.is_empty());

        log.save_query("x", "SELECT 1").unwrap();
        log.remove_query("x").unwrap();
        assert!(QueryLog::load_from(&dir).unwrap().saved.is_empty());

        // Fields added later default when an older file lacks them; a broken file is an error.
        std::fs::write(dir.join("history.json"), r#"{"entries": [{"sql": "SELECT 1"}]}"#).unwrap();
        assert_eq!(QueryLog::load_from(&dir).unwrap().history[0].database, "");
        std::fs::write(dir.join("queries.json"), "{").unwrap();
        assert!(QueryLog::load_from(&dir).unwrap_err().to_string().contains("queries.json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn windows_keep_each_others_changes() {
        let dir = std::env::temp_dir().join(format!("sdb-history-windows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut a, mut b) = (QueryLog::load_from(&dir).unwrap(), QueryLog::load_from(&dir).unwrap());
        let entry = |sql: &str| Entry { sql: sql.into(), database: "a.db".into(), ..Default::default() };
        a.record(entry("SELECT 1")).unwrap();
        b.record(entry("SELECT 2")).unwrap();
        a.save_query("x", "SELECT x").unwrap();
        b.save_query("y", "SELECT y").unwrap();
        let log = QueryLog::load_from(&dir).unwrap();
        assert_eq!(log.history.len(), 2);
        assert_eq!(log.saved.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ago_reads_naturally() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(100, 159), "just now");
        assert_eq!(ago(100, 160), "1 min ago");
        assert_eq!(ago(0, 3599), "59 min ago");
        assert_eq!(ago(0, 86_400 * 3), "3 d ago");
        assert_eq!(ago(200, 100), "just now", "a clock that went back");
    }
}
