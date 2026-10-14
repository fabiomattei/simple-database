//! The Connections dialog: saved connections on the left (search, favorites,
//! recent, groups), the selected connection's form on the right.

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke, Vec2};
use simple_database::connections::{Color, Connection, Kind, SafeMode, Store};
use simple_database::db::{self, Database};
use simple_database::discover::{self, Found};
use simple_database::tls;
use std::path::PathBuf;
use std::time::Instant;
use crate::theme::{self, Glyph};
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

    /// Open with an empty form, for a new connection.
    pub fn empty() -> Manager {
        Manager::blank()
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

    /// The dialog's content, `size` large: the list on a grey sidebar, the form on the right.
    pub fn show(&mut self, ui: &mut egui::Ui, store: &mut Store, size: Vec2) -> Option<Action> {
        let mut action = None;
        if let Some(r) = self.testing.as_ref().and_then(|t| t.poll()) {
            self.testing = None;
            self.message = Some(r.unwrap_or_else(|e| (e, true)));
        }
        let p = theme::palette(ui);
        let (full, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let left = egui::Rect::from_min_size(full.min, egui::vec2(LIST_WIDTH, size.y));
        let right = egui::Rect::from_min_max(egui::pos2(left.right(), full.top()), full.max);
        ui.painter().rect_filled(left, CornerRadius { nw: 14, sw: 14, ne: 0, se: 0 }, p.sidebar);
        ui.painter().vline(left.right(), left.y_range(), Stroke::new(1.0, p.separator));
        ui.scope_builder(egui::UiBuilder::new().max_rect(left.shrink2(egui::vec2(12.0, 16.0))), |ui| {
            self.list(ui, store, &mut action);
        });
        ui.scope_builder(egui::UiBuilder::new().max_rect(right.shrink2(egui::vec2(22.0, 18.0))), |ui| {
            self.form_ui(ui, store, &mut action);
        });
        action
    }

    /// Clear the form for a new connection, keeping the search and what was detected.
    fn reset(&mut self) {
        *self = Manager { search: std::mem::take(&mut self.search), detected: std::mem::take(&mut self.detected), ..Manager::blank() };
    }

    fn list(&mut self, ui: &mut egui::Ui, store: &Store, action: &mut Option<Action>) {
        let p = theme::palette(ui);
        ui.label(RichText::new("Connections").font(theme::bold(17.0)));
        ui.add_space(8.0);
        theme::search_field(ui, &mut self.search, "Search");
        ui.add_space(8.0);
        let new = theme::list_row(ui, self.editing.is_none(), |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
            theme::paint_glyph(ui.painter(), Glyph::Plus, rect.center(), 0.8, p.accent);
            ui.add(egui::Label::new("New connection").selectable(false));
        });
        if new.clicked() && self.editing.is_some() {
            self.reset();
        }
        ui.add_space(4.0);
        let mut picked: Option<(String, bool)> = None;
        let mut detected_pick: Option<Found> = None;
        let list_height = ui.available_height() - 22.0;
        egui::ScrollArea::vertical().id_salt("conn_list").max_height(list_height).auto_shrink([false, false]).show(ui, |ui| {
            let query = self.search.trim().to_lowercase();
            let mut row = |ui: &mut egui::Ui, c: &Connection| {
                let selected = self.editing.as_deref() == Some(c.name.as_str());
                let color = to_color32(c.color).unwrap_or(p.muted);
                let resp = theme::list_row(ui, selected, |ui| {
                    theme::database_icon(ui, color);
                    let star = if c.favorite { "★ " } else { "" };
                    ui.add(egui::Label::new(format!("{star}{}", c.name)).selectable(false).truncate());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(c.kind.label()).small().color(p.muted)).selectable(false));
                    });
                })
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
                    theme::section(ui, "Favorites");
                    favorites.into_iter().for_each(|c| row(ui, c));
                    ui.add_space(6.0);
                }
                let recent: Vec<&Connection> = store.recent(5).into_iter().filter(|c| !c.favorite).collect();
                if !recent.is_empty() {
                    theme::section(ui, "Recent");
                    recent.into_iter().for_each(|c| row(ui, c));
                    ui.add_space(6.0);
                }
            }
            // Found in the project and not saved yet.
            let saved: Vec<String> = store.connections.iter().map(|c| db::redact(&c.target(None))).collect();
            let fresh: Vec<&Found> = self.detected.iter().filter(|f| !saved.contains(&db::redact(&f.target))).collect();
            if query.is_empty() && !fresh.is_empty() {
                theme::section(ui, "Detected in project");
                for f in fresh {
                    let resp = theme::list_row(ui, false, |ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        theme::paint_glyph(ui.painter(), Glyph::Folder, rect.center(), 0.85, p.muted);
                        ui.add(egui::Label::new(f.name.as_str()).selectable(false).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(kind_of(&f.target)).small().color(p.muted)).selectable(false));
                        });
                    })
                    .on_hover_text(format!("{}\nfrom {}", db::redact(&f.target), f.source));
                    if resp.clicked() {
                        detected_pick = Some(f.clone());
                    }
                }
                ui.add_space(6.0);
            }
            for (group, conns) in store.grouped(&query) {
                theme::section(ui, if group.is_empty() { "All" } else { group });
                conns.into_iter().for_each(|c| row(ui, c));
                ui.add_space(6.0);
            }
            if store.connections.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("No saved connections yet. Fill in the form and press Save.").color(p.muted));
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
            self.reset();
            self.form = c;
            self.password_edited = pw.is_some();
            self.password = pw.unwrap_or_default();
            self.message = Some((format!("Found in {}; Save to keep it", f.source), false));
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.label(RichText::new("Double-click a connection to connect.").small().color(p.muted));
        });
    }

    /// The header with the connection's name and kind, the form in groups, and
    /// the buttons at the bottom.
    fn form_ui(&mut self, ui: &mut egui::Ui, store: &mut Store, action: &mut Option<Action>) {
        let p = theme::palette(ui);
        // The footer first, bottom up, so the form gets the space that is left.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            self.buttons(ui, store, action);
            ui.add_space(6.0);
            if let Some((msg, error)) = &self.message {
                let color = if *error { ui.visuals().error_fg_color } else { ok_color(ui) };
                ui.colored_label(color, if *error { format!("✖ {msg}") } else { format!("✔ {msg}") });
            }
            // What the connection will be once saved: new remote ones get verify-full.
            let mut effective = self.form.clone();
            if self.editing.is_none() && !effective.name.trim().is_empty() {
                effective.verify_remote();
            }
            let target = effective.target(None);
            if let Some(w) = tls::warning(&target) {
                ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {w}"));
            }
            ui.add(egui::Label::new(RichText::new(db::redact(&target)).monospace().small().color(p.muted)).truncate());
            ui.add_space(4.0);
            ui.separator();
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                self.header(ui, store);
                ui.add_space(10.0);
                egui::ScrollArea::vertical().id_salt("conn_form").auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_width(ui.available_width() - 8.0);
                    self.fields(ui, store);
                });
            });
        });
    }

    /// An icon in the connection's color, its name and what it is; the URL import beside it.
    fn header(&mut self, ui: &mut egui::Ui, store: &Store) {
        let p = theme::palette(ui);
        ui.horizontal(|ui| {
            let (icon, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
            let tint = to_color32(self.form.color).unwrap_or(p.accent);
            ui.painter().rect_filled(icon, CornerRadius::same(10), tint);
            theme::paint_glyph(ui.painter(), Glyph::Database, icon.center(), 1.4, Color32::WHITE);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let name = self.form.name.trim();
                let title = match (self.editing.is_some(), name.is_empty()) {
                    (_, false) => name.to_string(),
                    (true, true) => "Unnamed".to_string(),
                    (false, true) => "New connection".to_string(),
                };
                ui.add(egui::Label::new(RichText::new(title).font(theme::bold(18.0))).truncate());
                let status = match self.editing.as_deref() {
                    Some(_) if self.unsaved(store) => "unsaved changes",
                    Some(_) => "saved",
                    None if name.is_empty() => "name it to save it, or connect without saving",
                    None => "not saved yet",
                };
                ui.label(RichText::new(format!("{} · {status}", self.form.kind.label())).color(p.muted));
            });
        });
        ui.add_space(10.0);
        // Paste a URL to fill the form.
        let mut fill = false;
        egui::Frame::new().fill(p.well).corner_radius(CornerRadius::same(7)).inner_margin(Margin::symmetric(8, 3)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new("URL").font(theme::bold(11.0)).color(p.muted));
                let width = ui.available_width() - 50.0;
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.import_url)
                        .hint_text("Paste postgres://user:pass@host:5432/db or a file path to fill the form")
                        .frame(egui::Frame::NONE)
                        .desired_width(width),
                );
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let has_url = !self.import_url.trim().is_empty();
                fill = (ui.add_enabled(has_url, egui::Button::new("Fill").small()).clicked() || enter) && has_url;
            });
        });
        if fill {
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
    }

    fn fields(&mut self, ui: &mut egui::Ui, store: &Store) {
        let p = theme::palette(ui);
        let saved_password = self.editing.as_deref().is_some_and(|n| store.has_password(n));
        let f = &mut self.form;

        form_group(ui, if f.kind == Kind::Sqlite { "Database" } else { "Server" }, |ui| {
            field(ui, "Type", |ui| {
                let hover = |k| if k == Kind::Mysql { "MySQL or MariaDB" } else { "" };
                let items: Vec<(Kind, RichText, &str)> = Kind::ALL.iter().map(|&k| (k, RichText::new(k.label()), hover(k))).collect();
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if let (Some(k), _) = theme::segmented(ui, f.kind, &items, true)
                        && k != f.kind
                    {
                        f.kind = k;
                        f.port = None;
                    }
                });
            });
            if f.kind == Kind::Sqlite {
                field(ui, "File", |ui| {
                    let new = ui.button("New…").on_hover_text("Choose a new database file");
                    let browse = ui.button("Browse…");
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.database).hint_text("/path/to/app.db").desired_width(ui.available_width()));
                    });
                    if browse.clicked()
                        && let Some(p) = rfd::FileDialog::new()
                            .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                    {
                        f.database = p.to_string_lossy().into_owned();
                    }
                    if new.clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("SQLite database", &["db"]).set_file_name("app.db").save_file()
                    {
                        f.database = p.to_string_lossy().into_owned();
                    }
                });
                return;
            }
            field(ui, "Host", |ui| {
                let mut port = f.port.map(|p| p.to_string()).unwrap_or_default();
                let hint = f.kind.default_port().map(|p| p.to_string()).unwrap_or_default();
                let port_edit = ui.add(egui::TextEdit::singleline(&mut port).hint_text(hint).desired_width(56.0));
                if port_edit.changed() {
                    let digits: String = port.chars().filter(char::is_ascii_digit).collect();
                    f.port = digits.parse().ok();
                }
                ui.label(RichText::new("Port").color(p.muted));
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("localhost").desired_width(ui.available_width()));
                });
            });
            field(ui, "Database", |ui| {
                ui.add(egui::TextEdit::singleline(&mut f.database).hint_text("app").desired_width(ui.available_width()));
            });
        });

        if f.kind != Kind::Sqlite {
            form_group(ui, "Sign in", |ui| {
                field(ui, "User", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.user).hint_text("postgres, root …").desired_width(ui.available_width()));
                });
                field(ui, "Password", |ui| {
                    let hint = if saved_password { "•••••• (saved)" } else { "" };
                    if ui
                        .add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text(hint).desired_width(ui.available_width()))
                        .changed()
                    {
                        self.password_edited = true;
                    }
                });
                field(ui, "", |ui| {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.checkbox(&mut f.save_password, "Save password").on_hover_text(format!(
                        "Kept in {}, readable only by you, or in the OS keychain. Otherwise it is \
                         asked for each time (or taken from PGPASSWORD / MYSQL_PWD).",
                        store.dir().join("secrets.json").display()
                    ));
                    ui.add_enabled(f.save_password, egui::Checkbox::new(&mut f.keychain, "In the OS keychain")).on_hover_text(
                        "Keep the saved password in the macOS Keychain, Windows Credential Manager or the \
                         Secret Service (GNOME Keyring, KWallet) instead of secrets.json",
                        );
                    });
                });
            });
        }

        form_group(ui, "Save as", |ui| {
            field(ui, "Name", |ui| {
                ui.add(egui::TextEdit::singleline(&mut f.name).hint_text("empty = connect without saving").desired_width(ui.available_width()));
            });
            field(ui, "Group", |ui| {
                let groups = store.groups();
                ui.checkbox(&mut f.favorite, "★ Favorite");
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
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.group).hint_text("none").desired_width(ui.available_width()));
                });
            });
            field(ui, "Color", |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for c in Color::ALL {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                        let painter = ui.painter();
                        match to_color32(c) {
                            Some(col) => painter.circle_filled(rect.center(), 7.5, col),
                            None => painter.circle_stroke(rect.center(), 7.0, Stroke::new(1.2, p.muted)),
                        };
                        if f.color == c {
                            painter.circle_stroke(rect.center(), 10.0, Stroke::new(2.0, p.accent));
                        } else if resp.hovered() {
                            painter.circle_stroke(rect.center(), 10.0, Stroke::new(1.0, p.muted));
                        }
                        let name = if c == Color::None { "No color".to_string() } else { format!("{c:?}") };
                        if resp.on_hover_text(name).clicked() {
                            f.color = c;
                        }
                    }
                });
            });
        });

        form_group(ui, "Safety", |ui| {
            field(ui, "Safe mode", |ui| {
                let items: Vec<(SafeMode, RichText, &str)> =
                    SafeMode::ALL.iter().map(|&m| (m, RichText::new(m.label()), m.description())).collect();
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if let (Some(m), _) = theme::segmented(ui, f.safe_mode, &items, true) {
                        f.safe_mode = m;
                    }
                });
            });
            field(ui, "", |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(f.safe_mode.description()).small().color(p.muted)).wrap());
                });
            });
        });

        if f.kind != Kind::Sqlite {
            form_group(ui, "Advanced", |ui| {
                egui::CollapsingHeader::new("TLS and options").id_salt("conn_tls").show(ui, |ui| {
                    field(ui, "SSL mode", |ui| {
                        egui::ComboBox::from_id_salt("sslmode")
                            .width(ui.available_width())
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
                    });
                    for (label, value) in [("CA certificate", &mut f.sslrootcert), ("Client cert", &mut f.sslcert), ("Client key", &mut f.sslkey)] {
                        field(ui, label, |ui| {
                            let browse = ui.button("Browse…");
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::TextEdit::singleline(value).desired_width(ui.available_width()));
                            });
                            if browse.clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .add_filter("PEM", &["pem", "crt", "key"])
                                    .add_filter("All files", &["*"])
                                    .pick_file()
                            {
                                *value = p.to_string_lossy().into_owned();
                            }
                        });
                    }
                    field(ui, "Options", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.options).hint_text("key=value&key=value").desired_width(ui.available_width()));
                    });
                });
            });
        }
    }

    /// The footer: Delete and Duplicate on the left, Close, Test, Save and Connect on the right.
    fn buttons(&mut self, ui: &mut egui::Ui, store: &mut Store, action: &mut Option<Action>) {
        let p = theme::palette(ui);
        let unsaved = self.unsaved(store);
        let named = !self.form.name.trim().is_empty();
        let button = |text: RichText| egui::Button::new(text).corner_radius(CornerRadius::same(6)).min_size(egui::vec2(64.0, 28.0));
        ui.horizontal(|ui| {
            if let Some(name) = self.editing.clone() {
                let text = if self.confirm_delete {
                    RichText::new("Really delete?").color(ui.visuals().error_fg_color)
                } else {
                    RichText::new("Delete")
                };
                if ui.add(button(text)).clicked() {
                    if self.confirm_delete {
                        let res = Store::load().and_then(|mut fresh| {
                            fresh.remove(&name)?;
                            fresh.save()?;
                            Ok(fresh)
                        });
                        match res {
                            Ok(fresh) => {
                                *store = fresh;
                                self.reset();
                                self.message = Some((format!("Deleted @{name}"), false));
                            }
                            Err(e) => self.message = Some((format!("{e:#}"), true)),
                        }
                    } else {
                        self.confirm_delete = true;
                    }
                }
                if ui.add(button("Duplicate".into())).clicked() {
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
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let connect_hint = if named { "Save and connect" } else { "Connect without saving" };
                let connect = egui::Button::new(RichText::new("Connect").font(theme::bold(13.0)).color(Color32::WHITE))
                    .fill(p.accent)
                    .stroke(Stroke::NONE)
                    .corner_radius(CornerRadius::same(6))
                    .min_size(egui::vec2(88.0, 28.0));
                if ui.add(connect).on_hover_text(connect_hint).clicked() {
                    let saved = !named || !unsaved || self.save(store).map_err(|e| self.message = Some((format!("{e:#}"), true))).is_ok();
                    if saved {
                        self.connect(store, action);
                    }
                }
                if ui.add_enabled(named && unsaved, button("Save".into())).on_disabled_hover_text("Give it a name to save it").clicked() {
                    self.message = Some(match self.save(store) {
                        Ok(()) => (format!("Saved @{}", self.form.name), false),
                        Err(e) => (format!("{e:#}"), true),
                    });
                }
                if ui.add_enabled(self.testing.is_none(), button("Test".into())).on_hover_text("Try to connect, without saving").clicked() {
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
                if ui.add(button("Close".into())).clicked() {
                    *action = Some(Action::Close);
                }
            });
        });
    }
}

const LIST_WIDTH: f32 = 250.0;
const LABEL_WIDTH: f32 = 84.0;

/// A titled box of form fields, set off from the dialog's background.
fn form_group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette(ui);
    theme::section(ui, title);
    ui.add_space(2.0);
    let fill = if ui.visuals().dark_mode { Color32::from_rgb(0x27, 0x27, 0x29) } else { Color32::from_gray(0xf6) };
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, p.separator))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add(ui);
        });
    ui.add_space(6.0);
}

/// One form row: `label` right-aligned in a fixed column, then the widgets.
/// `add` lays out right to left, so a widget given `available_width` last fills the gap.
fn field(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette(ui);
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        ui.allocate_ui_with_layout(egui::vec2(LABEL_WIDTH, h), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.set_min_width(LABEL_WIDTH);
            ui.label(RichText::new(label).color(p.muted));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
    });
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
