//! Projects: a Laravel or Rails folder and the database it uses, opened
//! together. Opening a project connects to the saved connection linked to
//! its folder, else opens the Connections dialog filled in from the folder's
//! `.env` / compose files; connecting to a linked connection opens its
//! project. Either way the window shows the project's pending migrations.

use crate::migration_files::MIGRATION_FOLDERS;
use crate::{App, DiagramSource, Dialog, MigrateSource, Tab, conns, window};
use simple_database::connections::Store;
use simple_database::framework_history;
use std::path::PathBuf;

impl App {
    /// File → Open project… and the Welcome window's card.
    pub(crate) fn pick_project(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            self.open_project(dir);
        }
    }

    pub(crate) fn open_project(&mut self, dir: PathBuf) {
        // Another window may have linked a connection meanwhile.
        if let Ok(fresh) = Store::load() {
            self.conns = fresh;
        }
        let migrations = match framework_history::find(&dir) {
            Some(p) => format!("{} migrations in {}", p.framework.label(), p.dir.display()),
            None => format!("no {MIGRATION_FOLDERS} folder"),
        };
        match self.conns.linked_to(&dir).map(|c| format!("@{}", c.name)) {
            // One database per window: its own window opens the project once connected.
            Some(target) if self.shows_other_db(&target) => {
                self.open_window(Some(window::Launch { target: target.clone(), labeled: None, tab: Some(Tab::Diagram.name()) }), &target);
            }
            Some(target) => {
                self.show_project(dir.clone());
                if self.db.is_none() {
                    self.open_db(&target);
                }
                self.set_status(format!("Project {}: {target}, {migrations}", dir.display()), false);
            }
            None if self.db.is_none() => {
                self.show_project(dir.clone());
                self.dialog = Some(Dialog::Connections(Box::new(conns::Manager::for_project(&self.conns, &dir))));
                self.set_status(format!("Project {}: {migrations}; pick its database", dir.display()), false);
            }
            None => {
                self.show_project(dir.clone());
                let hint = "link a connection to it (Connections → Project) to open both together";
                self.set_status(format!("Project {}: {migrations}, shown with the open database; {hint}", dir.display()), false);
            }
        }
    }

    /// Make `dir` the project folder and, when it has migrations, show them:
    /// Diagram → With pending migrations, Migrate → From: Pending migrations.
    pub(crate) fn show_project(&mut self, dir: PathBuf) {
        let has_migrations = framework_history::find(&dir).is_some();
        self.project_dir = Some(dir);
        self.reload_pending_migrations();
        if has_migrations {
            self.er_source = DiagramSource::PendingMigrations;
            self.er.reset_layout();
            self.migrate_source = MigrateSource::PendingMigrations;
            self.tab = Tab::Diagram;
        }
    }

    /// The project folder of the saved connection `label` (`@name`) names, if it has one.
    pub(crate) fn linked_project(&self, label: &str) -> Option<PathBuf> {
        self.conns.get(label.strip_prefix('@')?)?.project_dir.clone()
    }
}
