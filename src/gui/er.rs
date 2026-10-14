//! Interactive entity-relationship diagram: drag tables, pan, zoom, hide tables,
//! export as PNG (what the canvas shows) or SVG (every visible table). When
//! editable, it also asks for schema changes through `ErAction`s.

use crate::er_layout;
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use simple_database::schema::{Schema, Table};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;

const ROW_H: f32 = 20.0;
const HEADER_H: f32 = 26.0;
const PAD_X: f32 = 10.0;
pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 4.0;

#[derive(Default)]
pub struct ErView {
    /// Top-left corner of each table box, in world coordinates.
    positions: HashMap<String, Pos2>,
    pan: Vec2,
    zoom: f32,
    dragging: Option<String>,
    /// Table highlighted from outside (e.g. the sidebar).
    pub selected: Option<String>,
    /// Rows per table of the open database, shown in the box titles.
    pub row_counts: HashMap<String, i64>,
    fit_requested: bool,
    /// Tables left out of the diagram and its exports.
    hidden: HashSet<String>,
    /// Canvas and box sizes (world units) from the last frame.
    canvas: Option<Rect>,
    sizes: HashMap<String, Vec2>,
    /// Table, column and world position under the pointer when the context menu opened.
    context_table: Option<String>,
    context_column: Option<String>,
    context_world: Option<Pos2>,
    /// A relationship being drawn from this (table, column)'s handle.
    linking: Option<(String, String)>,
    png_request: Option<PathBuf>,
    /// A screenshot is on its way: keep hover effects and the hint off the canvas.
    capturing: bool,
    png_result: Option<Result<PathBuf, String>>,
}

/// A schema change asked for on the canvas, applied by the caller.
pub enum ErAction {
    EditTable(String),
    AddColumn(String),
    /// A new table whose top-left corner goes here (world coordinates).
    NewTable(Pos2),
    DeleteTable(String),
    DeleteColumn { table: String, column: String },
    /// Make `table.column` a foreign key to `target.target_column`.
    Link { table: String, column: String, target: String, target_column: String },
    Unlink { table: String, column: String },
    /// Show the table's rows in the Data tab.
    OpenData(String),
}

/// Sent with the screenshot command, returned with the screenshot.
struct PngCapture {
    path: PathBuf,
    canvas: Rect,
    pixels_per_point: f32,
}

/// A relationship line in world coordinates: a cubic Bézier from the child
/// column to the parent column, plus which way each end leaves its box.
struct Edge {
    points: [Pos2; 4],
    child_dir: f32,
    parent_dir: f32,
    nullable: bool,
    child: String,
    parent: String,
}

struct BoxLayout {
    rect: Rect,
    /// World y of each column's row center, by column name.
    rows: HashMap<String, f32>,
}

impl ErView {
    /// Keep a renamed table's position, visibility and selection.
    pub fn rename(&mut self, old: &str, new: &str) {
        if let Some(p) = self.positions.remove(old) {
            self.positions.insert(new.to_string(), p);
        }
        if self.hidden.remove(old) {
            self.hidden.insert(new.to_string());
        }
        if self.selected.as_deref() == Some(old) {
            self.selected = Some(new.to_string());
        }
    }

    /// Put a table's top-left corner at `pos` (world coordinates).
    pub fn place(&mut self, table: &str, pos: Pos2) {
        self.positions.insert(table.to_string(), pos);
    }

    pub fn reset_layout(&mut self) {
        self.positions.clear();
        self.fit_requested = true;
    }

    pub fn request_fit(&mut self) {
        self.fit_requested = true;
    }

    pub fn zoom(&self) -> f32 {
        if self.zoom == 0.0 { 1.0 } else { self.zoom }
    }

    /// Zoom by `factor` around the canvas center.
    pub fn zoom_by(&mut self, factor: f32) {
        if let Some(canvas) = self.canvas
            && self.zoom != 0.0
        {
            self.zoom_around(canvas, canvas.center(), factor);
        }
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom_by(zoom / self.zoom());
    }

    fn zoom_around(&mut self, canvas: Rect, screen: Pos2, factor: f32) {
        let world = (screen - canvas.min - self.pan) / self.zoom;
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan = screen - canvas.min - world * self.zoom;
    }

    pub fn is_hidden(&self, table: &str) -> bool {
        self.hidden.contains(table)
    }

    pub fn set_hidden(&mut self, table: &str, hidden: bool) {
        if hidden {
            self.hidden.insert(table.to_string());
        } else {
            self.hidden.remove(table);
        }
    }

    pub fn show_all(&mut self) {
        self.hidden.clear();
    }

    pub fn hide_all(&mut self, schema: &Schema) {
        self.hidden.extend(schema.tables.iter().map(|t| t.name.clone()));
    }

    /// Show only `table` and the tables it references or is referenced by.
    pub fn show_related(&mut self, schema: &Schema, table: &str) {
        let Some(t) = schema.table(table) else { return };
        let mut keep: HashSet<&str> = HashSet::new();
        for fk in t.all_foreign_keys() {
            if let Some(parent) = schema.table(&fk.table) {
                keep.insert(&parent.name);
            }
        }
        keep.insert(table);
        for other in &schema.tables {
            if other.all_foreign_keys().iter().any(|fk| fk.table == table) {
                keep.insert(&other.name);
            }
        }
        self.hidden = schema.tables.iter().filter(|t| !keep.contains(t.name.as_str())).map(|t| t.name.clone()).collect();
        self.fit_requested = true;
    }

    /// The schema without the hidden tables.
    pub fn visible(&self, schema: &Schema) -> Schema {
        let mut s = schema.clone();
        s.tables.retain(|t| !self.hidden.contains(&t.name));
        s
    }

    /// Save the canvas, as currently shown, to a PNG file on the next frame.
    pub fn export_png(&mut self, path: PathBuf) {
        self.png_request = Some(path);
    }

    /// Outcome of the last `export_png`, once the screenshot has arrived.
    pub fn take_png_result(&mut self) -> Option<Result<PathBuf, String>> {
        self.png_result.take()
    }

    fn layouts(&self, schema: &Schema, sizes: &HashMap<String, Vec2>) -> HashMap<String, BoxLayout> {
        schema
            .tables
            .iter()
            .filter_map(|t| {
                let pos = *self.positions.get(&t.name)?;
                let rect = Rect::from_min_size(pos, *sizes.get(&t.name)?);
                let rows = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (c.name.clone(), pos.y + HEADER_H + ROW_H * (i as f32 + 0.5)))
                    .collect();
                Some((t.name.clone(), BoxLayout { rect, rows }))
            })
            .collect()
    }

    /// A box's title: the table name, plus its row count when known.
    fn title(&self, t: &Table) -> String {
        match self.row_counts.get(&t.name) {
            Some(n) => format!("{} ({n})", t.name),
            None => t.name.clone(),
        }
    }

    /// `full` is the whole schema; hidden tables are left out here. With
    /// `editable`, double-clicks, the context menu and the column handles
    /// return the changes the user asks for. `browsable` are the tables of
    /// the open database, whose data the context menu offers to show.
    pub fn ui(&mut self, ui: &mut egui::Ui, full: &Schema, editable: bool, browsable: &HashSet<String>) -> Option<ErAction> {
        if self.zoom == 0.0 {
            self.zoom = 1.0;
            self.fit_requested = true;
        }
        let schema = &self.visible(full);
        let (canvas, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        self.canvas = Some(canvas);
        self.receive_screenshot(ui);
        let painter = ui.painter_at(canvas);
        let visuals = ui.visuals().clone();
        painter.rect_filled(canvas, 0.0, visuals.extreme_bg_color);

        if schema.tables.is_empty() {
            let msg = match (full.tables.is_empty(), editable) {
                (true, true) => "No tables yet: right-click to add one",
                (true, false) => "No tables to draw",
                _ => "All tables are hidden (see the Tables menu)",
            };
            painter.text(canvas.center(), Align2::CENTER_CENTER, msg, FontId::proportional(16.0), visuals.weak_text_color());
            if response.secondary_clicked() {
                self.context_table = None;
                self.context_column = None;
                self.context_world = response.interact_pointer_pos().map(|p| ((p - canvas.min - self.pan) / self.zoom).to_pos2());
            }
            return self.context_menu(&response, full, editable, browsable);
        }

        // Measure boxes in world units (zoom 1).
        let before = std::mem::take(&mut self.sizes);
        let sizes: HashMap<String, Vec2> = full.tables.iter().map(|t| (t.name.clone(), box_size(&painter, &self.title(t), t))).collect();
        // A box that grew (a column added, a longer name) pushes its neighbors aside.
        let grown: HashSet<String> = schema
            .tables
            .iter()
            .filter(|t| before.get(&t.name).zip(sizes.get(&t.name)).is_some_and(|(a, b)| b.x > a.x + 0.5 || b.y > a.y + 0.5))
            .map(|t| t.name.clone())
            .collect();
        if !grown.is_empty() {
            let mut placed: HashMap<String, Pos2> =
                schema.tables.iter().filter_map(|t| Some((t.name.clone(), *self.positions.get(&t.name)?))).collect();
            if er_layout::separate(&mut placed, &sizes, &grown) {
                self.positions.extend(placed);
            }
        }
        self.auto_place(schema, &sizes);
        let layouts = self.layouts(schema, &sizes);
        self.sizes = sizes;

        if let Some(path) = self.png_request.take() {
            let capture = PngCapture { path, canvas, pixels_per_point: ui.ctx().pixels_per_point() };
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(capture)));
            self.capturing = true;
        }
        let capturing = self.capturing;
        if capturing {
            ui.ctx().request_repaint();
        }

        if self.fit_requested {
            self.fit(canvas, &layouts);
            self.fit_requested = false;
        }

        let to_world = |p: Pos2, pan: Vec2, zoom: f32| ((p - canvas.min - pan) / zoom).to_pos2();

        // ---- interaction
        let hover_world = response.hover_pos().map(|p| to_world(p, self.pan, self.zoom));
        let hovered_table = hover_world.and_then(|w| {
            schema
                .tables
                .iter()
                .rev()
                .find(|t| layouts[&t.name].rect.contains(w))
                .map(|t| t.name.clone())
        });
        let hovered_column = hovered_table.as_ref().zip(hover_world).and_then(|(t, w)| row_at(&layouts[t], w));
        // The handle at the right end of a row, where relationships are drawn from.
        let handle_r = 5.0 / self.zoom.max(0.5);
        let hovered_handle = hover_world.filter(|_| editable && !capturing).and_then(|w| {
            schema.tables.iter().rev().find_map(|t| {
                let l = &layouts[&t.name];
                l.rows.iter().find(|(_, y)| w.distance(pos2(l.rect.max.x, **y)) <= handle_r * 1.6).map(|(c, _)| (t.name.clone(), c.clone()))
            })
        });
        let mut action = None;

        if response.drag_started() {
            if let Some(h) = hovered_handle.clone() {
                self.linking = Some(h);
            } else {
                self.dragging = hovered_table.clone();
            }
        }
        if self.linking.is_some() {
            if response.drag_stopped() {
                let (table, column) = self.linking.take().unwrap();
                let target = hovered_table.clone().and_then(|t| {
                    let c = hovered_column.clone().or_else(|| match full.table(&t)?.primary_key().as_slice() {
                        [pk] => Some(pk.clone()),
                        _ => None,
                    })?;
                    Some((t, c))
                });
                if let Some((t, c)) = target
                    && (t != table || c != column)
                {
                    action = Some(ErAction::Link { table, column, target: t, target_column: c });
                }
            }
        } else if response.dragged() {
            let delta = response.drag_delta();
            match &self.dragging {
                Some(name) => {
                    if let Some(p) = self.positions.get_mut(name) {
                        *p += delta / self.zoom;
                    }
                }
                None => self.pan += delta,
            }
        }
        if response.drag_stopped() {
            self.dragging = None;
        }
        if response.hovered() {
            let (scroll, zoom_delta) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = zoom_delta * (scroll * 0.0015).exp();
            if factor != 1.0
                && let Some(pointer) = response.hover_pos()
            {
                self.zoom_around(canvas, pointer, factor);
            }
        }
        if response.clicked() {
            self.selected = hovered_table.clone();
        }
        if response.double_clicked()
            && let Some(t) = &hovered_table
        {
            if editable {
                action = Some(ErAction::EditTable(t.clone()));
            } else if browsable.contains(t) {
                action = Some(ErAction::OpenData(t.clone()));
            }
        }
        if response.secondary_clicked() {
            self.context_table = hovered_table.clone();
            self.context_column = hovered_column.clone();
            self.context_world = hover_world;
        }
        if let Some(a) = self.context_menu(&response, full, editable, browsable) {
            action = Some(a);
        }
        // Comments show on hover: a column's on its row, the table's elsewhere on the box.
        let comment = hovered_table.as_ref().and_then(|t| full.table(t)).and_then(|t| match &hovered_column {
            Some(c) => t.column(c).and_then(|c| c.comment.clone()),
            None => t.comment.clone(),
        });
        if let Some(comment) = comment.filter(|_| !capturing && self.dragging.is_none() && self.linking.is_none()) {
            response.clone().on_hover_text(comment);
        }
        let focus = if capturing {
            self.selected.clone()
        } else {
            self.dragging.clone().or(hovered_table.clone()).or(self.selected.clone())
        };
        if self.linking.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        } else if hovered_handle.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        } else if self.dragging.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if hovered_table.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        let (pan, zoom) = (self.pan, self.zoom);
        let to_screen = |p: Pos2| canvas.min + pan + p.to_vec2() * zoom;

        // ---- background grid
        let grid = 40.0 * self.zoom;
        if grid > 8.0 {
            let dot = visuals.weak_text_color().gamma_multiply(0.35);
            let start = canvas.min + vec2(self.pan.x.rem_euclid(grid), self.pan.y.rem_euclid(grid));
            let mut x = start.x;
            while x < canvas.max.x {
                let mut y = start.y;
                while y < canvas.max.y {
                    painter.circle_filled(pos2(x, y), 1.0, dot);
                    y += grid;
                }
                x += grid;
            }
        }

        // ---- relationships (drawn under the boxes)
        let accent = visuals.selection.bg_fill;
        let line_color = visuals.weak_text_color();
        for e in edges(schema, &layouts) {
            let related = focus.as_deref().is_some_and(|f| f == e.child || f == e.parent);
            let stroke = if related { Stroke::new(2.0, accent) } else { Stroke::new(1.3, line_color) };
            let points = e.points.map(to_screen);
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(points, false, Color32::TRANSPARENT, stroke));
            let (a, b) = (points[0], points[3]);

            // Crow's foot ("many") at the child, bar ("one") at the parent.
            let s = 8.0 * self.zoom;
            let foot_base = a + vec2(e.child_dir * s, 0.0);
            for dy in [-s * 0.7, 0.0, s * 0.7] {
                painter.line_segment([foot_base, a + vec2(0.0, dy)], stroke);
            }
            let bar_x = b.x + e.parent_dir * s * 0.8;
            painter.line_segment([pos2(bar_x, b.y - s * 0.7), pos2(bar_x, b.y + s * 0.7)], stroke);
            if e.nullable {
                painter.circle_stroke(a + vec2(e.child_dir * s * 1.6, 0.0), s * 0.4, stroke);
            }
        }

        // ---- table boxes
        let fs = 13.0 * self.zoom;
        let header_bg = visuals.selection.bg_fill.gamma_multiply(0.8);
        for t in &schema.tables {
            let layout = &layouts[&t.name];
            let rect = Rect::from_min_max(to_screen(layout.rect.min), to_screen(layout.rect.max));
            if !rect.intersects(canvas) {
                continue;
            }
            let highlighted = focus.as_deref() == Some(t.name.as_str());
            let radius = CornerRadius::same((6.0 * self.zoom).round().clamp(0.0, 255.0) as u8);
            painter.rect_filled(rect.translate(vec2(2.0, 3.0) * self.zoom), radius, Color32::from_black_alpha(40));
            painter.rect_filled(rect, radius, visuals.window_fill);
            let header = Rect::from_min_size(rect.min, vec2(rect.width(), HEADER_H * self.zoom));
            painter.rect_filled(header, CornerRadius { ne: radius.ne, nw: radius.nw, se: 0, sw: 0 }, header_bg);
            painter.text(
                header.center(),
                Align2::CENTER_CENTER,
                self.title(t),
                FontId::proportional(fs * 1.05),
                visuals.strong_text_color(),
            );
            let border = if highlighted { Stroke::new(2.0, accent) } else { Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color) };
            painter.rect_stroke(rect, radius, border, StrokeKind::Inside);

            if fs < 5.0 {
                continue; // too small to read
            }
            let pk = t.primary_key();
            for c in &t.columns {
                let y = to_screen(pos2(0.0, layout.rows[&c.name])).y;
                let tag = column_tag(t, &pk, c);
                let tag_color = match tag {
                    "PK" => PK_COLOR,
                    "FK" => accent,
                    _ => visuals.weak_text_color(),
                };
                let x0 = rect.min.x + PAD_X * self.zoom;
                painter.text(pos2(x0, y), Align2::LEFT_CENTER, tag, FontId::monospace(fs * 0.8), tag_color);
                let name_color = if c.nullable { visuals.text_color() } else { visuals.strong_text_color() };
                painter.text(pos2(x0 + 26.0 * self.zoom, y), Align2::LEFT_CENTER, &c.name, FontId::proportional(fs), name_color);
                let ty = column_type(c);
                painter.text(
                    pos2(rect.max.x - PAD_X * self.zoom, y),
                    Align2::RIGHT_CENTER,
                    ty,
                    FontId::monospace(fs * 0.85),
                    visuals.weak_text_color(),
                );
            }
        }

        // ---- relationship handles and the one being drawn
        if editable && !capturing {
            let handles_of = self.linking.as_ref().map(|(t, _)| t.clone()).or(hovered_table.clone());
            for t in schema.tables.iter().filter(|t| Some(&t.name) == handles_of.as_ref()) {
                let l = &layouts[&t.name];
                for (c, y) in &l.rows {
                    let at = to_screen(pos2(l.rect.max.x, *y));
                    let active = hovered_handle.as_ref().is_some_and(|(ht, hc)| *ht == t.name && hc == c)
                        || self.linking.as_ref().is_some_and(|(lt, lc)| *lt == t.name && lc == c);
                    let r = handle_r * self.zoom;
                    painter.circle(at, r, if active { accent } else { visuals.window_fill }, Stroke::new(1.5, accent));
                }
            }
            if let Some((t, c)) = &self.linking
                && let Some(l) = layouts.get(t)
                && let Some(y) = l.rows.get(c)
                && let Some(pointer) = response.hover_pos().or(ui.ctx().pointer_latest_pos())
            {
                let from = to_screen(pos2(l.rect.max.x, *y));
                let stroke = Stroke::new(2.0, accent);
                painter.line_segment([from, pointer], stroke);
                if let Some(ht) = &hovered_table {
                    let hl = &layouts[ht];
                    let row = hovered_column.as_ref().and_then(|hc| hl.rows.get(hc)).map(|y| {
                        Rect::from_min_max(to_screen(pos2(hl.rect.min.x, y - ROW_H / 2.0)), to_screen(pos2(hl.rect.max.x, y + ROW_H / 2.0)))
                    });
                    let target = row.unwrap_or_else(|| Rect::from_min_max(to_screen(hl.rect.min), to_screen(hl.rect.max)));
                    painter.rect_stroke(target, 2.0, stroke, StrokeKind::Inside);
                }
            }
        }

        if capturing {
            return action;
        }
        let hidden = full.tables.len() - schema.tables.len();
        painter.text(
            canvas.left_bottom() + vec2(8.0, -8.0),
            Align2::LEFT_BOTTOM,
            format!(
                "{:.0}%{}  ·  drag tables to move, drag background to pan, scroll to zoom, right-click for options{}  ·  ! = not null",
                self.zoom * 100.0,
                if hidden > 0 { format!("  ·  {hidden} hidden") } else { String::new() },
                if editable {
                    ", double-click a table to edit, drag a column's ○ onto another column to link them"
                } else if !browsable.is_empty() {
                    ", double-click a table to browse its data"
                } else {
                    ""
                }
            ),
            FontId::proportional(11.0),
            visuals.weak_text_color(),
        );
        action
    }

    fn context_menu(&mut self, response: &egui::Response, full: &Schema, editable: bool, browsable: &HashSet<String>) -> Option<ErAction> {
        let mut action = None;
        response.context_menu(|ui| {
            if let Some(table) = self.context_table.clone()
                && let Some(t) = full.table(&table)
            {
                ui.label(egui::RichText::new(&table).strong());
                if ui
                    .add_enabled(browsable.contains(&table), egui::Button::new("Browse data"))
                    .on_disabled_hover_text("The open database has no such table")
                    .clicked()
                {
                    action = Some(ErAction::OpenData(table.clone()));
                    ui.close();
                }
                if editable {
                    let mut pick = |ui: &mut egui::Ui, label: &str, a: ErAction| {
                        if ui.button(label).clicked() {
                            action = Some(a);
                            ui.close();
                        }
                    };
                    pick(ui, "Edit table…", ErAction::EditTable(table.clone()));
                    pick(ui, "Add column…", ErAction::AddColumn(table.clone()));
                    if let Some(column) = self.context_column.clone() {
                        if t.column(&column).is_some_and(|c| c.references.is_some()) {
                            pick(ui, &format!("Remove reference of {column}"), ErAction::Unlink { table: table.clone(), column: column.clone() });
                        }
                        pick(ui, &format!("Delete column {column}"), ErAction::DeleteColumn { table: table.clone(), column });
                    }
                    pick(ui, "Delete table", ErAction::DeleteTable(table.clone()));
                    ui.separator();
                }
                if ui.button("Hide table").clicked() {
                    self.set_hidden(&table, true);
                    ui.close();
                }
                if ui.button("Show only this and related tables").clicked() {
                    self.show_related(full, &table);
                    ui.close();
                }
                ui.separator();
            }
            if editable
                && self.context_table.is_none()
                && ui.button("New table…").clicked()
            {
                action = Some(ErAction::NewTable(self.context_world.unwrap_or_default()));
                ui.close();
            }
            if ui.add_enabled(!self.hidden.is_empty(), egui::Button::new("Show all tables")).clicked() {
                self.show_all();
                ui.close();
            }
            if ui.button("Fit to window").clicked() {
                self.request_fit();
                ui.close();
            }
        });
        action
    }

    fn receive_screenshot(&mut self, ui: &egui::Ui) {
        let shots: Vec<_> = ui.input(|i| {
            i.raw
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Screenshot { user_data, image, .. } => {
                        let data = user_data.data.clone()?;
                        data.downcast::<PngCapture>().ok().map(|c| (c, image.clone()))
                    }
                    _ => None,
                })
                .collect()
        });
        for (capture, image) in shots {
            self.capturing = false;
            self.png_result = Some(save_png(&capture, &image).map(|()| capture.path.clone()).map_err(|e| format!("Cannot write {}: {e}", capture.path.display())));
        }
    }

    /// The visible tables as a standalone SVG document, light theme, at 100%.
    pub fn svg(&self, full: &Schema) -> String {
        let schema = &self.visible(full);
        let layouts = self.layouts(schema, &self.sizes);
        let Some(bounds) = layouts.values().map(|l| l.rect).reduce(|a, b| a.union(b)) else {
            return String::from("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"0\" height=\"0\"/>\n");
        };
        let edges = edges(schema, &layouts);
        // Self-reference loops and crow's feet stick out of the boxes.
        let bounds = edges.iter().flat_map(|e| e.points).fold(bounds, |r, p| r.union(Rect::from_center_size(p, Vec2::splat(20.0))));
        let margin = 24.0;
        let origin = bounds.min - Vec2::splat(margin);
        let size = bounds.size() + Vec2::splat(margin * 2.0);
        let p = |q: Pos2| q - origin;

        let (bg, box_fill, header, border, text, weak, line) = ("#ffffff", "#ffffff", "#dbe4f7", "#b7bfcc", "#1f2328", "#6e7781", "#7d8590");
        let mut out = String::new();
        let _ = writeln!(
            out,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="Helvetica, Arial, sans-serif">"#,
            w = size.x,
            h = size.y
        );
        let _ = writeln!(out, r#"<rect width="100%" height="100%" fill="{bg}"/>"#);
        let _ = writeln!(out, r#"<g fill="none" stroke="{line}" stroke-width="1.3">"#);
        for e in &edges {
            let [a, c1, c2, b] = e.points.map(p);
            let _ = writeln!(out, r#"<path d="M{:.1},{:.1} C{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}"/>"#, a.x, a.y, c1.x, c1.y, c2.x, c2.y, b.x, b.y);
            let s = 8.0;
            let foot = a + vec2(e.child_dir * s, 0.0);
            for dy in [-s * 0.7, 0.0, s * 0.7] {
                let _ = writeln!(out, r#"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}"/>"#, foot.x, foot.y, a.x, a.y + dy);
            }
            let bar_x = b.x + e.parent_dir * s * 0.8;
            let _ = writeln!(out, r#"<line x1="{bar_x:.1}" y1="{:.1}" x2="{bar_x:.1}" y2="{:.1}"/>"#, b.y - s * 0.7, b.y + s * 0.7);
            if e.nullable {
                let _ = writeln!(out, r#"<circle cx="{:.1}" cy="{:.1}" r="{:.1}"/>"#, a.x + e.child_dir * s * 1.6, a.y, s * 0.4);
            }
        }
        out.push_str("</g>\n");

        for t in &schema.tables {
            let Some(layout) = layouts.get(&t.name) else { continue };
            let min = p(layout.rect.min);
            let (w, h) = (layout.rect.width(), layout.rect.height());
            let _ = writeln!(out, "<g>");
            let _ = writeln!(out, r#"<rect x="{:.1}" y="{:.1}" width="{w:.1}" height="{h:.1}" rx="6" fill="{box_fill}"/>"#, min.x, min.y);
            let _ = writeln!(out, r#"<rect x="{:.1}" y="{:.1}" width="{w:.1}" height="{HEADER_H:.1}" rx="6" fill="{header}"/>"#, min.x, min.y);
            let _ = writeln!(out, r#"<rect x="{:.1}" y="{:.1}" width="{w:.1}" height="6" fill="{header}"/>"#, min.x, min.y + HEADER_H - 6.0);
            let _ = writeln!(out, r#"<rect x="{:.1}" y="{:.1}" width="{w:.1}" height="{h:.1}" rx="6" fill="none" stroke="{border}"/>"#, min.x, min.y);
            let _ = writeln!(
                out,
                r#"<text x="{:.1}" y="{:.1}" text-anchor="middle" dominant-baseline="central" font-size="13.6" font-weight="bold" fill="{text}">{}</text>"#,
                min.x + w / 2.0,
                min.y + HEADER_H / 2.0,
                xml_escape(&self.title(t))
            );
            let pk = t.primary_key();
            for c in &t.columns {
                let y = p(pos2(0.0, layout.rows[&c.name])).y;
                let x0 = min.x + PAD_X;
                let tag = column_tag(t, &pk, c);
                let tag_color = match tag {
                    "PK" => "#c98a10",
                    "FK" => "#3b6fd8",
                    _ => weak,
                };
                let weight = if c.nullable { "normal" } else { "bold" };
                let _ = writeln!(
                    out,
                    r#"<text x="{x0:.1}" y="{y:.1}" dominant-baseline="central" font-family="monospace" font-size="10.4" fill="{tag_color}">{tag}</text>"#
                );
                let _ = writeln!(
                    out,
                    r#"<text x="{:.1}" y="{y:.1}" dominant-baseline="central" font-size="13" font-weight="{weight}" fill="{text}">{}</text>"#,
                    x0 + 26.0,
                    xml_escape(&c.name)
                );
                let _ = writeln!(
                    out,
                    r#"<text x="{:.1}" y="{y:.1}" text-anchor="end" dominant-baseline="central" font-family="monospace" font-size="11" fill="{weak}">{}</text>"#,
                    min.x + w - PAD_X,
                    xml_escape(&column_type(c))
                );
            }
            out.push_str("</g>\n");
        }
        out.push_str("</svg>\n");
        out
    }

    /// Place tables that don't have a position yet: all of them through
    /// `er_layout::layout` when none has one, else each in free space next to
    /// its related tables.
    fn auto_place(&mut self, schema: &Schema, sizes: &HashMap<String, Vec2>) {
        let missing: Vec<&str> = schema.tables.iter().map(|t| t.name.as_str()).filter(|n| !self.positions.contains_key(*n)).collect();
        if missing.is_empty() {
            return;
        }
        let visible: HashMap<String, Vec2> = schema.tables.iter().filter_map(|t| Some((t.name.clone(), *sizes.get(&t.name)?))).collect();
        if missing.len() == schema.tables.len() {
            self.positions.extend(er_layout::layout(schema, &visible));
            return;
        }
        for name in missing {
            let at = er_layout::place(schema, name, &visible, &self.positions);
            self.positions.insert(name.to_string(), at);
        }
    }

    /// Move overlapping tables apart, keeping the arrangement.
    pub fn tidy(&mut self, full: &Schema) {
        let schema = self.visible(full);
        let sizes: HashMap<String, Vec2> = schema.tables.iter().filter_map(|t| Some((t.name.clone(), *self.sizes.get(&t.name)?))).collect();
        let mut positions: HashMap<String, Pos2> = sizes.keys().filter_map(|n| Some((n.clone(), *self.positions.get(n)?))).collect();
        er_layout::separate(&mut positions, &sizes, &HashSet::new());
        self.positions.extend(positions);
    }

    fn fit(&mut self, canvas: Rect, layouts: &HashMap<String, BoxLayout>) {
        let bounds = layouts.values().map(|l| l.rect).reduce(|a, b| a.union(b));
        let Some(bounds) = bounds else { return };
        let margin = 40.0;
        let zx = (canvas.width() - margin * 2.0) / bounds.width();
        let zy = (canvas.height() - margin * 2.0) / bounds.height();
        self.zoom = zx.min(zy).clamp(MIN_ZOOM, 1.5);
        let content = bounds.size() * self.zoom;
        let offset = (canvas.size() - content) / 2.0;
        self.pan = offset - bounds.min.to_vec2() * self.zoom;
    }
}

const PK_COLOR: Color32 = Color32::from_rgb(0xe0, 0xa0, 0x20);

fn column_tag(t: &Table, pk: &[String], c: &simple_database::schema::Column) -> &'static str {
    if pk.contains(&c.name) {
        "PK"
    } else if t.is_foreign_key(&c.name) {
        "FK"
    } else if c.unique {
        "UQ"
    } else {
        ""
    }
}

fn column_type(c: &simple_database::schema::Column) -> String {
    format!("{}{}", c.data_type, if c.nullable { "" } else { " !" })
}

/// The column whose row contains `world`, if any.
fn row_at(layout: &BoxLayout, world: Pos2) -> Option<String> {
    layout.rows.iter().find(|(_, y)| (world.y - **y).abs() <= ROW_H / 2.0).map(|(c, _)| c.clone())
}

/// Relationship lines between laid-out tables, in world coordinates.
fn edges(schema: &Schema, layouts: &HashMap<String, BoxLayout>) -> Vec<Edge> {
    let mut out = Vec::new();
    for t in &schema.tables {
        // A multi-column key is drawn between its first columns.
        for fk in t.all_foreign_keys() {
            let nullable = fk.columns.iter().any(|c| t.column(c).is_some_and(|c| c.nullable));
            let (Some(child), Some(parent)) = (layouts.get(&t.name), layouts.get(&fk.table)) else { continue };
            let (Some(&cy), Some(&py)) = (child.rows.get(&fk.columns[0]), parent.rows.get(&fk.ref_columns[0])) else { continue };
            let (child_x, child_dir, parent_x, parent_dir) = if t.name == fk.table {
                // Self reference: loop out of the right edge.
                (child.rect.max.x, 1.0, parent.rect.max.x, 1.0)
            } else if parent.rect.min.x < child.rect.max.x && child.rect.min.x < parent.rect.max.x {
                // One above the other: loop out of the right edges rather than cross both boxes.
                (child.rect.max.x, 1.0, parent.rect.max.x, 1.0)
            } else if parent.rect.center().x < child.rect.center().x {
                (child.rect.min.x, -1.0, parent.rect.max.x, 1.0)
            } else {
                (child.rect.max.x, 1.0, parent.rect.min.x, -1.0)
            };
            let a = pos2(child_x, cy);
            let b = pos2(parent_x, py);
            let bend = 40.0 + (a.x - b.x).abs() * 0.4;
            let (c1, c2) = if child_dir == parent_dir {
                // Same side: both control points beyond the wider box.
                let x = a.x.max(b.x) + 40.0 + (a.y - b.y).abs() * 0.15;
                (pos2(x, a.y), pos2(x, b.y))
            } else {
                (a + vec2(child_dir * bend, 0.0), b + vec2(parent_dir * bend, 0.0))
            };
            out.push(Edge {
                points: [a, c1, c2, b],
                child_dir,
                parent_dir,
                nullable,
                child: t.name.clone(),
                parent: fk.table.clone(),
            });
        }
    }
    out
}

fn save_png(capture: &PngCapture, image: &egui::ColorImage) -> Result<(), String> {
    let bounds = Rect::from_min_size(Pos2::ZERO, vec2(image.size[0] as f32, image.size[1] as f32) / capture.pixels_per_point);
    let region = capture.canvas.intersect(bounds);
    if !region.is_positive() {
        return Err("the canvas is not on screen".into());
    }
    let cropped = image.region(&region, Some(capture.pixels_per_point));
    let bytes: Vec<u8> = cropped.pixels.iter().flat_map(|p| p.to_srgba_unmultiplied()).collect();
    image::save_buffer_with_format(
        &capture.path,
        &bytes,
        cropped.size[0] as u32,
        cropped.size[1] as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|e| e.to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn box_size(painter: &egui::Painter, title: &str, t: &Table) -> Vec2 {
    let measure = |s: String, font: FontId| painter.layout_no_wrap(s, font, Color32::WHITE).size().x;
    let title = measure(title.to_string(), FontId::proportional(13.65)) + PAD_X * 2.0;
    let rows = t
        .columns
        .iter()
        .map(|c| {
            26.0 + measure(c.name.clone(), FontId::proportional(13.0)) + 24.0 + measure(column_type(c), FontId::monospace(11.05)) + PAD_X * 2.0
        })
        .fold(0.0, f32::max);
    vec2(title.max(rows).max(160.0), HEADER_H + ROW_H * t.columns.len() as f32 + 6.0)
}
