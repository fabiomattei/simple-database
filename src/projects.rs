//! Recent projects (`projects.json` in `config_dir()`): a project is a folder, and with it the
//! schema file and the database last used while it was open. `contents` looks at what a folder
//! holds (a `.dbs` schema, Laravel migrations, databases `discover` finds), for opening it.

use crate::connections::{config_dir, write_file};
use crate::{db, discover};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The most projects kept; the least recently used go first.
const LIMIT: usize = 60;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub path: PathBuf,
    /// The schema file last open with the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<PathBuf>,
    /// The database last open with it: a SQLite path, a URL without its password, or `@name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db: Option<String>,
    #[serde(default)]
    pub last_used: u64,
}

impl Project {
    /// The folder's name.
    pub fn name(&self) -> String {
        self.path.file_name().unwrap_or(self.path.as_os_str()).to_string_lossy().into_owned()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ProjectsFile {
    #[serde(default)]
    projects: Vec<Project>,
}

/// The recent projects, most recently used first.
#[derive(Clone, Debug, Default)]
pub struct Projects {
    dir: PathBuf,
    pub list: Vec<Project>,
}

impl Projects {
    pub fn load() -> Result<Projects> {
        Projects::load_from(&config_dir())
    }

    /// A missing file is no projects.
    pub fn load_from(dir: &Path) -> Result<Projects> {
        let path = dir.join("projects.json");
        let file: ProjectsFile = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProjectsFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut list = file.projects;
        list.sort_by_key(|p| std::cmp::Reverse(p.last_used));
        Ok(Projects { dir: dir.to_path_buf(), list })
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let file = ProjectsFile { projects: self.list.clone() };
        write_file(&self.dir.join("projects.json"), &serde_json::to_string_pretty(&file)?, false)
    }

    pub fn get(&self, path: &Path) -> Option<&Project> {
        self.list.iter().find(|p| p.path == path)
    }

    /// Re-reads the file (another window may have changed it), lets `edit` change the project
    /// at `path` (added if new), marks it used now and saves.
    pub fn update(&mut self, path: &Path, edit: impl FnOnce(&mut Project)) -> Result<()> {
        *self = Projects::load_from(&self.dir)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut project = match self.list.iter().position(|p| p.path == path) {
            Some(i) => self.list.remove(i),
            None => Project { path: path.to_path_buf(), ..Project::default() },
        };
        edit(&mut project);
        if let Some(target) = &project.db {
            project.db = Some(db::redact(target));
        }
        project.last_used = now.max(self.list.first().map_or(0, |p| p.last_used));
        self.list.insert(0, project);
        self.list.truncate(LIMIT);
        self.save()
    }

    /// Re-reads the file and removes the project at `path`.
    pub fn forget(&mut self, path: &Path) -> Result<()> {
        *self = Projects::load_from(&self.dir)?;
        self.list.retain(|p| p.path != path);
        self.save()
    }
}

/// What a project folder holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contents {
    /// The project's schema file if it still exists, else `schema.dbs`, else the first `.dbs`.
    pub schema: Option<PathBuf>,
    /// Laravel migrations (`database/migrations`).
    pub migrations: Option<PathBuf>,
    /// The project's database, if it still exists or is a server.
    pub db: Option<String>,
    /// Databases found in the folder (`.env`, compose, Prisma), when the project has none.
    pub found: Vec<discover::Found>,
}

impl Contents {
    /// A short line of what is there: `Laravel · schema.dbs · app.db`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.migrations.is_some() {
            parts.push("Laravel".to_string());
        }
        if let Some(s) = &self.schema
            && let Some(n) = s.file_name()
        {
            parts.push(n.to_string_lossy().into_owned());
        }
        match &self.db {
            Some(d) if db::is_url(d) || d.starts_with('@') => parts.push(d.trim_start_matches('@').to_string()),
            Some(d) => parts.push(Path::new(d).file_name().map_or(d.clone(), |n| n.to_string_lossy().into_owned())),
            None if !self.found.is_empty() => parts.push(format!("{} database(s) found", self.found.len())),
            None => {}
        }
        parts.join(" · ")
    }
}

/// What `project` (its folder, and what was last used with it) holds now.
pub fn contents(project: &Project) -> Contents {
    let dir = &project.path;
    let schema = project.schema.clone().filter(|p| p.is_file()).or_else(|| find_schema(dir));
    let migrations = Some(dir.join("database").join("migrations")).filter(|m| m.is_dir());
    let db = project.db.clone().filter(|t| db::is_url(t) || t.starts_with('@') || Path::new(t).is_file());
    let found = if db.is_none() && dir.is_dir() { discover::discover(dir) } else { Vec::new() };
    Contents { schema, migrations, db, found }
}

/// `schema.dbs` in `dir`, else its first `.dbs` file by name.
fn find_schema(dir: &Path) -> Option<PathBuf> {
    let preferred = dir.join("schema.dbs");
    if preferred.is_file() {
        return Some(preferred);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dbs")))
        .collect();
    files.sort();
    files.into_iter().next()
}

/// The project a path chosen as one names: a folder as is; the Laravel project around a
/// `database/migrations` folder; the folder holding a file.
pub fn root_of(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.parent().map_or_else(|| path.to_path_buf(), Path::to_path_buf);
    }
    if path.ends_with(Path::new("database").join("migrations"))
        && let Some(root) = path.parent().and_then(Path::parent)
    {
        return root.to_path_buf();
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sdb-projects-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn update_keeps_most_recent_first_and_forget_removes() {
        let dir = scratch("store");
        let mut store = Projects::load_from(&dir).unwrap();
        assert!(store.list.is_empty());
        store.update(Path::new("/work/a"), |_| {}).unwrap();
        store.update(Path::new("/work/b"), |p| p.db = Some("postgres://u:secret@h/db".into())).unwrap();
        store.update(Path::new("/work/a"), |p| p.schema = Some("/work/a/schema.dbs".into())).unwrap();
        let again = Projects::load_from(&dir).unwrap();
        let paths: Vec<&Path> = again.list.iter().map(|p| p.path.as_path()).collect();
        assert_eq!(paths, [Path::new("/work/a"), Path::new("/work/b")]);
        assert_eq!(again.list[0].schema.as_deref(), Some(Path::new("/work/a/schema.dbs")));
        // Passwords are not kept.
        assert!(!again.list[1].db.as_deref().unwrap().contains("secret"));
        store.forget(Path::new("/work/a")).unwrap();
        assert_eq!(Projects::load_from(&dir).unwrap().list.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contents_finds_schema_migrations_and_database() {
        let dir = scratch("contents");
        std::fs::write(dir.join("b.dbs"), "").unwrap();
        std::fs::write(dir.join("a.dbs"), "").unwrap();
        let migrations = dir.join("database").join("migrations");
        std::fs::create_dir_all(&migrations).unwrap();
        let project = Project { path: dir.clone(), ..Project::default() };
        let c = contents(&project);
        assert_eq!(c.schema, Some(dir.join("a.dbs")));
        assert_eq!(c.migrations, Some(migrations.clone()));
        std::fs::write(dir.join("schema.dbs"), "").unwrap();
        let db = dir.join("app.db");
        std::fs::write(&db, "").unwrap();
        let project = Project { db: Some(db.to_string_lossy().into_owned()), ..project };
        let c = contents(&project);
        assert_eq!(c.schema, Some(dir.join("schema.dbs")));
        assert_eq!(c.summary(), "Laravel · schema.dbs · app.db");
        assert_eq!(root_of(&migrations), dir);
        assert_eq!(root_of(&db), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
