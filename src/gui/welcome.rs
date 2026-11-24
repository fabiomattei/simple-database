//! The welcome screen, over the whole window: the app's name and buttons to start something, a
//! filter field, and one space of tiles: project folders, SQLite files and server connections,
//! each with its own icon, in a card per group (`Project::group`, `RecentFile::group`,
//! `Connection::group`). A tile dragged onto a card moves into that group; right-click to
//! choose a group, a card's heading to rename or dissolve it. A project is a folder: opening it
//! loads its schema file, its Laravel migrations (History tab) and the database last used with
//! it (`projects.rs`). Folders and files dropped on the window open.

use crate::{App, Dialog, Tab, conns, db_title, theme};
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, FontId, Key, Layout, Modifiers, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use simple_database::connections::{Kind, SafeMode, Store};
use simple_database::projects::{self, Contents, Project, Projects};
use simple_database::{db, sqlite};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MARGIN: f32 = 28.0;
const TILE_WIDTH: f32 = 124.0;
/// The icon's height: the folder's width over the golden ratio.
const ICON_HEIGHT: f32 = TILE_WIDTH / 1.618;
/// Two rows of the name, then where it is and what it is.
const TILE_TEXT: f32 = 70.0;
const TILE_HEIGHT: f32 = ICON_HEIGHT + TILE_TEXT;
const TILE_GAP: Vec2 = vec2(24.0, 16.0);
const CARD_PADDING: f32 = 14.0;
const CARD_GAP: f32 = 16.0;
const HEADING_HEIGHT: f32 = 26.0;
/// Light enough for the dark letters on every color.
const COLORS: [Color32; 9] = [
    Color32::from_rgb(0x7f, 0xd4, 0xe8),
    Color32::from_rgb(0xa8, 0x94, 0xe0),
    Color32::from_rgb(0xf5, 0xa0, 0x5a),
    Color32::from_rgb(0x7c, 0xd0, 0x6c),
    Color32::from_rgb(0xd4, 0x8a, 0xd0),
    Color32::from_rgb(0xf0, 0xd0, 0x60),
    Color32::from_rgb(0xa9, 0xb8, 0xa9),
    Color32::from_rgb(0xf0, 0x8a, 0x8a),
    Color32::from_rgb(0xb4, 0xb6, 0xc4),
];
/// Card backgrounds, barely off the screen's, so the tiles' colors stay what the eye goes to.
const TINTS_LIGHT: [Color32; 6] = [
    Color32::from_rgb(0xe9, 0xee, 0xf5),
    Color32::from_rgb(0xf0, 0xeb, 0xf4),
    Color32::from_rgb(0xea, 0xf2, 0xeb),
    Color32::from_rgb(0xf5, 0xef, 0xe4),
    Color32::from_rgb(0xf5, 0xea, 0xea),
    Color32::from_rgb(0xe7, 0xf2, 0xf2),
];
const TINTS_DARK: [Color32; 6] = [
    Color32::from_rgb(0x33, 0x3b, 0x45),
    Color32::from_rgb(0x3b, 0x35, 0x41),
    Color32::from_rgb(0x34, 0x3e, 0x38),
    Color32::from_rgb(0x40, 0x3b, 0x32),
    Color32::from_rgb(0x40, 0x35, 0x37),
    Color32::from_rgb(0x31, 0x3e, 0x40),
];
const UNGROUPED: &str = "Other";
const DIGITS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

/// What the welcome screen keeps between frames.
pub(crate) struct WelcomeState {
    /// The database of the last session, offered when it isn't listed otherwise.
    pub(crate) last: Option<String>,
    filter: String,
    /// Chosen with the arrow keys: an index into the tiles shown, in reading order.
    selected: Option<usize>,
    /// What each project folder holds, looked at once per showing.
    contents: HashMap<PathBuf, Contents>,
    /// The centers of the tiles drawn last frame, for the arrow keys.
    centers: Vec<Pos2>,
}

impl WelcomeState {
    pub(crate) fn new(last: Option<String>) -> Box<WelcomeState> {
        Box::new(WelcomeState { last, filter: String::new(), selected: None, contents: HashMap::new(), centers: Vec::new() })
    }
}

/// Which stored thing a tile is.
#[derive(Clone, Debug, PartialEq)]
enum ItemId {
    /// A project folder or a SQLite file opened directly.
    Path(PathBuf),
    /// A saved connection, by name.
    Conn(String),
    /// The last session's database, kept nowhere else.
    Last(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Icon {
    Folder,
    File,
    Server,
}

/// One tile.
struct Item {
    id: ItemId,
    icon: Icon,
    title: String,
    /// Where it is: the folder holding it, or the server.
    place: String,
    /// What it is: what a project holds, the kind of database.
    what: String,
    color: Color32,
    group: String,
    favorite: bool,
    last_used: u64,
    open: Open,
}

#[derive(Clone)]
enum Open {
    Project(PathBuf),
    Db(String),
}

impl Item {
    fn matches(&self, needle: &str) -> bool {
        needle.is_empty() || [&self.title, &self.place, &self.what, &self.group].iter().any(|t| t.to_lowercase().contains(needle))
    }

    /// A stable key for the tile's widget id.
    fn id_key(&self) -> String {
        match &self.id {
            ItemId::Path(p) => format!("p:{}", p.display()),
            ItemId::Conn(n) => format!("c:{n}"),
            ItemId::Last(t) => format!("l:{t}"),
        }
    }
}

/// What was picked on the welcome screen.
enum Pick {
    Open(Open),
    Forget(PathBuf),
    EditConnection(String),
    SetGroup(ItemId, String),
    RenameGroup(String, String),
    Dropped(Vec<PathBuf>),
    OpenProject,
    NewConnection,
    OpenSqlite,
    NewSqlite,
    Skip,
}

/// A group's tiles.
struct Section {
    /// Empty for the tiles in no group.
    group: String,
    /// `None` when nothing is grouped: the tiles then show without a card.
    title: Option<String>,
    items: Vec<Item>,
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

fn safe_note(mode: SafeMode) -> String {
    if mode == SafeMode::Off { String::new() } else { format!(" · {}", mode.label()) }
}

/// Every tile: projects whose folder exists, SQLite files that exist and aren't a saved
/// connection, saved connections, and the last session's database when it is none of these.
fn items(projects: &Projects, contents: &HashMap<PathBuf, Contents>, conns: &Store, last: Option<&str>) -> Vec<Item> {
    let mut out = Vec::new();
    for p in projects.list.iter().filter(|p| p.path.is_dir()) {
        out.push(Item {
            id: ItemId::Path(p.path.clone()),
            icon: Icon::Folder,
            title: p.name(),
            place: parent_label(&p.path),
            what: contents.get(&p.path).map(Contents::summary).unwrap_or_default(),
            color: path_color(&p.path),
            group: p.group.clone(),
            favorite: false,
            last_used: p.last_used,
            open: Open::Project(p.path.clone()),
        });
    }
    let saved_file = |path: &Path| conns.connections.iter().any(|c| c.kind == Kind::Sqlite && Path::new(&c.database) == path);
    for f in projects.files.iter().filter(|f| f.path.is_file() && !saved_file(&f.path)) {
        out.push(Item {
            id: ItemId::Path(f.path.clone()),
            icon: Icon::File,
            title: file_name(&f.path),
            place: parent_label(&f.path),
            what: "SQLite file".into(),
            color: path_color(&f.path),
            group: f.group.clone(),
            favorite: false,
            last_used: f.last_used,
            open: Open::Db(f.path.to_string_lossy().into_owned()),
        });
    }
    for c in &conns.connections {
        let file = c.kind == Kind::Sqlite;
        let default = if file { path_color(Path::new(&c.database)) } else { kind_color(c.kind) };
        out.push(Item {
            id: ItemId::Conn(c.name.clone()),
            icon: if file { Icon::File } else { Icon::Server },
            title: format!("{}{}", if c.favorite { "★ " } else { "" }, c.name),
            place: if file { parent_label(Path::new(&c.database)) } else { c.summary() },
            what: format!("{}{}", c.kind.label(), safe_note(c.safe_mode)),
            color: conns::to_color32(c.color).unwrap_or(default),
            group: c.group.clone(),
            favorite: c.favorite,
            last_used: c.last_used.unwrap_or(0),
            open: Open::Db(format!("@{}", c.name)),
        });
    }
    if let Some(t) = last {
        let listed = match t.strip_prefix('@') {
            Some(name) => conns.get(name).is_some(),
            None => {
                let canonical = Path::new(t).canonicalize().ok();
                out.iter().any(|i| matches!(&i.id, ItemId::Path(p) if Some(p) == canonical.as_ref())) || saved_file(Path::new(t))
            }
        };
        if !listed {
            let server = db::is_url(t) || t.starts_with('@');
            out.push(Item {
                id: ItemId::Last(t.to_string()),
                icon: if server { Icon::Server } else { Icon::File },
                title: db_title(t),
                place: if server { t.to_string() } else { parent_label(Path::new(t)) },
                what: "Last session".into(),
                color: path_color(Path::new(t)),
                group: String::new(),
                favorite: false,
                last_used: u64::MAX,
                open: Open::Db(t.to_string()),
            });
        }
    }
    out
}

/// The tiles matching `needle` in a section per group, alphabetically, then those in no group;
/// in each, favorites first, then the most recently used.
fn sections(mut items: Vec<Item>, needle: &str) -> Vec<Section> {
    items.retain(|i| i.matches(needle));
    items.sort_by_key(|i| (!i.favorite, std::cmp::Reverse(i.last_used), i.title.to_lowercase()));
    let mut groups: Vec<String> = items.iter().map(|i| i.group.clone()).filter(|g| !g.is_empty()).collect();
    groups.sort_by_key(|g| g.to_lowercase());
    groups.dedup();
    let any_group = !groups.is_empty();
    groups.push(String::new());
    groups
        .into_iter()
        .filter_map(|group| {
            let (inside, rest): (Vec<Item>, Vec<Item>) = std::mem::take(&mut items).into_iter().partition(|i| i.group == group);
            items = rest;
            let title = if group.is_empty() { any_group.then(|| UNGROUPED.to_string()) } else { Some(group.clone()) };
            (!inside.is_empty()).then_some(Section { group, title, items: inside })
        })
        .collect()
}

impl App {
    /// The welcome screen, while `Dialog::Welcome` is open.
    pub(crate) fn welcome(&mut self, ctx: &egui::Context) {
        let Some(Dialog::Welcome(mut st)) = self.dialog.take() else { return };
        if st.contents.is_empty() {
            st.contents = self.projects.list.iter().map(|p| (p.path.clone(), projects::contents(p))).collect();
        }
        let pick = show(ctx, &mut st, &self.projects, &self.conns);
        self.dialog = Some(Dialog::Welcome(st));
        let Some(pick) = pick else { return };
        let saved = match pick {
            Pick::Open(Open::Project(path)) => {
                self.open_project(&path);
                Ok(())
            }
            Pick::Open(Open::Db(target)) => {
                self.dialog = None;
                self.project = None;
                self.open_db(&target);
                Ok(())
            }
            Pick::Forget(path) => self.projects.forget(&path),
            Pick::EditConnection(name) => {
                self.dialog = Some(Dialog::Connections(Box::new(conns::Manager::new(&self.conns, Some(&name)))));
                Ok(())
            }
            Pick::SetGroup(ItemId::Path(path), group) => self.projects.set_group(&path, &group),
            Pick::SetGroup(ItemId::Conn(name), group) => Store::load().and_then(|mut fresh| {
                fresh.set_group(&name, &group);
                fresh.save()?;
                self.conns = fresh;
                Ok(())
            }),
            Pick::SetGroup(ItemId::Last(_), _) => Ok(()),
            Pick::RenameGroup(from, to) => self.projects.rename_group(&from, &to).and_then(|()| {
                let mut fresh = Store::load()?;
                fresh.rename_group(&from, &to);
                fresh.save()?;
                self.conns = fresh;
                Ok(())
            }),
            Pick::Dropped(paths) => {
                self.open_dropped(paths);
                Ok(())
            }
            Pick::OpenProject => {
                self.pick_project();
                Ok(())
            }
            Pick::NewConnection => {
                self.connections_dialog(true);
                Ok(())
            }
            Pick::OpenSqlite => {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                    .add_filter("All files", &["*"])
                    .pick_file()
                {
                    self.dialog = None;
                    self.project = None;
                    self.open_db(&p.to_string_lossy());
                }
                Ok(())
            }
            Pick::NewSqlite => {
                if let Some(p) = rfd::FileDialog::new().add_filter("SQLite database", &["db"]).set_file_name("app.db").save_file() {
                    self.dialog = None;
                    self.project = None;
                    self.open_db_at(&p.to_string_lossy(), Some(Tab::Migrate));
                }
                Ok(())
            }
            Pick::Skip => {
                self.dialog = None;
                Ok(())
            }
        };
        if let Err(e) = saved {
            self.set_status(format!("Cannot save the change: {e:#}"), true);
        }
    }

    /// Shows the welcome screen.
    pub(crate) fn show_welcome(&mut self) {
        let last = self.db.as_ref().map(|d| d.label.clone());
        if let Ok(fresh) = Projects::load() {
            self.projects = fresh;
        }
        if let Ok(fresh) = Store::load() {
            self.conns = fresh;
        }
        self.dialog = Some(Dialog::Welcome(WelcomeState::new(last)));
    }

    /// Asks for a project folder and opens it.
    pub(crate) fn pick_project(&mut self) {
        if let Some(folder) = rfd::FileDialog::new().set_title("Open project folder").pick_folder() {
            self.open_project(&folder);
        }
    }

    /// Opens the project at `path` (a folder, or a file or `database/migrations` in one): its
    /// schema file, its Laravel migrations and the database last used with it.
    pub(crate) fn open_project(&mut self, path: &Path) {
        let root = projects::root_of(path);
        if !root.is_dir() {
            self.set_status(format!("{} is not a folder", root.display()), true);
            return;
        }
        let project = self.projects.get(&root).cloned().unwrap_or_else(|| Project { path: root.clone(), ..Project::default() });
        let found = projects::contents(&project);
        self.dialog = None;
        self.project = Some(root.clone());
        let mut notes = Vec::new();
        if self.dirty() {
            notes.push("your unsaved schema stays in the editor".to_string());
        } else if let Some(s) = &found.schema {
            self.load_schema_file(s);
        } else {
            self.schema_text.clear();
            self.saved_text.clear();
            self.schema_path = None;
            self.er.reset_layout();
        }
        if let Some(m) = &found.migrations {
            self.history.open(m.clone());
        }
        self.tab = if found.migrations.is_some() && found.schema.is_none() { Tab::History } else { Tab::Schema };
        let schema = found.schema.clone();
        self.remember_project(|p| {
            if schema.is_some() {
                p.schema = schema;
            }
        });
        match &found.db {
            Some(t) => self.open_db(t),
            None if !found.found.is_empty() => notes.push(format!("{} database(s) found in it: Connections (⌘K) lists them", found.found.len())),
            None => {}
        }
        if found.db.is_none() {
            let name = project.name();
            let message = if notes.is_empty() { format!("Opened project {name}") } else { format!("Opened project {name}; {}", notes.join("; ")) };
            self.set_status(message, false);
        }
    }

    /// Records what is open with the current project, if there is one.
    pub(crate) fn remember_project(&mut self, edit: impl FnOnce(&mut Project)) {
        let Some(root) = self.project.clone() else { return };
        if let Err(e) = self.projects.update(&root, edit) {
            self.set_status(format!("Cannot save recent projects: {e:#}"), true);
        }
    }

    /// Records a SQLite file opened directly (`label` is its path), for the welcome screen.
    pub(crate) fn remember_sqlite_file(&mut self, label: &str) {
        let Ok(path) = Path::new(label).canonicalize() else { return };
        if let Err(e) = self.projects.touch_file(&path) {
            self.set_status(format!("Cannot save recent files: {e:#}"), true);
        }
    }

    /// Paths dropped on the welcome screen: a folder opens as a project, a schema file in the
    /// editor, a SQLite file as the database.
    fn open_dropped(&mut self, paths: Vec<PathBuf>) {
        let Some(first) = paths.into_iter().next() else { return };
        if first.is_dir() {
            self.open_project(&first);
        } else if sqlite::is_sqlite_file(&first) || matches!(first.extension().and_then(|e| e.to_str()), Some("db" | "sqlite" | "sqlite3")) {
            self.dialog = None;
            self.project = None;
            self.open_db(&first.to_string_lossy());
        } else if first.extension().is_some_and(|e| e.eq_ignore_ascii_case("dbs")) {
            self.dialog = None;
            self.load_schema_file(&first);
            self.tab = Tab::Schema;
        } else {
            self.set_status(format!("Cannot open {}: drop a folder, a .dbs schema or a SQLite file", first.display()), true);
        }
    }
}

fn show(ctx: &egui::Context, st: &mut WelcomeState, projects: &Projects, conns: &Store) -> Option<Pick> {
    let p = theme::palette_for_ctx(ctx);
    let screen = ctx.content_rect();
    let needle = st.filter.trim().to_lowercase();
    let all = items(projects, &st.contents, conns, st.last.as_deref());
    let total = all.len();
    let sections = sections(all, &needle);
    let shown: Vec<&Item> = sections.iter().flat_map(|s| &s.items).collect();
    let mut groups: Vec<&str> = projects
        .list
        .iter()
        .map(|p| p.group.as_str())
        .chain(projects.files.iter().map(|f| f.group.as_str()))
        .chain(conns.connections.iter().map(|c| c.group.as_str()))
        .filter(|g| !g.is_empty())
        .collect();
    groups.sort_by_key(|g| g.to_lowercase());
    groups.dedup();
    let mut pick = keys(ctx, st, &shown);
    let frame = egui::Frame::new().fill(p.content);
    egui::Modal::new(egui::Id::new("welcome")).frame(frame).backdrop_color(p.content).show(ctx, |ui| {
        let (full, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
        let inner = full.shrink(MARGIN);
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            if let Some(b) = title_row(ui) {
                pick = Some(b);
            }
            ui.add_space(16.0);
            let before = st.filter.clone();
            let filter_id = filter_field(ui, &mut st.filter);
            if ui.memory(|m| m.focused().is_none()) && !egui::Popup::is_any_open(ui.ctx()) {
                ui.memory_mut(|m| m.request_focus(filter_id));
            }
            if st.filter != before {
                st.selected = (!st.filter.trim().is_empty()).then_some(0);
            }
            ui.add_space(20.0);
            let body = Rect::from_min_max(pos2(inner.left(), ui.cursor().top()), pos2(inner.right(), inner.bottom() - 22.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
                if sections.is_empty() {
                    empty_state(ui, &st.filter, total);
                    return;
                }
                let cx = Cx { selected: st.selected, shortcuts: ui.input(|i| i.modifiers.command), groups: &groups, contents: &st.contents };
                let mut centers = Vec::new();
                egui::ScrollArea::vertical().id_salt("welcome_tiles").auto_shrink([false, false]).show(ui, |ui| {
                    if let Some(pk) = show_sections(ui, &sections, &cx, &mut centers) {
                        pick = Some(pk);
                    }
                });
                st.centers = centers;
            });
            let hint = "Drag a tile onto a group to move it · right-click for groups · ⌘1–⌘9 open the first tiles · Escape continues without a database";
            let hint = hint.replace('⌘', if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" });
            ui.painter().text(pos2(inner.left(), inner.bottom()), Align2::LEFT_BOTTOM, hint, FontId::proportional(11.5), p.muted);
        });
        drop_overlay(ui, full, p);
    });
    let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
    if !dropped.is_empty() {
        pick = Some(Pick::Dropped(dropped));
    }
    pick
}

/// The icon and name on the left, the buttons on the right.
fn title_row(ui: &mut egui::Ui) -> Option<Pick> {
    let p = theme::palette(ui);
    let mut pick = None;
    ui.horizontal(|ui| {
        let (logo, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
        ui.painter().rect_filled(logo, CornerRadius::same(10), p.accent);
        theme::paint_glyph(ui.painter(), theme::Glyph::Database, logo.center(), 1.4, Color32::WHITE);
        ui.add_space(4.0);
        ui.label(RichText::new("Simple Database").font(theme::bold(26.0)));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let green = Color32::from_rgb(0x34, 0xc7, 0x59);
            let orange = Color32::from_rgb(0xff, 0x9f, 0x0a);
            let violet = Color32::from_rgb(0xaf, 0x52, 0xde);
            let buttons = [
                (theme::Glyph::Folder, violet, "Open Project…", "A folder: its schema file, Laravel migrations and database", Pick::OpenProject),
                (theme::Glyph::Plus, p.accent, "New Connection…", "PostgreSQL, MySQL or SQLite, saved to reuse", Pick::NewConnection),
                (theme::Glyph::Folder, orange, "Open SQLite…", "A .db, .sqlite or .sqlite3 file", Pick::OpenSqlite),
                (theme::Glyph::Database, green, "New SQLite…", "An empty file to push a schema into", Pick::NewSqlite),
                (theme::Glyph::Arrow, p.muted, "Continue without a database", "Edit schemas, draw diagrams, browse migrations", Pick::Skip),
            ];
            for (glyph, tint, label, hover, value) in buttons {
                if header_button(ui, glyph, tint, label).on_hover_text(hover).clicked() {
                    pick = Some(value);
                }
            }
        });
    });
    pick
}

/// A framed button with a small painted icon before `label`.
fn header_button(ui: &mut egui::Ui, glyph: theme::Glyph, tint: Color32, label: &str) -> egui::Response {
    let p = theme::palette(ui);
    let text = ui.visuals().text_color();
    let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.0), text);
    let (icon_w, pad) = (16.0, 10.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(pad * 3.0 + icon_w + galley.size().x, 30.0), Sense::click());
    let fill = if resp.is_pointer_button_down_on() {
        p.selected
    } else if resp.hovered() {
        p.selected.gamma_multiply(0.6)
    } else {
        p.sidebar
    };
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(7), fill, Stroke::new(1.0, p.separator), StrokeKind::Inside);
    let icon = Rect::from_center_size(pos2(rect.left() + pad + icon_w / 2.0, rect.center().y), Vec2::splat(icon_w));
    painter.rect_filled(icon, CornerRadius::same(4), tint);
    theme::paint_glyph(painter, glyph, icon.center(), 0.6, Color32::WHITE);
    painter.galley(pos2(icon.right() + pad, rect.center().y - galley.size().y / 2.0), galley, text);
    resp
}

/// The filter field; Escape empties it. Returns its id, to keep the keyboard on it.
fn filter_field(ui: &mut egui::Ui, text: &mut String) -> egui::Id {
    let id = egui::Id::new("welcome_filter");
    if !text.is_empty() && ui.memory(|m| m.has_focus(id)) && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        text.clear();
    }
    let p = theme::palette(ui);
    egui::Frame::new().fill(p.well).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
        ui.add(
            egui::TextEdit::singleline(text)
                .id(id)
                .frame(egui::Frame::NONE)
                .hint_text("Filter projects, files and connections — arrows to choose, Enter to open")
                .font(FontId::proportional(14.0))
                .desired_width(420.0),
        );
    });
    id
}

/// What drawing the sections needs besides them.
struct Cx<'a> {
    selected: Option<usize>,
    /// ⌘ is held: the first nine tiles show their digit.
    shortcuts: bool,
    /// Every group name, for the tiles' menus.
    groups: &'a [&'a str],
    contents: &'a HashMap<PathBuf, Contents>,
}

/// A tile being dragged onto a card.
struct Dragged {
    id: ItemId,
    group: String,
}

/// The sections stacked as cards; `centers` gets each tile's center, in reading order.
fn show_sections(ui: &mut egui::Ui, sections: &[Section], cx: &Cx, centers: &mut Vec<Pos2>) -> Option<Pick> {
    let width = ui.available_width();
    let mut pick = None;
    let mut index = 0;
    for (n, section) in sections.iter().enumerate() {
        let framed = section.title.is_some();
        let pad = if framed { CARD_PADDING } else { 0.0 };
        let inner_w = width - 2.0 * pad;
        let columns = (((inner_w + TILE_GAP.x) / (TILE_WIDTH + TILE_GAP.x)) as usize).max(1);
        let rows = section.items.len().div_ceil(columns);
        let heading_h = if framed { HEADING_HEIGHT + 6.0 } else { 0.0 };
        let height = 2.0 * pad + heading_h + rows as f32 * TILE_HEIGHT + rows.saturating_sub(1) as f32 * TILE_GAP.y;
        let (card, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let background = ui.painter().add(egui::Shape::Noop);
        if let Some(title) = &section.title {
            let rect = Rect::from_min_size(card.min + vec2(pad, pad), vec2(inner_w, HEADING_HEIGHT));
            let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("welcome heading", &section.group)));
            if let Some(pk) = heading(&mut hui, title, &section.group, section.items.len()) {
                pick = Some(pk);
            }
        }
        for (i, item) in section.items.iter().enumerate() {
            let at = card.min + vec2(pad + (i % columns) as f32 * (TILE_WIDTH + TILE_GAP.x), pad + heading_h + (i / columns) as f32 * (TILE_HEIGHT + TILE_GAP.y));
            let rect = Rect::from_min_size(at, vec2(TILE_WIDTH, TILE_HEIGHT));
            centers.push(rect.center());
            let badge = (cx.shortcuts && index < 9).then_some(index + 1);
            if let Some(pk) = tile(ui, rect, item, cx, cx.selected == Some(index), badge) {
                pick = Some(pk);
            }
            index += 1;
        }
        // A card takes tiles dragged onto it into its group.
        let resp = ui.interact(card, ui.id().with(("welcome card", &section.group)), Sense::hover());
        let moves_in = |d: &Dragged| d.group != section.group && !matches!(d.id, ItemId::Last(_));
        if framed {
            let target = resp.dnd_hover_payload::<Dragged>().is_some_and(|d| moves_in(&d));
            let tint = if section.group.is_empty() {
                theme::palette(ui).sidebar
            } else {
                let tints = if ui.visuals().dark_mode { &TINTS_DARK } else { &TINTS_LIGHT };
                tints[n % tints.len()]
            };
            let stroke = if target { Stroke::new(2.0, theme::palette(ui).accent) } else { Stroke::NONE };
            ui.painter().set(background, egui::epaint::RectShape::new(card, CornerRadius::same(12), tint, stroke, StrokeKind::Inside));
            if let Some(d) = resp.dnd_release_payload::<Dragged>()
                && moves_in(&d)
            {
                pick = Some(Pick::SetGroup(d.id.clone(), section.group.clone()));
            }
        }
        ui.add_space(CARD_GAP);
    }
    pick
}

/// A card's heading: the group's name in spaced capitals with its count in a pill and a rule
/// to the right edge; right-click to rename the group or take everything out of it.
fn heading(ui: &mut egui::Ui, title: &str, group: &str, count: usize) -> Option<Pick> {
    let p = theme::palette(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), HEADING_HEIGHT), Sense::click());
    let painter = ui.painter();
    let format = egui::TextFormat { font_id: theme::bold(11.5), color: ui.visuals().text_color(), extra_letter_spacing: 1.2, ..Default::default() };
    let galley = painter.layout_job(egui::text::LayoutJob::single_section(title.to_uppercase(), format));
    let x = rect.left() + galley.size().x;
    painter.galley(pos2(rect.left(), rect.center().y - galley.size().y / 2.0), galley, Color32::PLACEHOLDER);
    let n = painter.layout_no_wrap(count.to_string(), FontId::proportional(10.5), p.muted);
    let pill = Rect::from_min_size(pos2(x + 8.0, rect.center().y - 8.0), vec2(n.size().x + 12.0, 16.0));
    painter.rect_filled(pill, CornerRadius::same(8), p.well);
    painter.galley(pill.center() - n.size() / 2.0, n, p.muted);
    if pill.right() + 10.0 < rect.right() {
        painter.hline(pill.right() + 10.0..=rect.right(), rect.center().y, Stroke::new(1.0, p.separator));
    }
    if group.is_empty() {
        return None;
    }
    let mut pick = None;
    egui::Popup::context_menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        ui.label(RichText::new("Rename group").small().color(p.muted));
        if let Some(name) = name_field(ui, resp.id.with("rename"), group) {
            pick = Some(Pick::RenameGroup(group.to_string(), name));
            ui.close();
        }
        ui.separator();
        if ui.button("Ungroup all").on_hover_text("Everything in it moves to no group").clicked() {
            pick = Some(Pick::RenameGroup(group.to_string(), String::new()));
            ui.close();
        }
    });
    pick
}

/// A text field kept under `id`; returns its text, trimmed, when Enter is pressed in it.
fn name_field(ui: &mut egui::Ui, id: egui::Id, start: &str) -> Option<String> {
    let mut text: String = ui.data(|d| d.get_temp(id)).unwrap_or_else(|| start.to_string());
    let resp = ui.add(egui::TextEdit::singleline(&mut text).hint_text("New group").desired_width(170.0));
    let done = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !text.trim().is_empty();
    if done {
        ui.data_mut(|d| d.remove::<String>(id));
        return Some(text.trim().to_string());
    }
    ui.data_mut(|d| d.insert_temp(id, text));
    None
}

/// One tile: its icon in its color with the first letters of its name, and below the name,
/// where it is and what it is.
fn tile(ui: &mut egui::Ui, rect: Rect, item: &Item, cx: &Cx, selected: bool, badge: Option<usize>) -> Option<Pick> {
    let p = theme::palette(ui);
    let id = ui.id().with(("welcome tile", item.id_key()));
    let resp = ui.interact(rect, id, Sense::click_and_drag());
    if !matches!(item.id, ItemId::Last(_)) {
        resp.dnd_set_drag_payload(Dragged { id: item.id.clone(), group: item.group.clone() });
    }
    if selected {
        resp.scroll_to_me(None);
    }
    let dragged = resp.dragged();
    let resp = resp.on_hover_text(hover_text(item, cx));
    let lift = ui.ctx().animate_bool_with_time(id.with("lift"), resp.hovered() && !dragged, 0.12);
    let lift = if resp.is_pointer_button_down_on() && !dragged { lift * 0.3 } else { lift };
    let icon = Rect::from_min_size(rect.min, vec2(TILE_WIDTH, ICON_HEIGHT));
    let mut painter = ui.painter().clone();
    if dragged {
        painter.multiply_opacity(0.35);
    }
    if selected {
        painter.rect_stroke(icon.expand(5.0), CornerRadius::same(12), Stroke::new(2.0, p.accent), StrokeKind::Outside);
    }
    let dark = ui.visuals().dark_mode;
    let face = match item.icon {
        Icon::Folder => paint_folder(&painter, icon, item.color, lift, dark),
        Icon::File => paint_file(&painter, icon, item.color, lift, dark),
        Icon::Server => paint_server(&painter, icon, item.color, lift, dark),
    };
    let letters: String = item.title.trim_start_matches("★ ").chars().take(2).collect::<String>().to_lowercase();
    let size = if item.icon == Icon::Folder { 0.27 } else { 0.22 };
    painter.text(face.center(), Align2::CENTER_CENTER, letters, FontId::proportional(TILE_WIDTH * size), shade(item.color, 0.3));
    if let Some(n) = badge {
        let c = pos2(icon.right() - 4.0, icon.top() + 9.0);
        painter.circle_filled(c, 9.0, p.sidebar);
        painter.circle_stroke(c, 9.0, Stroke::new(1.0, p.muted));
        painter.text(c, Align2::CENTER_CENTER, n.to_string(), FontId::proportional(11.0), ui.visuals().text_color());
    }
    let mut y = icon.bottom() + 8.0;
    for (text, font, color, rows) in [
        (item.title.as_str(), theme::bold(13.0), ui.visuals().text_color(), 2),
        (&item.place, FontId::proportional(11.0), p.muted, 1),
        (&item.what, FontId::proportional(11.0), p.accent, 1),
    ] {
        if text.is_empty() {
            continue;
        }
        let mut job = egui::text::LayoutJob::simple(text.to_string(), font, color, TILE_WIDTH);
        job.wrap.max_rows = rows;
        job.halign = Align::Center;
        let galley = painter.layout_job(job);
        let h = galley.size().y;
        painter.galley(pos2(rect.center().x, y), galley, color);
        y += h;
    }
    if resp.clicked() {
        return Some(Pick::Open(item.open.clone()));
    }
    let mut pick = None;
    egui::Popup::context_menu(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        pick = tile_menu(ui, item, cx, resp.id);
    });
    pick
}

fn hover_text(item: &Item, cx: &Cx) -> String {
    match (&item.id, &item.open) {
        (ItemId::Path(path), Open::Project(_)) => {
            let what = cx.contents.get(path).map(Contents::summary).unwrap_or_default();
            if what.is_empty() { path.display().to_string() } else { format!("{}\n{what}", path.display()) }
        }
        (ItemId::Path(path), _) => path.display().to_string(),
        _ => format!("Connect to {}", item.place),
    }
}

/// A tile's right-click menu: open, its group, and removing it (projects and files) or
/// editing it (saved connections).
fn tile_menu(ui: &mut egui::Ui, item: &Item, cx: &Cx, id: egui::Id) -> Option<Pick> {
    let p = theme::palette(ui);
    let mut pick = None;
    let mut choose = |ui: &mut egui::Ui, value: Pick| {
        pick = Some(value);
        ui.close();
    };
    if ui.button("Open").clicked() {
        choose(ui, Pick::Open(item.open.clone()));
    }
    if let ItemId::Path(path) = &item.id
        && ui.button("Copy path").clicked()
    {
        ui.ctx().copy_text(path.display().to_string());
        ui.close();
    }
    if matches!(item.id, ItemId::Last(_)) {
        return pick;
    }
    ui.separator();
    ui.label(RichText::new("Group").small().color(p.muted));
    for g in cx.groups {
        let current = item.group == *g;
        if ui.add(egui::Button::selectable(current, *g)).clicked() && !current {
            choose(ui, Pick::SetGroup(item.id.clone(), g.to_string()));
        }
    }
    if !item.group.is_empty() && ui.button("No group").clicked() {
        choose(ui, Pick::SetGroup(item.id.clone(), String::new()));
    }
    if let Some(name) = name_field(ui, id.with("new group"), "") {
        choose(ui, Pick::SetGroup(item.id.clone(), name));
    }
    ui.separator();
    match &item.id {
        ItemId::Conn(name) => {
            if ui.button("Edit connection…").clicked() {
                choose(ui, Pick::EditConnection(name.clone()));
            }
        }
        ItemId::Path(path) => {
            let label = if item.icon == Icon::Folder { "Remove from projects" } else { "Remove from recent files" };
            if ui.button(label).on_hover_text("The folder or file itself stays").clicked() {
                choose(ui, Pick::Forget(path.clone()));
            }
        }
        ItemId::Last(_) => {}
    }
    pick
}

/// A dashed area inviting a folder to be dropped on it, or a note that nothing matches.
fn empty_state(ui: &mut egui::Ui, filter: &str, total: usize) {
    let p = theme::palette(ui);
    if total > 0 {
        ui.label(RichText::new(format!("Nothing matches “{}”", filter.trim())).color(p.muted));
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 180.0), Sense::hover());
    let painter = ui.painter();
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
    painter.extend(egui::Shape::dashed_line(&corners, Stroke::new(1.5, p.muted), 6.0, 6.0));
    let text = ui.visuals().text_color();
    painter.text(rect.center() - vec2(0.0, 11.0), Align2::CENTER_CENTER, "Drop a project folder or a SQLite file here to open it", FontId::proportional(16.0), text);
    painter.text(rect.center() + vec2(0.0, 12.0), Align2::CENTER_CENTER, "or use the buttons above: projects, files and connections you open show here", FontId::proportional(12.5), p.muted);
}

/// Arrow keys move between the tiles by where they are, Enter opens the chosen one (the first
/// match while filtering), ⌘1 to ⌘9 open the first tiles, Escape continues without a database.
/// Nothing happens while another field or a menu has the keyboard.
fn keys(ctx: &egui::Context, st: &mut WelcomeState, shown: &[&Item]) -> Option<Pick> {
    let filter_id = egui::Id::new("welcome_filter");
    if ctx.memory(|m| m.focused().is_some_and(|f| f != filter_id)) || egui::Popup::is_any_open(ctx) {
        return None;
    }
    let consume = |m, k| ctx.input_mut(|i| i.consume_key(m, k));
    for (i, &k) in DIGITS.iter().enumerate().take(shown.len()) {
        if consume(Modifiers::COMMAND, k) {
            return Some(Pick::Open(shown[i].open.clone()));
        }
    }
    if st.selected.is_some_and(|s| s >= shown.len()) {
        st.selected = None;
    }
    if st.filter.is_empty() && consume(Modifiers::NONE, Key::Escape) {
        if st.selected.take().is_none() {
            return Some(Pick::Skip);
        }
        return None;
    }
    let sideways = st.filter.is_empty();
    for (key, allowed) in [(Key::ArrowLeft, sideways), (Key::ArrowRight, sideways), (Key::ArrowUp, true), (Key::ArrowDown, true)] {
        if allowed && !shown.is_empty() && consume(Modifiers::NONE, key) {
            let next = match st.selected {
                Some(i) if st.centers.len() == shown.len() => step(&st.centers, i, key),
                Some(i) => i,
                None => 0,
            };
            st.selected = Some(next);
            return None;
        }
    }
    let chosen = st.selected.or((!st.filter.trim().is_empty() && !shown.is_empty()).then_some(0))?;
    consume(Modifiers::NONE, Key::Enter).then(|| Pick::Open(shown[chosen].open.clone()))
}

/// The tile `key` moves to from tile `from`, by the tiles' `centers`. Left and right stay in
/// the row, going on to the previous or next tile at its ends; up and down go to the nearest
/// tile in the closest row above or below.
fn step(centers: &[Pos2], from: usize, key: Key) -> usize {
    let here = centers[from];
    let same_row = |c: Pos2| (c.y - here.y).abs() < TILE_HEIGHT / 2.0;
    let nearest = |score: &dyn Fn(Pos2) -> Option<f32>| {
        centers.iter().enumerate().filter_map(|(i, &c)| score(c).map(|d| (i, d))).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i)
    };
    match key {
        Key::ArrowLeft => nearest(&|c| (same_row(c) && c.x < here.x - 1.0).then_some(here.x - c.x)).unwrap_or(from.saturating_sub(1)),
        Key::ArrowRight => nearest(&|c| (same_row(c) && c.x > here.x + 1.0).then_some(c.x - here.x)).unwrap_or((from + 1).min(centers.len() - 1)),
        Key::ArrowUp | Key::ArrowDown => {
            let down = key == Key::ArrowDown;
            let ahead = |c: Pos2| if down { c.y > here.y + TILE_HEIGHT / 2.0 } else { c.y < here.y - TILE_HEIGHT / 2.0 };
            let Some(row) = nearest(&|c| ahead(c).then_some((c.y - here.y).abs())) else { return from };
            let row_y = centers[row].y;
            nearest(&|c| ((c.y - row_y).abs() < 1.0).then_some((c.x - here.x).abs())).unwrap_or(from)
        }
        _ => from,
    }
}

/// While files are dragged over the window, a veil saying they will open.
fn drop_overlay(ui: &egui::Ui, screen: Rect, p: theme::Palette) {
    if ui.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("welcome drop")));
    let area = screen.shrink(MARGIN / 2.0);
    painter.rect(area, CornerRadius::same(12), p.content.gamma_multiply(0.92), Stroke::new(2.0, p.accent), StrokeKind::Inside);
    let text = ui.visuals().text_color();
    painter.text(area.center(), Align2::CENTER_CENTER, "Drop a project folder, a .dbs schema or a SQLite file", FontId::proportional(20.0), text);
}

/// A color picked from the path, so it stays the same across launches.
fn path_color(path: &Path) -> Color32 {
    let sum: usize = path.as_os_str().as_encoded_bytes().iter().map(|&b| b as usize).sum();
    COLORS[sum % COLORS.len()]
}

/// A server connection without a color of its own: blue for PostgreSQL, orange for MySQL.
fn kind_color(kind: Kind) -> Color32 {
    match kind {
        Kind::Postgres => Color32::from_rgb(0x8c, 0xb4, 0xe8),
        Kind::Mysql => Color32::from_rgb(0xf5, 0xb0, 0x70),
        Kind::Sqlite => COLORS[3],
    }
}

/// The folder holding `path`, with the home folder as `~`.
fn parent_label(path: &Path) -> String {
    let Some(parent) = path.parent() else { return String::new() };
    match std::env::home_dir().and_then(|home| parent.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(inside) if inside.as_os_str().is_empty() => "~".to_string(),
        Some(inside) => format!("~{}{}", std::path::MAIN_SEPARATOR, inside.display()),
        None => parent.display().to_string(),
    }
}

fn shade(color: Color32, factor: f32) -> Color32 {
    color.lerp_to_gamma(Color32::BLACK, 1.0 - factor)
}

/// A soft shadow under `rect` that grows as `lift` (0 to 1) raises the icon.
fn shadow(painter: &egui::Painter, rect: Rect, radius: f32, lift: f32, dark: bool) {
    let alpha = if dark { 110.0 } else { 45.0 };
    let shadow = egui::epaint::Shadow {
        offset: [0, (2.0 + 6.0 * lift) as i8],
        blur: (8.0 + 8.0 * lift) as u8,
        spread: 0,
        color: Color32::from_black_alpha((alpha * (1.0 - 0.3 * lift)) as u8),
    };
    painter.add(shadow.as_shape(rect, radius));
}

/// A rounded folder filling `icon` in `color`: a back sheet with a tab on the top left behind
/// a lower front sheet: a project. Returns the front sheet, where the letters go.
fn paint_folder(painter: &egui::Painter, icon: Rect, color: Color32, lift: f32, dark: bool) -> Rect {
    const RADIUS: f32 = 9.0;
    let icon = icon.translate(vec2(0.0, -3.0 * lift));
    let back = Rect::from_min_max(pos2(icon.left(), icon.top() + icon.height() * 0.1), icon.max);
    let front = Rect::from_min_max(pos2(icon.left(), icon.top() + icon.height() * 0.26), icon.max);
    shadow(painter, front, RADIUS, lift, dark);
    let tab_right = icon.left() + icon.width() * 0.42;
    let slope = (back.top() - icon.top()) * 1.2;
    let tab_bottom = back.top() + RADIUS * 1.5;
    let tab = [icon.min, pos2(tab_right, icon.top()), pos2(tab_right + slope, back.top()), pos2(tab_right + slope, tab_bottom), pos2(icon.left(), tab_bottom)];
    let back_shades = (shade(color, 0.72), shade(color, 0.6));
    gradient(painter, &rounded(&tab, RADIUS / 2.0), (icon.top(), back.bottom()), back_shades);
    gradient(painter, &rounded(&corners(back), RADIUS), (icon.top(), back.bottom()), back_shades);
    gradient(painter, &rounded(&corners(front), RADIUS), (front.top(), front.bottom()), (color.lerp_to_gamma(Color32::WHITE, 0.3), shade(color, 0.86)));
    let inset = RADIUS * 0.8;
    painter.hline((front.left() + inset)..=(front.right() - inset), front.top() + 1.0, Stroke::new(1.0, Color32::from_white_alpha(90)));
    front
}

/// A page with its top right corner folded over, standing in `icon`: a SQLite file. Returns
/// where the letters go.
fn paint_file(painter: &egui::Painter, icon: Rect, color: Color32, lift: f32, dark: bool) -> Rect {
    const RADIUS: f32 = 7.0;
    let icon = icon.translate(vec2(0.0, -3.0 * lift));
    let page = Rect::from_center_size(icon.center(), vec2(icon.height() * 0.8, icon.height()));
    let fold = page.width() * 0.3;
    shadow(painter, page, RADIUS, lift, dark);
    let outline = [page.left_top(), pos2(page.right() - fold, page.top()), pos2(page.right(), page.top() + fold), page.right_bottom(), page.left_bottom()];
    gradient(painter, &rounded(&outline, RADIUS), (page.top(), page.bottom()), (color.lerp_to_gamma(Color32::WHITE, 0.3), shade(color, 0.86)));
    let ear = [pos2(page.right() - fold, page.top()), pos2(page.right(), page.top() + fold), pos2(page.right() - fold, page.top() + fold)];
    painter.add(egui::Shape::convex_polygon(rounded(&ear, 2.0), shade(color, 0.68), Stroke::NONE));
    // Two short lines near the bottom, like a page's text.
    let line = Stroke::new(2.0, shade(color, 0.7).gamma_multiply(0.6));
    for (i, w) in [0.55, 0.4].into_iter().enumerate() {
        let y = page.bottom() - page.height() * 0.2 + i as f32 * 6.0;
        painter.hline((page.center().x - page.width() * w / 2.0)..=(page.center().x + page.width() * w / 2.0), y, line);
    }
    Rect::from_center_size(page.center() - vec2(0.0, page.height() * 0.06), page.size() * 0.6)
}

/// A database cylinder standing in `icon`: a server connection. Returns where the letters go.
fn paint_server(painter: &egui::Painter, icon: Rect, color: Color32, lift: f32, dark: bool) -> Rect {
    let icon = icon.translate(vec2(0.0, -3.0 * lift));
    let w = icon.height() * 1.05;
    let ry = icon.height() * 0.13;
    let body = Rect::from_min_max(pos2(icon.center().x - w / 2.0, icon.top() + ry), pos2(icon.center().x + w / 2.0, icon.bottom() - ry));
    shadow(painter, body.expand2(vec2(0.0, ry * 0.5)), w * 0.2, lift, dark);
    // Points along the bottom half of an ellipse centered at height `cy`, right to left.
    let arc = |cy: f32| -> Vec<Pos2> {
        (0..=16).map(|i| std::f32::consts::PI * i as f32 / 16.0).map(|a| pos2(body.center().x + w / 2.0 * a.cos(), cy + ry * a.sin())).collect()
    };
    // The side and the bottom's curve, as one convex outline.
    let mut outline = vec![body.left_top(), body.right_top()];
    outline.extend(arc(body.bottom()));
    gradient(painter, &outline, (body.top(), body.bottom() + ry), (color.lerp_to_gamma(Color32::WHITE, 0.2), shade(color, 0.8)));
    // Bands between the disks.
    let band = Stroke::new(1.5, shade(color, 0.62));
    for t in [1.0 / 3.0, 2.0 / 3.0] {
        painter.add(egui::Shape::line(arc(body.top() + body.height() * t), band));
    }
    let top = pos2(body.center().x, body.top());
    painter.add(egui::epaint::EllipseShape::filled(top, vec2(w / 2.0, ry), color.lerp_to_gamma(Color32::WHITE, 0.45)));
    painter.add(egui::epaint::EllipseShape::stroke(top, vec2(w / 2.0, ry), Stroke::new(1.0, shade(color, 0.8))));
    Rect::from_center_size(pos2(body.center().x, body.center().y + ry * 0.6), vec2(w * 0.6, body.height() * 0.5))
}

fn corners(r: Rect) -> [Pos2; 4] {
    [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()]
}

/// The outline of the convex polygon `points` (clockwise) with its corners rounded.
fn rounded(points: &[Pos2], radius: f32) -> Vec<Pos2> {
    const STEPS: usize = 8;
    let n = points.len();
    let mut out = Vec::with_capacity(n * (STEPS + 1));
    for (i, &corner) in points.iter().enumerate() {
        let prev = points[(i + n - 1) % n];
        let next = points[(i + 1) % n];
        let start = corner + (prev - corner).normalized() * radius.min(corner.distance(prev) / 2.0);
        let end = corner + (next - corner).normalized() * radius.min(corner.distance(next) / 2.0);
        for s in 0..=STEPS {
            let t = s as f32 / STEPS as f32;
            out.push(start.lerp(corner, t).lerp(corner.lerp(end, t), t));
        }
    }
    out
}

/// Fills the convex `outline` fading from the first of `shades` at the top of `span` to the
/// second at its bottom, over an anti-aliased fill in the middle color.
fn gradient(painter: &egui::Painter, outline: &[Pos2], (top, bottom): (f32, f32), shades: (Color32, Color32)) {
    let at = |y: f32| shades.0.lerp_to_gamma(shades.1, (y - top) / (bottom - top));
    let center = outline.iter().fold(Pos2::ZERO, |s, p| s + p.to_vec2()) / outline.len() as f32;
    painter.add(egui::Shape::convex_polygon(outline.to_vec(), at(center.y), Stroke::NONE));
    let mut mesh = egui::epaint::Mesh::default();
    mesh.colored_vertex(center, at(center.y));
    for &p in outline {
        let inside = p + (center - p).normalized() * 0.5;
        mesh.colored_vertex(inside, at(inside.y));
    }
    let n = outline.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    painter.add(mesh);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, group: &str, last_used: u64, favorite: bool) -> Item {
        Item {
            id: ItemId::Conn(title.into()),
            icon: Icon::Server,
            title: title.into(),
            place: String::new(),
            what: String::new(),
            color: Color32::WHITE,
            group: group.into(),
            favorite,
            last_used,
            open: Open::Db(format!("@{title}")),
        }
    }

    #[test]
    fn sections_by_group_then_ungrouped_favorites_and_recent_first() {
        let all = vec![item("a", "", 1, false), item("b", "Zeta", 5, false), item("c", "acme", 2, false), item("d", "", 9, false), item("e", "", 0, true)];
        let s = sections(all, "");
        let names: Vec<(Option<&str>, Vec<&str>)> = s.iter().map(|s| (s.title.as_deref(), s.items.iter().map(|i| i.title.as_str()).collect())).collect();
        assert_eq!(names, [(Some("acme"), vec!["c"]), (Some("Zeta"), vec!["b"]), (Some(UNGROUPED), vec!["e", "d", "a"])]);
        // Nothing grouped: one section without a card; the filter also looks at the group.
        let s = sections(vec![item("a", "", 1, false)], "");
        assert_eq!(s[0].title, None);
        assert_eq!(sections(vec![item("a", "Acme", 1, false), item("b", "", 1, false)], "acm").len(), 1);
    }

    #[test]
    fn arrows_move_by_position() {
        // Two cards: three tiles in a row, then two in the next card.
        let c = [pos2(0.0, 0.0), pos2(150.0, 0.0), pos2(300.0, 0.0), pos2(0.0, 300.0), pos2(150.0, 300.0)];
        assert_eq!(step(&c, 0, Key::ArrowRight), 1);
        assert_eq!(step(&c, 2, Key::ArrowRight), 3, "past the end of a row: the next tile");
        assert_eq!(step(&c, 3, Key::ArrowLeft), 2);
        assert_eq!(step(&c, 2, Key::ArrowDown), 4, "the nearest in the row below");
        assert_eq!(step(&c, 4, Key::ArrowUp), 1);
        assert_eq!(step(&c, 1, Key::ArrowUp), 1);
        assert_eq!(step(&c, 4, Key::ArrowRight), 4);
    }
}
