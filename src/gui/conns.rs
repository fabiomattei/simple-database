//! The Connections dialog: saved connections on the left (search, favorites,
//! recent, groups), the selected connection's form on the right.

use eframe::egui::{self, Color32, RichText};
use simple_database::connections::{Color, Connection, Kind, SafeMode, Store};
use simple_database::db::{self, Database};
use simple_database::discover::{self, Found};
use simple_database::tls;
use std::path::PathBuf;
use std::time::Instant;
use crate::worker::Task;

const SSL_MODES: [&str; 6] = ["", "disable", "prefer", "require", "verify-ca", "verify-full"];

pub struct Manager {
    search: String,
    /// The saved connection the form was loaded from; `None` for a new one.
    editing: Option<String>,
    form: Connection,
    /// Typed password. Saved only with "Save password"; otherwise used for this session.
    password: String,
    password_edited: bool,
    import_url: String,
    /// Outcome of the last action: (text, is_error).
    pub message: Option<(String, bool)>,
    confirm_delete: bool,
    /// Databases described by files in the project folders.
    detected: Vec<Found>,
    /// The Test button's connection attempt, while it runs.
    testing: Option<Task<(String, bool)>>,
}

/// What the dialog asks the app to do.
pub enum Action {
    Connect { target: String, label: String, safe_mode: SafeMode },
    Close,
}

impl Manager {
    /// Open with `select` (or the most recent connection) loaded in the form.
    pub fn new(store: &Store, select: Option<&str>) -> Manager {
        let mut m = Manager::blank();
        let name = select.or_else(|| store.recent(1).first().map(|c| c.name.as_str()));
        if let Some(c) = name.and_then(|n| store.get(n)) {
            m.load(c);
        }
        m
    }

    /// An unsaved form filled from a URL or path, e.g. after it failed to connect.
    pub fn for_target(target: &str, error: String) -> Manager {
        let mut m = Manager::blank();
        if let Ok((c, pw)) = Connection::from_target("", target) {
            m.form = c;
            m.password = pw.unwrap_or_default();
        }
        m.message = Some((error, true));
        m
    }

    fn blank() -> Manager {
        Manager {
            search: String::new(),
            editing: None,
            form: Connection::default(),
            password: String::new(),
            password_edited: false,
            import_url: String::new(),
            message: None,
            confirm_delete: false,
            detected: Vec::new(),
            testing: None,
        }
    }

    /// Also list databases found in `dirs` (.env, docker-compose.yml, Prisma).
    pub fn with_detected(mut self, dirs: &[PathBuf]) -> Manager {
        for d in dirs {
            for f in discover::discover(d) {
                if !self.detected.iter().any(|x| x.target == f.target) {
                    self.detected.push(f);
                }
            }
        }
        self
    }

    fn load(&mut self, c: &Connection) {
        self.editing = Some(c.name.clone());
        self.form = c.clone();
        self.password.clear();
        self.password_edited = false;
        self.message = None;
        self.confirm_delete = false;
    }

    /// What to connect to with the form as it is: (target, label).
    fn target(&self, store: &Store) -> anyhow::Result<(String, String)> {
        let pw = match (self.password.is_empty(), self.editing.as_deref()) {
            (false, _) => Some(self.password.clone()),
            (true, Some(name)) => store.password(name)?,
            (true, None) => None,
        };
        let target = self.form.target(pw.as_deref());
        let label = if self.form.name.trim().is_empty() { db::redact(&target) } else { format!("@{}", self.form.name.trim()) };
        Ok((target, label))
    }

    /// Connect with the form as it is, or show why not.
    fn connect(&mut self, store: &Store, action: &mut Option<Action>) {
        match self.target(store) {
            Ok((target, label)) => *action = Some(Action::Connect { target, label, safe_mode: self.form.safe_mode }),
            Err(e) => self.message = Some((format!("{e:#}"), true)),
        }
    }

    /// Save the form, re-reading the store first so changes made meanwhile
    /// (say by the CLI) aren't overwritten.
    fn save(&mut self, store: &mut Store) -> anyhow::Result<()> {
        let mut fresh = Store::load()?;
        let password = self.password_edited.then(|| self.password.clone());
        fresh.upsert(self.editing.as_deref(), self.form.clone(), password)?;
        fresh.save()?;
        *store = fresh;
        let name = self.form.name.trim().to_string();
        // The store may have filled in some fields (sslmode).
        if let Some(saved) = store.get(&name) {
            self.form = saved.clone();
        }
        self.editing = Some(name);
        self.password_edited = false;
        Ok(())
    }

    fn unsaved(&self, store: &Store) -> bool {
        match self.editing.as_deref().and_then(|n| store.get(n)) {
            Some(c) => *c != self.form || self.password_edited,
            None => true,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, store: &mut Store) -> Option<Action> {
        let mut action = None;
        if let Some(r) = self.testing.as_ref().and_then(|t| t.poll()) {
            self.testing = None;
            self.message = Some(r.unwrap_or_else(|e| (e, true)));
        }
        ui.heading("Connections");
        ui.add_space(4.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(230.0);
                self.list(ui, store, &mut action);
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(470.0);
                self.form_ui(ui, store, &mut action);
            });
        });
        action
    }

    fn list(&mut self, ui: &mut egui::Ui, store: &Store, action: &mut Option<Action>) {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Search").desired_width(150.0));
            if ui.button("➕ New").clicked() {
                *self = Manager { search: std::mem::take(&mut self.search), detected: std::mem::take(&mut self.detected), ..Manager::blank() };
            }
        });
        ui.add_space(4.0);
        let mut picked: Option<(String, bool)> = None;
        let mut detected_pick: Option<Found> = None;
        egui::ScrollArea::vertical().id_salt("conn_list").max_height(440.0).auto_shrink([false, false]).show(ui, |ui| {
            if store.connections.is_empty() {
                ui.label(RichText::new("No saved connections yet. Fill in the form and press Save.").weak());
            }
            let query = self.search.trim().to_lowercase();
            let mut row = |ui: &mut egui::Ui, c: &Connection| {
                let selected = self.editing.as_deref() == Some(c.name.as_str());
                let resp = ui
                    .horizontal(|ui| {
                        color_dot(ui, c.color);
                        let star = if c.favorite { "★ " } else { "" };
                        let r = ui.selectable_label(selected, format!("{star}{}", c.name));
                        ui.label(RichText::new(c.kind.label()).small().weak());
                        r
                    })
                    .inner
                    .on_hover_text(c.summary());
                if resp.double_clicked() {
                    picked = Some((c.name.clone(), true));
                } else if resp.clicked() {
                    picked = Some((c.name.clone(), false));
                }
            };
            if query.is_empty() {
                let favorites: Vec<&Connection> = store.connections.iter().filter(|c| c.favorite).collect();
                if !favorites.is_empty() {
                    ui.label(RichText::new("FAVORITES").small().strong().weak());
                    favorites.into_iter().for_each(|c| row(ui, c));
                    ui.add_space(6.0);
                }
                let recent: Vec<&Connection> = store.recent(5).into_iter().filter(|c| !c.favorite).collect();
                if !recent.is_empty() {
                    ui.label(RichText::new("RECENT").small().strong().weak());
                    recent.into_iter().for_each(|c| row(ui, c));
                    ui.add_space(6.0);
                }
            }
            // Found in the project and not saved yet.
            let saved: Vec<String> = store.connections.iter().map(|c| db::redact(&c.target(None))).collect();
            let fresh: Vec<&Found> = self.detected.iter().filter(|f| !saved.contains(&db::redact(&f.target))).collect();
            if query.is_empty() && !fresh.is_empty() {
                ui.label(RichText::new("DETECTED IN PROJECT").small().strong().weak());
                for f in fresh {
                    let resp = ui
                        .horizontal(|ui| {
                            let r = ui.selectable_label(false, format!("🔎 {}", f.name));
                            ui.label(RichText::new(kind_of(&f.target)).small().weak());
                            r
                        })
                        .inner
                        .on_hover_text(format!("{}\nfrom {}", db::redact(&f.target), f.source));
                    if resp.clicked() {
                        detected_pick = Some(f.clone());
                    }
                }
                ui.add_space(6.0);
            }
            for (group, conns) in store.grouped(&query) {
                if group.is_empty() {
                    ui.label(RichText::new("ALL").small().strong().weak());
                    conns.into_iter().for_each(|c| row(ui, c));
                } else {
                    egui::CollapsingHeader::new(RichText::new(format!("📁 {group}")).strong())
                        .id_salt(("conn_group", group.to_lowercase()))
                        .default_open(true)
                        .show(ui, |ui| conns.into_iter().for_each(|c| row(ui, c)));
                }
            }
        });
        if let Some((name, connect)) = picked
            && let Some(c) = store.get(&name)
        {
            self.load(c);
            if connect {
                self.connect(store, action);
            }
        }
        if let Some(f) = detected_pick
            && let Ok((mut c, pw)) = Connection::from_target(&store.unique_name(&f.name), &f.target)
        {
            c.save_password = pw.is_some();
            *self = Manager {
                search: std::mem::take(&mut self.search),
                detected: std::mem::take(&mut self.detected),
                ..Manager::blank()
            };
            self.form = c;
            self.password_edited = pw.is_some();
            self.password = pw.unwrap_or_default();
            self.message = Some((format!("Found in {}; Save to keep it", f.source), false));
        }
        ui.label(RichText::new("Double-click to connect.").small().weak());
    }

    fn form_ui(&mut self, ui: &mut egui::Ui, store: &mut Store, action: &mut Option<Action>) {
        ui.horizontal(|ui| {
            ui.label("Import URL");
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.import_url)
                    .hint_text("postgres://user:pass@host:5432/db")
                    .desired_width(300.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(!self.import_url.trim().is_empty(), egui::Button::new("Fill")).clicked() || enter)
                && !self.import_url.trim().is_empty()
            {
                match Connection::from_target(&self.form.name, &self.import_url) {
                    Ok((mut c, pw)) => {
                        // Keep what the URL doesn't say.
                        c.group = std::mem::take(&mut self.form.group);
                        c.color = self.form.color;
                        c.favorite = self.form.favorite;
                        c.save_password = self.form.save_password || pw.is_some();
                        c.last_used = self.form.last_used;
                        self.form = c;
                        if let Some(pw) = pw {
                            self.password = pw;
                            self.password_edited = true;
                        }
                        self.import_url.clear();
                        self.message = None;
                    }
                    Err(e) => self.message = Some((format!("{e:#}"), true)),
                }
            }
        });
        ui.separator();

        let f = &mut self.form;
        let saved_password = self.editing.as_deref().is_some_and(|n| store.has_password(n));
        egui::Grid::new("conn_form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Name");
            ui.add(
                egui::TextEdit::singleline(&mut f.name)
                    .hint_text("empty = connect without saving")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Type");
            ui.horizontal(|ui| {
                for k in Kind::ALL {
                    if ui.selectable_label(f.kind == k, k.label()).clicked() && f.kind != k {
                        f.kind = k;
                        f.port = None;
                    }
                }
            });
            ui.end_row();

            if f.kind == Kind::Sqlite {
                ui.label("File");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.database).hint_text("/path/to/app.db").desired_width(300.0));
                    if ui.button("Browse…").clicked()
                        && let Some(p) = rfd::FileDialog::new()
                            .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                    {
                        f.database = p.to_string_lossy().into_owned();
                    }
                    if ui.button("New…").on_hover_text("Choose a new database file").clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("SQLite database", &["db"]).set_file_name("app.db").save_file()
                    {
                        f.database = p.to_string_lossy().into_owned();
                    }
                });
                ui.end_row();
            } else {
                ui.label("Host");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("localhost").desired_width(260.0));
                    ui.label("Port");
                    let mut port = f.port.map(|p| p.to_string()).unwrap_or_default();
                    let hint = f.kind.default_port().map(|p| p.to_string()).unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut port).hint_text(hint).desired_width(60.0)).changed() {
                        let digits: String = port.chars().filter(char::is_ascii_digit).collect();
                        f.port = digits.parse().ok();
                    }
                });
                ui.end_row();

                ui.label("User");
                ui.add(egui::TextEdit::singleline(&mut f.user).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label("Password");
                ui.horizontal(|ui| {
                    let hint = if saved_password { "•••••• (saved)" } else { "" };
                    if ui
                        .add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text(hint).desired_width(260.0))
                        .changed()
                    {
                        self.password_edited = true;
                    }
                    ui.checkbox(&mut f.save_password, "Save password").on_hover_text(format!(
                        "Kept in {}, readable only by you, or in the OS keychain. Otherwise it is \
                         asked for each time (or taken from PGPASSWORD / MYSQL_PWD).",
                        store.dir().join("secrets.json").display()
                    ));
                });
                ui.end_row();

                ui.label("");
                ui.add_enabled(f.save_password, egui::Checkbox::new(&mut f.keychain, "In the OS keychain")).on_hover_text(
                    "Keep the saved password in the macOS Keychain, Windows Credential Manager or the \
                     Secret Service (GNOME Keyring, KWallet) instead of secrets.json",
                );
                ui.end_row();

                ui.label("Database");
                ui.add(egui::TextEdit::singleline(&mut f.database).hint_text("app").desired_width(f32::INFINITY));
                ui.end_row();
            }

            ui.label("Group");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut f.group).hint_text("none").desired_width(260.0));
                let groups = store.groups();
                if !groups.is_empty() {
                    ui.menu_button("Pick…", |ui| {
                        for g in groups {
                            if ui.button(g).clicked() {
                                f.group = g.to_string();
                                ui.close();
                            }
                        }
                    });
                }
                ui.checkbox(&mut f.favorite, "★ Favorite");
            });
            ui.end_row();

            ui.label("Safe mode");
            ui.horizontal(|ui| {
                for m in SafeMode::ALL {
                    let text = if m == SafeMode::ReadOnly { format!("🔒 {}", m.label()) } else { m.label().to_string() };
                    ui.selectable_value(&mut f.safe_mode, m, text).on_hover_text(m.description());
                }
            });
            ui.end_row();

            ui.label("Color");
            ui.horizontal(|ui| {
                for c in Color::ALL {
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
                    let p = ui.painter();
                    match to_color32(c) {
                        Some(col) => p.circle_filled(rect.center(), 7.0, col),
                        None => p.circle_stroke(rect.center(), 6.5, ui.visuals().widgets.inactive.fg_stroke),
                    };
                    if f.color == c {
                        p.circle_stroke(rect.center(), 9.5, (1.5, ui.visuals().strong_text_color()));
                    }
                    if resp.on_hover_text(format!("{c:?}")).clicked() {
                        f.color = c;
                    }
                }
            });
            ui.end_row();
        });

        if f.kind != Kind::Sqlite {
            egui::CollapsingHeader::new("TLS and options").id_salt("conn_tls").show(ui, |ui| {
                egui::Grid::new("conn_tls_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("SSL mode");
                    egui::ComboBox::from_id_salt("sslmode")
                        .selected_text(if f.sslmode.is_empty() { "default (prefer)" } else { f.sslmode.as_str() })
                        .show_ui(ui, |ui| {
                            for m in SSL_MODES {
                                ui.selectable_value(&mut f.sslmode, m.to_string(), if m.is_empty() { "default (prefer)" } else { m });
                            }
                        })
                        .response
                        .on_hover_text(
                            "verify-full checks that the server's certificate is trusted and matches the host. \
                             New connections to remote hosts get it when saved.",
                        );
                    ui.end_row();
                    for (label, value) in [
                        ("CA certificate", &mut f.sslrootcert),
                        ("Client certificate", &mut f.sslcert),
                        ("Client key", &mut f.sslkey),
                    ] {
                        ui.label(label);
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(value).desired_width(280.0));
                            if ui.button("Browse…").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .add_filter("PEM", &["pem", "crt", "key"])
                                    .add_filter("All files", &["*"])
                                    .pick_file()
                            {
                                *value = p.to_string_lossy().into_owned();
                            }
                        });
                        ui.end_row();
                    }
                    ui.label("Options");
                    ui.add(
                        egui::TextEdit::singleline(&mut f.options)
                            .hint_text("key=value&key=value")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                });
            });
        }

        ui.add_space(4.0);
        // What the connection will be once saved: new remote ones get verify-full.
        let mut effective = self.form.clone();
        if self.editing.is_none() && !effective.name.trim().is_empty() {
            effective.verify_remote();
        }
        let target = effective.target(None);
        ui.label(RichText::new(db::redact(&target)).monospace().small().weak());
        if let Some(w) = tls::warning(&target) {
            ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {w}"));
        }
        if let Some((msg, error)) = &self.message {
            let color = if *error { ui.visuals().error_fg_color } else { ok_color(ui) };
            ui.colored_label(color, if *error { format!("✖ {msg}") } else { format!("✔ {msg}") });
        }
        ui.add_space(6.0);

        let unsaved = self.unsaved(store);
        let named = !self.form.name.trim().is_empty();
        ui.horizontal(|ui| {
            let connect_hint = if named { "Save and connect" } else { "Connect without saving" };
            if ui.button(RichText::new("Connect").strong()).on_hover_text(connect_hint).clicked() {
                let saved = !named || !unsaved || self.save(store).map_err(|e| self.message = Some((format!("{e:#}"), true))).is_ok();
                if saved {
                    self.connect(store, action);
                }
            }
            if ui.add_enabled(self.testing.is_none(), egui::Button::new("Test")).clicked() {
                match self.target(store) {
                    Ok((target, _)) => {
                        self.message = None;
                        self.testing = Some(Task::spawn("Testing the connection", ui.ctx(), move || {
                            let started = Instant::now();
                            match Database::open_labeled(&target, db::redact(&target)).and_then(|mut d| d.introspect()) {
                                Ok(s) => (format!("Connected in {:.0?}: {} table(s)", started.elapsed(), s.tables.len()), false),
                                Err(e) => (format!("{e:#}"), true),
                            }
                        }));
                    }
                    Err(e) => self.message = Some((format!("{e:#}"), true)),
                }
            }
            if self.testing.is_some() {
                ui.spinner();
            }
            if ui.add_enabled(named && unsaved, egui::Button::new("Save")).clicked() {
                self.message = Some(match self.save(store) {
                    Ok(()) => (format!("Saved @{}", self.form.name), false),
                    Err(e) => (format!("{e:#}"), true),
                });
            }
            if ui.add_enabled(self.editing.is_some(), egui::Button::new("Duplicate")).clicked() {
                self.form.name = store.unique_name(&format!("{} copy", self.form.name.trim()));
                self.form.last_used = None;
                let mut message = ("Copy created; Save to keep it".to_string(), false);
                if !self.password_edited
                    && let Some(name) = self.editing.as_deref()
                {
                    match store.password(name) {
                        Ok(pw) => self.password = pw.unwrap_or_default(),
                        Err(e) => message = (format!("Copy created without the password: {e:#}"), true),
                    }
                }
                self.editing = None;
                self.password_edited = !self.password.is_empty();
                self.message = Some(message);
            }
            if let Some(name) = self.editing.clone() {
                let text = if self.confirm_delete {
                    RichText::new("Really delete?").color(ui.visuals().error_fg_color)
                } else {
                    RichText::new("Delete")
                };
                if ui.button(text).clicked() {
                    if self.confirm_delete {
                        let res = Store::load().and_then(|mut fresh| {
                            fresh.remove(&name)?;
                            fresh.save()?;
                            Ok(fresh)
                        });
                        match res {
                            Ok(fresh) => {
                                *store = fresh;
                                *self = Manager {
                                    search: std::mem::take(&mut self.search),
                                    detected: std::mem::take(&mut self.detected),
                                    ..Manager::blank()
                                };
                                self.message = Some((format!("Deleted @{name}"), false));
                            }
                            Err(e) => self.message = Some((format!("{e:#}"), true)),
                        }
                    } else {
                        self.confirm_delete = true;
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    *action = Some(Action::Close);
                }
                if named && unsaved && self.editing.is_some() {
                    ui.label(RichText::new("unsaved changes").small().color(ui.visuals().warn_fg_color));
                }
            });
        });
    }
}

pub fn to_color32(c: Color) -> Option<Color32> {
    c.rgb().map(|[r, g, b]| Color32::from_rgb(r, g, b))
}

/// A small dot in the connection's color (blank space if it has none).
pub fn color_dot(ui: &mut egui::Ui, c: Color) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    if let Some(col) = to_color32(c) {
        ui.painter().circle_filled(rect.center(), 4.5, col);
    }
}

fn kind_of(target: &str) -> &'static str {
    match Connection::from_target("", target) {
        Ok((c, _)) => c.kind.label(),
        Err(_) => "",
    }
}

fn ok_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(0x6c, 0xc6, 0x6c) } else { Color32::from_rgb(0x1a, 0x7f, 0x37) }
}
