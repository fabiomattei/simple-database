//! Find databases a project folder already describes: `DATABASE_URL`-style
//! entries and Laravel `DB_*` settings in `.env` files, PostgreSQL and MySQL
//! services in `docker-compose.yml` / `compose.yaml`, and the `datasource`
//! of a Prisma schema.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ENV_FILES: [&str; 5] = [".env", ".env.local", ".env.development", ".env.development.local", ".env.example"];
const COMPOSE_FILES: [&str; 4] = ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"];
const PRISMA_FILES: [&str; 2] = ["prisma/schema.prisma", "schema.prisma"];
/// URL parameters only Prisma understands.
const PRISMA_PARAMS: [&str; 5] = ["schema", "connection_limit", "pool_timeout", "pgbouncer", "statement_cache_size"];

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// A suggested connection name.
    pub name: String,
    /// SQLite path or URL, possibly with a password.
    pub target: String,
    /// Where it was found, e.g. `.env: DATABASE_URL`.
    pub source: String,
}

/// Everything found in `dir` (not its subfolders, apart from `prisma/`).
pub fn discover(dir: &Path) -> Vec<Found> {
    let project = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".into());
    let mut env: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for file in ENV_FILES {
        let Ok(text) = std::fs::read_to_string(dir.join(file)) else { continue };
        let vars = parse_env(&text);
        for (k, v) in &vars {
            if let Some(target) = url_target(v, dir) {
                out.push(Found { name: format!("{project} {}", short_key(k)), target, source: format!("{file}: {k}") });
            }
        }
        if let Some(target) = laravel_target(&vars, dir) {
            out.push(Found { name: format!("{project} db"), target, source: format!("{file}: DB_*") });
        }
        for (k, v) in vars {
            env.entry(k).or_insert(v);
        }
    }
    for file in COMPOSE_FILES {
        if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
            for (service, target) in compose_targets(&text, &env) {
                out.push(Found { name: format!("{project} {service}"), target, source: format!("{file}: service {service}") });
            }
        }
    }
    for file in PRISMA_FILES {
        let path = dir.join(file);
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some(target) = prisma_target(&text, &env, path.parent().unwrap_or(dir))
        {
            out.push(Found { name: format!("{project} prisma"), target, source: format!("{file}: datasource") });
        }
    }
    let mut seen = Vec::new();
    out.retain(|f| {
        let fresh = !seen.contains(&f.target);
        seen.push(f.target.clone());
        fresh
    });
    out
}

/// `DATABASE_URL` → `database`, `DB_URL` → `db`.
fn short_key(k: &str) -> String {
    let k = k.to_ascii_lowercase();
    let k = k.strip_suffix("_url").unwrap_or(&k);
    k.replace('_', " ")
}

fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let l = l.strip_prefix("export ").unwrap_or(l);
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = if let Some(q) = v.strip_prefix('"').and_then(|v| v.split_once('"')) {
                q.0.to_string()
            } else if let Some(q) = v.strip_prefix('\'').and_then(|v| v.split_once('\'')) {
                q.0.to_string()
            } else {
                v.split(" #").next().unwrap_or(v).trim().to_string()
            };
            Some((k.trim().to_string(), v))
        })
        .collect()
}

/// A URL value as something `Database::open` takes; SQLite paths are made
/// absolute against `dir`.
fn url_target(v: &str, dir: &Path) -> Option<String> {
    let (scheme, rest) = v.split_once(':')?;
    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "mysql" | "mariadb" if rest.starts_with("//") => Some(strip_params(v)),
        "sqlite" | "sqlite3" | "file" => {
            let path = rest.trim_start_matches("//");
            // sqlite:///abs/path keeps one slash; sqlite://rel/path doesn't.
            let path = if rest.starts_with("///") { format!("/{}", path.trim_start_matches('/')) } else { path.to_string() };
            let path = path.split('?').next()?.to_string();
            if path.is_empty() || path == ":memory:" {
                return None;
            }
            Some(absolute(dir, &path))
        }
        _ => None,
    }
}

fn absolute(dir: &Path, path: &str) -> String {
    let p = PathBuf::from(path);
    let p = if p.is_absolute() { p } else { dir.join(p) };
    std::path::absolute(&p).unwrap_or(p).to_string_lossy().into_owned()
}

fn strip_params(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else { return url.to_string() };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !PRISMA_PARAMS.contains(&p.split('=').next().unwrap_or("")))
        .collect();
    if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) }
}

fn userinfo(user: &str, password: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    match (user.is_empty(), password.is_empty()) {
        (true, _) => String::new(),
        (false, true) => format!("{}@", enc(user)),
        (false, false) => format!("{}:{}@", enc(user), enc(password)),
    }
}

/// Laravel-style `DB_CONNECTION`, `DB_HOST`, `DB_PORT`, `DB_DATABASE`, `DB_USERNAME`, `DB_PASSWORD`.
fn laravel_target(vars: &[(String, String)], dir: &Path) -> Option<String> {
    let get = |k: &str| vars.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str()).unwrap_or("");
    let scheme = match get("DB_CONNECTION") {
        "pgsql" | "postgres" | "postgresql" => "postgres",
        "mysql" | "mariadb" => "mysql",
        "sqlite" => {
            let db = get("DB_DATABASE");
            return (!db.is_empty()).then(|| absolute(dir, db));
        }
        _ => return None,
    };
    let host = if get("DB_HOST").is_empty() { "localhost" } else { get("DB_HOST") };
    let port = if get("DB_PORT").is_empty() { String::new() } else { format!(":{}", get("DB_PORT")) };
    Some(format!("{scheme}://{}{host}{port}/{}", userinfo(get("DB_USERNAME"), get("DB_PASSWORD")), get("DB_DATABASE")))
}

/// `${VAR}`, `${VAR:-default}` and `$VAR` from `env`.
fn interpolate(s: &str, env: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(body) = after.strip_prefix('{')
            && let Some(end) = body.find('}')
        {
            let expr = &body[..end];
            let (name, default) = match expr.split_once(":-").or_else(|| expr.split_once('-')) {
                Some((n, d)) => (n, d),
                None => (expr, ""),
            };
            out.push_str(env.get(name).map_or(default, String::as_str));
            rest = &body[end + 1..];
        } else {
            let end = after.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(after.len());
            if end == 0 {
                out.push('$');
            } else {
                out.push_str(env.get(&after[..end]).map_or("", String::as_str));
            }
            rest = &after[end..];
        }
    }
    out + rest
}

/// (service, URL) for each PostgreSQL / MySQL / MariaDB service. A lenient,
/// indentation-based reading of the YAML that is enough for compose files.
fn compose_targets(text: &str, env: &HashMap<String, String>) -> Vec<(String, String)> {
    #[derive(Default)]
    struct Service {
        name: String,
        image: String,
        env: HashMap<String, String>,
        ports: Vec<String>,
    }
    let unquote = |s: &str| s.trim().trim_matches('"').trim_matches('\'').to_string();
    let mut services: Vec<Service> = Vec::new();
    let mut in_services = false;
    let mut service_indent = None;
    let mut section = "";
    let mut section_indent = 0;
    for raw in text.lines() {
        let line = raw.split(" #").next().unwrap_or(raw);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let l = line.trim();
        if indent == 0 {
            in_services = l == "services:";
            service_indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        if service_indent.is_none_or(|si| indent <= si) && l.ends_with(':') && !l.starts_with('-') {
            service_indent = Some(indent);
            services.push(Service { name: l.trim_end_matches(':').to_string(), ..Default::default() });
            section = "";
            continue;
        }
        let Some(s) = services.last_mut() else { continue };
        if !section.is_empty() && indent <= section_indent {
            section = "";
        }
        if section.is_empty() {
            let (key, value) = l.split_once(':').unwrap_or((l, ""));
            match key.trim() {
                "image" => s.image = interpolate(&unquote(value), env).to_ascii_lowercase(),
                "environment" => (section, section_indent) = ("environment", indent),
                "ports" => (section, section_indent) = ("ports", indent),
                _ => {}
            }
            continue;
        }
        let item = l.strip_prefix('-').map(str::trim);
        match section {
            "environment" => {
                let pair = match item {
                    Some(i) => unquote(i).split_once('=').map(|(k, v)| (k.to_string(), v.to_string())),
                    None => l.split_once(':').map(|(k, v)| (k.trim().to_string(), unquote(v))),
                };
                if let Some((k, v)) = pair {
                    s.env.insert(k.trim().to_string(), interpolate(&v, env));
                }
            }
            "ports" => {
                if let Some(i) = item {
                    s.ports.push(interpolate(&unquote(i), env));
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for s in services {
        let get = |k: &str| s.env.get(k).map(String::as_str).unwrap_or("");
        let (scheme, inner, user, password, db) = if ["postgres", "postgis", "timescale"].iter().any(|k| s.image.contains(k)) {
            let user = if get("POSTGRES_USER").is_empty() { "postgres" } else { get("POSTGRES_USER") };
            let db = if get("POSTGRES_DB").is_empty() { user } else { get("POSTGRES_DB") };
            ("postgres", "5432", user, get("POSTGRES_PASSWORD"), db)
        } else if ["mysql", "mariadb", "percona"].iter().any(|k| s.image.contains(k)) {
            let var = |k: &str| {
                let v = get(&format!("MYSQL_{k}"));
                if v.is_empty() { get(&format!("MARIADB_{k}")) } else { v }
            };
            let (user, password) = if var("USER").is_empty() { ("root", var("ROOT_PASSWORD")) } else { (var("USER"), var("PASSWORD")) };
            ("mysql", "3306", user, password, var("DATABASE"))
        } else {
            continue;
        };
        if db.is_empty() {
            continue;
        }
        // "5433:5432", "127.0.0.1:5433:5432", "5432"; the host port is the one before the container's.
        let port = s.ports.iter().find_map(|p| {
            let parts: Vec<&str> = p.split('/').next()?.split(':').collect();
            match parts.len() {
                1 if parts[0] == inner => Some(inner.to_string()),
                n if n >= 2 && parts[n - 1] == inner => Some(parts[n - 2].to_string()),
                _ => None,
            }
        });
        let Some(port) = port else { continue }; // not published to the host
        let port = if port == inner { String::new() } else { format!(":{port}") };
        out.push((s.name, format!("{scheme}://{}localhost{port}/{db}", userinfo(user, password))));
    }
    out
}

/// The datasource URL of a Prisma schema.
fn prisma_target(text: &str, env: &HashMap<String, String>, dir: &Path) -> Option<String> {
    let start = text.find("datasource")?;
    let body = &text[start..];
    let body = &body[body.find('{')? + 1..body.find('}')?];
    let mut url = None;
    for line in body.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() != "url" {
            continue;
        }
        let v = v.trim();
        url = if let Some(var) = v.strip_prefix("env(") {
            env.get(var.trim_end_matches(')').trim().trim_matches('"')).cloned()
        } else {
            Some(v.trim_matches('"').to_string())
        };
    }
    url_target(&url?, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_project_databases() {
        let dir = std::env::temp_dir().join(format!("sdb-discover-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("prisma")).unwrap();
        std::fs::write(
            dir.join(".env"),
            "# comment\nexport DATABASE_URL=\"postgresql://app:s3cret@localhost:5432/app?schema=public\"\nPG_PASS=pw\nREDIS_URL=redis://x\n\
             SHADOW_URL=sqlite:./data/dev.db\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("docker-compose.yml"),
            "version: '3'\nservices:\n  db:\n    image: postgres:17\n    environment:\n      POSTGRES_USER: app\n      \
             POSTGRES_PASSWORD: ${PG_PASS}\n      POSTGRES_DB: shop\n    ports:\n      - \"55432:5432\"\n  \
             cache:\n    image: redis\n  maria:\n    image: \"mariadb:11\"\n    environment:\n      - MARIADB_ROOT_PASSWORD=${ROOT:-root}\n      \
             - MARIADB_DATABASE=shop\n    ports:\n      - 127.0.0.1:3306:3306\n  hidden:\n    image: mysql\n    environment:\n      MYSQL_DATABASE: x\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("prisma/schema.prisma"),
            "generator client {}\ndatasource db {\n  provider = \"sqlite\"\n  url      = \"file:./dev.db\"\n}\n",
        )
        .unwrap();
        let found = discover(&dir);
        let targets: Vec<&str> = found.iter().map(|f| f.target.as_str()).collect();
        let abs = |p: &str| std::path::absolute(dir.join(p)).unwrap().to_string_lossy().into_owned();
        assert_eq!(
            targets,
            [
                "postgresql://app:s3cret@localhost:5432/app",
                abs("./data/dev.db").as_str(),
                "postgres://app:pw@localhost:55432/shop",
                "mysql://root:root@localhost/shop",
                abs("prisma/./dev.db").as_str(),
            ]
        );
        assert_eq!(found[0].source, ".env: DATABASE_URL");
        assert!(found[0].name.ends_with(" database"));
        assert_eq!(found[2].source, "docker-compose.yml: service db");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_laravel_settings() {
        let vars = parse_env("DB_CONNECTION=pgsql\nDB_HOST=127.0.0.1\nDB_PORT=5433\nDB_DATABASE=app\nDB_USERNAME=me\nDB_PASSWORD='p@ss'\n");
        assert_eq!(laravel_target(&vars, Path::new("/")).unwrap(), "postgres://me:p%40ss@127.0.0.1:5433/app");
    }
}
