//! Saved connections, shared by the CLI and the GUI.
//!
//! They live in `connections.json` in the config directory (`$SDB_CONFIG_DIR`,
//! else `$XDG_CONFIG_HOME/sdb`, else `~/.config/sdb`). Saved passwords are kept
//! apart in `secrets.json`, readable only by the owner (like `~/.pgpass`), so
//! the connection list can be copied or shared without them.
//!
//! Anywhere a database target is accepted, `@name` means a saved connection.

pub use crate::safety::SafeMode;
use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Postgres,
    Mysql,
    Sqlite,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Postgres, Kind::Mysql, Kind::Sqlite];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Postgres => "PostgreSQL",
            Kind::Mysql => "MySQL",
            Kind::Sqlite => "SQLite",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            Kind::Postgres => Some(5432),
            Kind::Mysql => Some(3306),
            Kind::Sqlite => None,
        }
    }

    fn scheme(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::Mysql => "mysql",
            Kind::Sqlite => "",
        }
    }
}

/// A label color, to tell connections (say production from staging) apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    #[default]
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
}

impl Color {
    pub const ALL: [Color; 8] =
        [Color::None, Color::Red, Color::Orange, Color::Yellow, Color::Green, Color::Blue, Color::Purple, Color::Gray];

    pub fn rgb(self) -> Option<[u8; 3]> {
        Some(match self {
            Color::None => return None,
            Color::Red => [0xe5, 0x48, 0x4d],
            Color::Orange => [0xf0, 0x8c, 0x2e],
            Color::Yellow => [0xe0, 0xb7, 0x1c],
            Color::Green => [0x3f, 0xb9, 0x50],
            Color::Blue => [0x3b, 0x82, 0xf6],
            Color::Purple => [0x9b, 0x5d, 0xe5],
            Color::Gray => [0x8b, 0x94, 0x9e],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Connection {
    /// Unique; `@name` refers to it.
    pub name: String,
    pub kind: Kind,
    pub host: String,
    /// `None` = the default port for `kind`.
    pub port: Option<u16>,
    pub user: String,
    /// Database name, or the file path for SQLite.
    pub database: String,
    /// `disable`, `prefer`, `require`, `verify-ca` or `verify-full`; empty = driver default.
    pub sslmode: String,
    pub sslrootcert: String,
    pub sslcert: String,
    pub sslkey: String,
    /// Other URL parameters, as `key=value&key=value`.
    pub options: String,
    /// Folder in the connection list; empty = ungrouped.
    pub group: String,
    pub color: Color,
    pub favorite: bool,
    /// Keep the password in `secrets.json`. Otherwise it is asked for on connect
    /// (or taken from PGPASSWORD / MYSQL_PWD).
    pub save_password: bool,
    /// Unix seconds of the last successful connection.
    pub last_used: Option<u64>,
    /// What may run without asking.
    pub safe_mode: SafeMode,
}

/// Characters escaped in the user, password and database parts of a URL.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

impl Connection {
    /// A connection built from a SQLite path or a `postgres://` / `mysql://`
    /// URL, and the password the URL contained.
    pub fn from_target(name: &str, target: &str) -> Result<(Connection, Option<String>)> {
        let target = target.trim();
        let mut c = Connection { name: name.trim().to_string(), ..Default::default() };
        let Some((scheme, rest)) = target.split_once("://") else {
            c.kind = Kind::Sqlite;
            c.database = target.to_string();
            return Ok((c, None));
        };
        c.kind = match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Kind::Postgres,
            "mysql" | "mariadb" => Kind::Mysql,
            other => bail!("unsupported URL scheme '{other}' (expected postgres:// or mysql://)"),
        };
        let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
        let decode = |s: &str| -> Result<String> { Ok(percent_decode_str(s).decode_utf8()?.into_owned()) };

        // The password may contain '/' or '@', so the host starts after the last '@'.
        let (auth, host_db) = match main.rsplit_once('@') {
            Some((auth, rest)) => (Some(auth), rest),
            None => (None, main),
        };
        let mut password = None;
        if let Some(auth) = auth {
            let (user, pw) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
            c.user = decode(user)?;
            password = pw.map(decode).transpose()?.filter(|p| !p.is_empty());
        }
        let (host_port, db) = host_db.split_once('/').unwrap_or((host_db, ""));
        c.database = decode(db)?;
        let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
            let (h, after) = v6.split_once(']').context("unterminated IPv6 address")?;
            (format!("[{h}]"), after.strip_prefix(':'))
        } else {
            match host_port.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (host_port.to_string(), None),
            }
        };
        c.host = host;
        if let Some(p) = port.filter(|p| !p.is_empty()) {
            c.port = Some(p.parse().with_context(|| format!("invalid port '{p}'"))?);
        }
        let mut other = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let field = match key {
                "sslmode" => &mut c.sslmode,
                "sslrootcert" => &mut c.sslrootcert,
                "sslcert" => &mut c.sslcert,
                "sslkey" => &mut c.sslkey,
                _ => {
                    other.push(pair);
                    continue;
                }
            };
            *field = decode(value)?;
        }
        c.options = other.join("&");
        Ok((c, password))
    }

    /// What [`crate::db::Database::open`] takes: the SQLite path, or the URL
    /// with `password` filled in.
    pub fn target(&self, password: Option<&str>) -> String {
        if self.kind == Kind::Sqlite {
            return self.database.clone();
        }
        let enc = |s: &str| utf8_percent_encode(s, COMPONENT).to_string();
        let mut url = format!("{}://", self.kind.scheme());
        if !self.user.is_empty() {
            url += &enc(&self.user);
            if let Some(pw) = password.filter(|p| !p.is_empty()) {
                url += ":";
                url += &enc(pw);
            }
            url += "@";
        }
        url += if self.host.is_empty() { "localhost" } else { &self.host };
        if let Some(p) = self.port {
            url += &format!(":{p}");
        }
        if !self.database.is_empty() {
            url += "/";
            url += &enc(&self.database);
        }
        let mut params: Vec<String> = [
            ("sslmode", &self.sslmode),
            ("sslrootcert", &self.sslrootcert),
            ("sslcert", &self.sslcert),
            ("sslkey", &self.sslkey),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", enc(v)))
        .collect();
        params.extend(self.options.split('&').filter(|p| !p.is_empty()).map(str::to_string));
        if !params.is_empty() {
            url += "?";
            url += &params.join("&");
        }
        url
    }

    /// One line for lists: `user@host:port/db` or the SQLite path.
    pub fn summary(&self) -> String {
        crate::db::redact(&self.target(None))
    }

    /// Whether `query` (lowercase) matches the name, host, database, user, type or group.
    pub fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || [&self.name, &self.host, &self.database, &self.user, &self.group, self.kind.label()]
                .iter()
                .any(|f| f.to_lowercase().contains(query))
    }
}

/// The saved connections and passwords, as loaded from the config directory.
#[derive(Debug, Default)]
pub struct Store {
    dir: PathBuf,
    pub connections: Vec<Connection>,
    secrets: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
struct ConnectionsFile {
    connections: Vec<Connection>,
}

#[derive(Serialize, Deserialize, Default)]
struct SecretsFile {
    passwords: BTreeMap<String, String>,
}

/// Where `connections.json` and `secrets.json` live.
pub fn config_dir() -> PathBuf {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(d) = env("SDB_CONFIG_DIR") {
        return d;
    }
    if cfg!(windows)
        && let Some(d) = env("APPDATA")
    {
        return d.join("sdb");
    }
    if let Some(d) = env("XDG_CONFIG_HOME") {
        return d.join("sdb");
    }
    env("HOME").unwrap_or_default().join(".config").join("sdb")
}

impl Store {
    pub fn load() -> Result<Store> {
        Store::load_from(&config_dir())
    }

    /// Missing files are an empty store.
    pub fn load_from(dir: &Path) -> Result<Store> {
        fn read<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
            match std::fs::read_to_string(path) {
                Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            }
        }
        let file: ConnectionsFile = read(&dir.join("connections.json"))?;
        let secrets: SecretsFile = read(&dir.join("secrets.json"))?;
        Ok(Store { dir: dir.to_path_buf(), connections: file.connections, secrets: secrets.passwords })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let file = ConnectionsFile { connections: self.connections.clone() };
        write_file(&self.dir.join("connections.json"), &serde_json::to_string_pretty(&file)?, false)?;
        let secrets = SecretsFile { passwords: self.secrets.clone() };
        write_file(&self.dir.join("secrets.json"), &serde_json::to_string_pretty(&secrets)?, true)
    }

    pub fn get(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.name == name)
    }

    /// The saved password, if any.
    pub fn password(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    /// Add `conn`, or replace the one called `old_name` (which may rename it).
    /// `password` replaces the saved one when `conn.save_password` is set;
    /// `None` keeps it. Without `save_password` any saved password is dropped.
    pub fn upsert(&mut self, old_name: Option<&str>, mut conn: Connection, password: Option<String>) -> Result<()> {
        conn.name = conn.name.trim().to_string();
        let name = conn.name.as_str();
        if name.is_empty() {
            bail!("the connection needs a name");
        }
        if name.starts_with('@') || name.chars().any(char::is_control) {
            bail!("connection names can't start with '@' or contain control characters");
        }
        if old_name != Some(name) && self.get(name).is_some() {
            bail!("a connection named '{name}' already exists");
        }
        if conn.kind == Kind::Sqlite && conn.database.trim().is_empty() {
            bail!("choose the SQLite database file");
        }
        let mut secret = old_name.and_then(|o| self.secrets.remove(o));
        if conn.save_password {
            if let Some(pw) = password {
                secret = Some(pw).filter(|p| !p.is_empty());
            }
        } else {
            secret = None;
        }
        if let Some(pw) = secret {
            self.secrets.insert(conn.name.clone(), pw);
        }
        match old_name.and_then(|o| self.connections.iter().position(|c| c.name == o)) {
            Some(i) => self.connections[i] = conn,
            None => self.connections.push(conn),
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.secrets.remove(name);
        let before = self.connections.len();
        self.connections.retain(|c| c.name != name);
        self.connections.len() != before
    }

    /// Record a successful connection, for the recent list.
    pub fn touch(&mut self, name: &str) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if let Some(c) = self.connections.iter_mut().find(|c| c.name == name) {
            c.last_used = Some(now);
        }
    }

    /// A name not used yet: `base`, `base 2`, `base 3`, ...
    pub fn unique_name(&self, base: &str) -> String {
        let base = base.trim();
        (1..)
            .map(|i| if i == 1 { base.to_string() } else { format!("{base} {i}") })
            .find(|n| self.get(n).is_none())
            .unwrap()
    }

    /// The last `n` connections used, most recent first.
    pub fn recent(&self, n: usize) -> Vec<&Connection> {
        let mut v: Vec<&Connection> = self.connections.iter().filter(|c| c.last_used.is_some()).collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.last_used));
        v.truncate(n);
        v
    }

    /// Group names in order, `""` (ungrouped) last; within each group the
    /// matching connections, sorted by name.
    pub fn grouped(&self, query: &str) -> Vec<(&str, Vec<&Connection>)> {
        let query = query.trim().to_lowercase();
        let mut groups: BTreeMap<(bool, String), Vec<&Connection>> = BTreeMap::new();
        for c in self.connections.iter().filter(|c| c.matches(&query)) {
            groups.entry((c.group.is_empty(), c.group.to_lowercase())).or_default().push(c);
        }
        groups
            .into_values()
            .map(|mut v| {
                v.sort_by_key(|c| c.name.to_lowercase());
                (v[0].group.as_str(), v)
            })
            .collect()
    }

    /// The groups in use, for suggestions.
    pub fn groups(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.connections.iter().map(|c| c.group.as_str()).filter(|g| !g.is_empty()).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// A saved connection looked up by `@name`.
pub struct Resolved {
    /// The SQLite path or URL, with the saved password.
    pub target: String,
    pub name: String,
    pub safe_mode: SafeMode,
}

/// If `target` is `@name`, the saved connection; otherwise `None`.
pub fn resolve(target: &str) -> Result<Option<Resolved>> {
    let Some(name) = target.strip_prefix('@') else { return Ok(None) };
    let mut store = Store::load()?;
    let Some(c) = store.get(name) else {
        bail!("no saved connection named '{name}' (see `sdb conn list`)");
    };
    let resolved = Resolved { target: c.target(store.password(name)), name: name.to_string(), safe_mode: c.safe_mode };
    store.touch(name);
    // Recording the last use is a nicety; a read-only config must not stop the connection.
    let _ = store.save();
    Ok(Some(resolved))
}

pub(crate) fn write_file(path: &Path, text: &str, private: bool) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_round_trips() {
        let url = "postgres://app%40corp:p%2Fw%40rd@db.example.com:6543/my%20db?sslmode=verify-full&sslrootcert=%2Fetc%2Fca.pem&application_name=sdb";
        let (c, pw) = Connection::from_target("prod", url).unwrap();
        assert_eq!(c.kind, Kind::Postgres);
        assert_eq!(c.user, "app@corp");
        assert_eq!(pw.as_deref(), Some("p/w@rd"));
        assert_eq!(c.host, "db.example.com");
        assert_eq!(c.port, Some(6543));
        assert_eq!(c.database, "my db");
        assert_eq!(c.sslmode, "verify-full");
        assert_eq!(c.sslrootcert, "/etc/ca.pem");
        assert_eq!(c.options, "application_name=sdb");
        assert_eq!(c.target(pw.as_deref()), url);
        assert_eq!(c.summary(), "postgres://app%40corp@db.example.com:6543/my%20db?sslmode=verify-full&sslrootcert=%2Fetc%2Fca.pem&application_name=sdb");
    }

    #[test]
    fn parses_short_forms() {
        let (c, pw) = Connection::from_target("m", "mysql://root@[::1]/app").unwrap();
        assert_eq!((c.kind, c.host.as_str(), c.port, pw), (Kind::Mysql, "[::1]", None, None));
        assert_eq!(c.target(None), "mysql://root@[::1]/app");
        let (c, _) = Connection::from_target("l", "app.db").unwrap();
        assert_eq!((c.kind, c.target(None).as_str()), (Kind::Sqlite, "app.db"));
        assert!(Connection::from_target("x", "redis://h").is_err());
    }

    #[test]
    fn store_keeps_passwords_apart() {
        let dir = std::env::temp_dir().join(format!("sdb-conn-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::load_from(&dir).unwrap();
        let (mut c, pw) = Connection::from_target("prod", "postgres://u:secret@h/db").unwrap();
        c.save_password = true;
        store.upsert(None, c.clone(), pw).unwrap();
        assert!(store.upsert(None, c.clone(), None).is_err(), "duplicate name");
        store.save().unwrap();

        let text = std::fs::read_to_string(dir.join("connections.json")).unwrap();
        assert!(!text.contains("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("secrets.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let mut store = Store::load_from(&dir).unwrap();
        assert_eq!(store.password("prod"), Some("secret"));
        // Renaming carries the password along; `None` keeps it.
        c.name = "production".into();
        store.upsert(Some("prod"), c.clone(), None).unwrap();
        assert_eq!((store.password("prod"), store.password("production")), (None, Some("secret")));
        assert_eq!(store.connections.len(), 1);
        // Turning off save_password forgets it.
        c.save_password = false;
        store.upsert(Some("production"), c, None).unwrap();
        assert_eq!(store.password("production"), None);
        assert_eq!(store.unique_name("production"), "production 2");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn groups_and_search() {
        let mut store = Store::default();
        for (name, group, url) in [
            ("b", "", "app.db"),
            ("prod", "Work", "postgres://u@prod.example.com/app"),
            ("a", "", "x.db"),
            ("stage", "Work", "mysql://u@stage/app"),
        ] {
            let (mut c, _) = Connection::from_target(name, url).unwrap();
            c.group = group.into();
            store.upsert(None, c, None).unwrap();
        }
        let names = |q| {
            store
                .grouped(q)
                .into_iter()
                .map(|(g, cs)| format!("{g}:{}", cs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(",")))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(""), ["Work:prod,stage", ":a,b"]);
        assert_eq!(names("MYSQL"), ["Work:stage"]);
        assert_eq!(names("example"), ["Work:prod"]);
        store.touch("a");
        assert_eq!(store.recent(5).len(), 1);
    }
}
