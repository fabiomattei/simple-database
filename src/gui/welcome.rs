//! The welcome screen, over the whole window: the app's name and buttons to start something, a
//! filter field, the recent projects as folder tiles and the saved connections in a column on
//! the right. A project is a folder: opening it loads its schema file, its Laravel migrations
//! (History tab) and the database last used with it (`projects.rs`). Folders and files dropped
//! on the window open.

use crate::{App, Dialog, Tab, conns, db_title, theme};
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, FontId, Key, Layout, Modifiers, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use simple_database::connections::{Connection, SafeMode, Store};
use simple_database::projects::{self, Contents, Project, Projects};
use simple_database::sqlite;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MARGIN: f32 = 28.0;
const COLUMN_WIDTH: f32 = 320.0;
const COLUMN_GAP: f32 = 28.0;
const TILE_WIDTH: f32 = 124.0;
/// Width over height of the folder is the golden ratio.
const FOLDER_HEIGHT: f32 = TILE_WIDTH / 1.618;
/// Two rows of the name, the folder holding it and what the project holds.
const TILE_TEXT: f32 = 70.0;
const TILE_GAP: Vec2 = vec2(26.0, 18.0);
/// Light enough for the folders' dark letters on every color.
const FOLDER_COLORS: [Color32; 9] = [
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
const DIGITS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

/// What the welcome screen keeps between frames.
pub(crate) struct WelcomeState {
    /// The database of the last session, offered when it isn't a saved connection.
    pub(crate) last: Option<String>,
    filter: String,
    /// Chosen with the arrow keys: an index into the projects, then the connections shown.
    selected: Option<usize>,
    /// What each project folder holds, looked at once per showing.
    contents: HashMap<PathBuf, Contents>,
    /// Tiles per row last frame, for moving up and down.
    columns: usize,
}

impl WelcomeState {
    pub(crate) fn new(last: Option<String>) -> Box<WelcomeState> {
        Box::new(WelcomeState { last, filter: String::new(), selected: None, contents: HashMap::new(), columns: 1 })
    }
}

/// What was picked on the welcome screen.
enum Pick {
    Project(PathBuf),
    Db(String),
    Forget(PathBuf),
    Dropped(Vec<PathBuf>),
    OpenProject,
    NewConnection,
    OpenSqlite,
    NewSqlite,
    Skip,
}

/// A connection row: the last session's database, or a saved connection.
struct Row<'a> {
    target: String,
    title: String,
    subtitle: String,
    tint: Color32,
    /// The heading above it, when it starts a section.
    section: Option<&'static str>,
    hover: String,
    saved: Option<&'a Connection>,
}

fn matches(needle: &str, texts: &[&str]) -> bool {
    needle.is_empty() || texts.iter().any(|t| t.to_lowercase().contains(needle))
}

/// The projects that show, most recent first: their folder still exists and they match `needle`.
fn shown_projects<'a>(projects: &'a Projects, contents: &HashMap<PathBuf, Contents>, needle: &str) -> Vec<&'a Project> {
    projects
        .list
        .iter()
        .filter(|p| p.path.is_dir())
        .filter(|p| matches(needle, &[&p.path.to_string_lossy(), &contents.get(&p.path).map(Contents::summary).unwrap_or_default()]))
        .collect()
}

fn connection_rows<'a>(conns: &'a Store, last: Option<&str>, needle: &str, accent: Color32) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    // The last session, unless it is one of the saved connections listed anyway.
    if let Some(t) = last
        && !t.strip_prefix('@').is_some_and(|n| conns.get(n).is_some())
        && matches(needle, &[t])
    {
        rows.push(Row { target: t.to_string(), title: db_title(t), subtitle: t.to_string(), tint: accent, section: Some("Last session"), hover: "Reopen".into(), saved: None });
    }
    let mut list: Vec<&Connection> = conns.connections.iter().filter(|c| matches(needle, &[&c.name, &c.summary()])).collect();
    list.sort_by_key(|c| (!c.favorite, std::cmp::Reverse(c.last_used), c.name.to_lowercase()));
    let mut section = None;
    for c in list {
        let title = if c.favorite { "Favorites" } else { "Saved" };
        let mut subtitle = format!("{} · {}", c.kind.label(), c.summary());
        if last == Some(format!("@{}", c.name).as_str()) {
            subtitle = format!("Last used · {subtitle}");
        }
        if c.safe_mode != SafeMode::Off {
            subtitle = format!("{subtitle} · {}", c.safe_mode.label());
        }
        rows.push(Row {
            target: format!("@{}", c.name),
            title: format!("{}{}", if c.favorite { "★ " } else { "" }, c.name),
            subtitle,
            tint: conns::to_color32(c.color).unwrap_or(accent),
            section: (section != Some(title)).then_some(title),
            hover: format!("Connect to {}", c.summary()),
            saved: Some(c),
        });
        section = Some(title);
    }
    rows
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
        match pick {
            None => {}
            Some(Pick::Project(path)) => self.open_project(&path),
            Some(Pick::Db(target)) => {
                self.dialog = None;
                self.project = None;
                self.open_db(&target);
            }
            Some(Pick::Forget(path)) => {
                if let Err(e) = self.projects.forget(&path) {
                    self.set_status(format!("Cannot save recent projects: {e:#}"), true);
                }
            }
            Some(Pick::Dropped(paths)) => self.open_dropped(paths),
            Some(Pick::OpenProject) => self.pick_project(),
            Some(Pick::NewConnection) => self.connections_dialog(true),
            Some(Pick::OpenSqlite) => {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                    .add_filter("All files", &["*"])
                    .pick_file()
                {
                    self.dialog = None;
                    self.project = None;
                    self.open_db(&p.to_string_lossy());
                }
            }
            Some(Pick::NewSqlite) => {
                if let Some(p) = rfd::FileDialog::new().add_filter("SQLite database", &["db"]).set_file_name("app.db").save_file() {
                    self.dialog = None;
                    self.project = None;
                    self.open_db_at(&p.to_string_lossy(), Some(Tab::Migrate));
                }
            }
            Some(Pick::Skip) => self.dialog = None,
        }
    }

    /// Shows the welcome screen.
    pub(crate) fn show_welcome(&mut self) {
        let last = self.db.as_ref().map(|d| d.label.clone());
        if let Ok(fresh) = Projects::load() {
            self.projects = fresh;
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
        let name = project.name();
        let message = if notes.is_empty() { format!("Opened project {name}") } else { format!("Opened project {name}; {}", notes.join("; ")) };
        if found.db.is_none() {
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
    let shown = shown_projects(projects, &st.contents, &needle);
    let rows = connection_rows(conns, st.last.as_deref(), &needle, p.accent);
    let mut pick = keys(ctx, st, &shown, &rows);
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
            ui.add_space(22.0);
            let top = ui.cursor().top();
            let footer = 22.0;
            let body = Rect::from_min_max(pos2(inner.left(), top), pos2(inner.right(), inner.bottom() - footer));
            let column_w = COLUMN_WIDTH.min(body.width() * 0.45);
            let right = Rect::from_min_max(pos2(body.right() - column_w, body.top()), body.max);
            let left = Rect::from_min_max(body.min, pos2(right.left() - COLUMN_GAP, body.bottom()));
            ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
                heading(ui, "Projects", shown.len());
                if let Some(pk) = project_grid(ui, st, &shown) {
                    pick = Some(pk);
                }
            });
            ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| {
                heading(ui, "Connections", rows.iter().filter(|r| r.saved.is_some()).count());
                if let Some(pk) = connection_column(ui, st, &rows, shown.len(), conns.connections.is_empty()) {
                    pick = Some(pk);
                }
            });
            let hint = "⌘1–⌘9 open the first projects · Escape continues without a database · Help → Example schema shows a sample schema";
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
                .hint_text("Filter projects and connections — arrows to choose, Enter to open")
                .font(FontId::proportional(14.0))
                .desired_width(380.0),
        );
    });
    id
}

/// `title` in spaced capitals with `count` in a pill, and a thin rule to the right edge.
fn heading(ui: &mut egui::Ui, title: &str, count: usize) {
    let p = theme::palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
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
    ui.add_space(8.0);
}

fn project_grid(ui: &mut egui::Ui, st: &mut WelcomeState, shown: &[&Project]) -> Option<Pick> {
    if shown.is_empty() {
        empty_projects(ui, &st.filter);
        st.columns = 1;
        return None;
    }
    let width = ui.available_width();
    let columns = (((width + TILE_GAP.x) / (TILE_WIDTH + TILE_GAP.x)) as usize).max(1);
    st.columns = columns;
    let shortcuts = ui.input(|i| i.modifiers.command);
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("welcome_projects").auto_shrink([false, false]).show(ui, |ui| {
        let rows = shown.len().div_ceil(columns);
        let height = rows as f32 * (FOLDER_HEIGHT + TILE_TEXT) + rows.saturating_sub(1) as f32 * TILE_GAP.y + 8.0;
        let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        for (i, project) in shown.iter().enumerate() {
            let at = area.min + vec2((i % columns) as f32 * (TILE_WIDTH + TILE_GAP.x), (i / columns) as f32 * (FOLDER_HEIGHT + TILE_TEXT + TILE_GAP.y) + 4.0);
            let rect = Rect::from_min_size(at, vec2(TILE_WIDTH, FOLDER_HEIGHT + TILE_TEXT));
            let summary = st.contents.get(&project.path).map(Contents::summary).unwrap_or_default();
            let badge = (shortcuts && i < 9).then_some(i + 1);
            if let Some(pk) = tile(ui, rect, project, &summary, st.selected == Some(i), badge) {
                pick = Some(pk);
            }
        }
    });
    pick
}

/// One project: a large folder in its color with the first letters of its name, and below
/// the name, the folder holding it and what it holds.
fn tile(ui: &mut egui::Ui, rect: Rect, project: &Project, summary: &str, selected: bool, badge: Option<usize>) -> Option<Pick> {
    let p = theme::palette(ui);
    let id = ui.id().with(("welcome tile", &project.path));
    let resp = ui.interact(rect, id, Sense::click());
    if selected {
        resp.scroll_to_me(None);
    }
    let mut hover = project.path.display().to_string();
    if !summary.is_empty() {
        hover = format!("{hover}\n{summary}");
    }
    let resp = resp.on_hover_text(hover);
    let lift = ui.ctx().animate_bool_with_time(id.with("lift"), resp.hovered(), 0.12);
    let lift = if resp.is_pointer_button_down_on() { lift * 0.3 } else { lift };
    let color = folder_color(&project.path);
    let icon = Rect::from_min_size(rect.min, vec2(TILE_WIDTH, FOLDER_HEIGHT));
    let painter = ui.painter().clone();
    if selected {
        painter.rect_stroke(icon.expand(5.0), CornerRadius::same(12), Stroke::new(2.0, p.accent), StrokeKind::Outside);
    }
    let front = paint_folder(&painter, icon, color, lift, ui.visuals().dark_mode);
    let name = project.name();
    painter.text(front.center(), Align2::CENTER_CENTER, name.chars().take(2).collect::<String>().to_lowercase(), FontId::proportional(TILE_WIDTH * 0.27), shade(color, 0.3));
    if let Some(n) = badge {
        let c = pos2(icon.right() - 4.0, icon.top() + 9.0);
        painter.circle_filled(c, 9.0, p.sidebar);
        painter.circle_stroke(c, 9.0, Stroke::new(1.0, p.muted));
        painter.text(c, Align2::CENTER_CENTER, n.to_string(), FontId::proportional(11.0), ui.visuals().text_color());
    }
    let mut y = icon.bottom() + 8.0;
    for (text, font, color, rows) in [
        (name.as_str(), theme::bold(13.0), ui.visuals().text_color(), 2),
        (&parent_label(&project.path), FontId::proportional(11.0), p.muted, 1),
        (summary, FontId::proportional(11.0), p.accent, 1),
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
        return Some(Pick::Project(project.path.clone()));
    }
    let mut pick = None;
    resp.context_menu(|ui| {
        if ui.button("Open").clicked() {
            pick = Some(Pick::Project(project.path.clone()));
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(project.path.display().to_string());
        }
        ui.separator();
        if ui.button("Remove from projects").on_hover_text("The folder itself stays").clicked() {
            pick = Some(Pick::Forget(project.path.clone()));
        }
    });
    pick
}

/// A dashed area inviting a folder to be dropped on it, or a note that nothing matches.
fn empty_projects(ui: &mut egui::Ui, filter: &str) {
    let p = theme::palette(ui);
    if !filter.trim().is_empty() {
        ui.label(RichText::new(format!("No projects match “{}”", filter.trim())).color(p.muted));
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 180.0), Sense::hover());
    let painter = ui.painter();
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
    painter.extend(egui::Shape::dashed_line(&corners, Stroke::new(1.5, p.muted), 6.0, 6.0));
    let text = ui.visuals().text_color();
    painter.text(rect.center() - vec2(0.0, 11.0), Align2::CENTER_CENTER, "Drop a project folder here to open it", FontId::proportional(16.0), text);
    painter.text(rect.center() + vec2(0.0, 12.0), Align2::CENTER_CENTER, "or use Open Project… above: its schema, migrations and database are remembered", FontId::proportional(12.5), p.muted);
}

fn connection_column(ui: &mut egui::Ui, st: &WelcomeState, rows: &[Row], first: usize, none_saved: bool) -> Option<Pick> {
    let p = theme::palette(ui);
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("welcome_conns").auto_shrink([false, false]).show(ui, |ui| {
        for (i, row) in rows.iter().enumerate() {
            if let Some(s) = row.section {
                if i > 0 {
                    ui.add_space(6.0);
                }
                theme::section(ui, s);
            }
            let glyph = if row.saved.is_some() { theme::Glyph::Database } else { theme::Glyph::Arrow };
            let resp = theme::card(ui, glyph, row.tint, &row.title, &row.subtitle);
            if st.selected == Some(first + i) {
                ui.painter().rect_stroke(resp.rect, CornerRadius::same(10), Stroke::new(2.0, p.accent), StrokeKind::Inside);
                resp.scroll_to_me(None);
            }
            if resp.on_hover_text(&row.hover).clicked() {
                pick = Some(Pick::Db(row.target.clone()));
            }
        }
        if none_saved {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No saved connections yet").font(theme::bold(15.0)));
                ui.label(RichText::new("New Connection… above adds one.").color(p.muted));
            });
        } else if rows.is_empty() {
            ui.label(RichText::new("No connection matches the filter.").color(p.muted));
        }
    });
    pick
}

/// Arrow keys move through the projects and on into the connections, Enter opens the chosen
/// one (the first match while filtering), ⌘1 to ⌘9 open the first projects, Escape continues
/// without a database. Nothing happens while another field or a menu has the keyboard.
fn keys(ctx: &egui::Context, st: &mut WelcomeState, shown: &[&Project], rows: &[Row]) -> Option<Pick> {
    let filter_id = egui::Id::new("welcome_filter");
    if ctx.memory(|m| m.focused().is_some_and(|f| f != filter_id)) || egui::Popup::is_any_open(ctx) {
        return None;
    }
    let consume = |m, k| ctx.input_mut(|i| i.consume_key(m, k));
    for (i, &k) in DIGITS.iter().enumerate().take(shown.len()) {
        if consume(Modifiers::COMMAND, k) {
            return Some(Pick::Project(shown[i].path.clone()));
        }
    }
    let total = shown.len() + rows.len();
    if st.selected.is_some_and(|s| s >= total) {
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
        if allowed && total > 0 && consume(Modifiers::NONE, key) {
            st.selected = Some(st.selected.map_or(0, |i| step(i, key, shown.len(), rows.len(), st.columns)));
            return None;
        }
    }
    let chosen = st.selected.or((!st.filter.trim().is_empty() && total > 0).then_some(0))?;
    if !consume(Modifiers::NONE, Key::Enter) {
        return None;
    }
    Some(match shown.get(chosen) {
        Some(project) => Pick::Project(project.path.clone()),
        None => Pick::Db(rows[chosen - shown.len()].target.clone()),
    })
}

/// Where `key` moves the selection from `i`: through a grid of `projects` tiles `columns`
/// wide, then down a column of `conns` rows on its right.
fn step(i: usize, key: Key, projects: usize, conns: usize, columns: usize) -> usize {
    let columns = columns.max(1);
    if i < projects {
        let last_row = (projects - 1) / columns;
        match key {
            Key::ArrowLeft if !i.is_multiple_of(columns) => i - 1,
            Key::ArrowRight if i + 1 < projects && !(i + 1).is_multiple_of(columns) => i + 1,
            Key::ArrowRight if conns > 0 => projects,
            Key::ArrowUp if i >= columns => i - columns,
            Key::ArrowDown if i + columns < projects => i + columns,
            Key::ArrowDown if i / columns < last_row => projects - 1,
            _ => i,
        }
    } else {
        let j = i - projects;
        match key {
            Key::ArrowUp if j > 0 => i - 1,
            Key::ArrowDown if j + 1 < conns => i + 1,
            Key::ArrowLeft if projects > 0 => projects.min(columns) - 1,
            _ => i,
        }
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
fn folder_color(path: &Path) -> Color32 {
    let sum: usize = path.as_os_str().as_encoded_bytes().iter().map(|&b| b as usize).sum();
    FOLDER_COLORS[sum % FOLDER_COLORS.len()]
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

/// A rounded folder filling `icon` in `color`: a back sheet with a tab on the top left behind
/// a lower front sheet, over a shadow that grows as `lift` (0 to 1) raises it. Returns the
/// front sheet, where the letters go.
fn paint_folder(painter: &egui::Painter, icon: Rect, color: Color32, lift: f32, dark: bool) -> Rect {
    const RADIUS: f32 = 9.0;
    let icon = icon.translate(vec2(0.0, -3.0 * lift));
    let back = Rect::from_min_max(pos2(icon.left(), icon.top() + icon.height() * 0.1), icon.max);
    let front = Rect::from_min_max(pos2(icon.left(), icon.top() + icon.height() * 0.26), icon.max);
    let alpha = if dark { 110.0 } else { 45.0 };
    let shadow = egui::epaint::Shadow {
        offset: [0, (2.0 + 6.0 * lift) as i8],
        blur: (8.0 + 8.0 * lift) as u8,
        spread: 0,
        color: Color32::from_black_alpha((alpha * (1.0 - 0.3 * lift)) as u8),
    };
    painter.add(shadow.as_shape(front, RADIUS));
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

    #[test]
    fn arrows_move_through_the_grid_then_the_connections() {
        // Five projects three wide, then two connections.
        let s = |i, k| step(i, k, 5, 2, 3);
        assert_eq!(s(0, Key::ArrowRight), 1);
        assert_eq!(s(2, Key::ArrowRight), 5, "the end of a row goes to the connections");
        assert_eq!(s(0, Key::ArrowLeft), 0);
        assert_eq!(s(1, Key::ArrowDown), 4);
        assert_eq!(s(2, Key::ArrowDown), 4, "into the short last row");
        assert_eq!(s(4, Key::ArrowDown), 4);
        assert_eq!(s(4, Key::ArrowUp), 1);
        assert_eq!(s(5, Key::ArrowDown), 6);
        assert_eq!(s(6, Key::ArrowDown), 6);
        assert_eq!(s(6, Key::ArrowUp), 5);
        assert_eq!(s(5, Key::ArrowLeft), 2);
        // Without projects the arrows stay in the connections.
        assert_eq!(step(0, Key::ArrowLeft, 0, 2, 3), 0);
    }
}
