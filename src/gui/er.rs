//! Interactive entity-relationship diagram: drag tables, pan, zoom.

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use simple_database::schema::{Schema, Table};
use std::collections::HashMap;

const ROW_H: f32 = 20.0;
const HEADER_H: f32 = 26.0;
const PAD_X: f32 = 10.0;
const COL_GAP: f32 = 90.0;
const ROW_GAP: f32 = 40.0;

#[derive(Default)]
pub struct ErView {
    /// Top-left corner of each table box, in world coordinates.
    positions: HashMap<String, Pos2>,
    pan: Vec2,
    zoom: f32,
    dragging: Option<String>,
    /// Table highlighted from outside (e.g. the sidebar).
    pub selected: Option<String>,
    fit_requested: bool,
}

struct BoxLayout {
    rect: Rect,
    /// World y of each column's row center, by column name.
    rows: HashMap<String, f32>,
}

impl ErView {
    pub fn reset_layout(&mut self) {
        self.positions.clear();
        self.fit_requested = true;
    }

    pub fn request_fit(&mut self) {
        self.fit_requested = true;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, schema: &Schema) {
        if self.zoom == 0.0 {
            self.zoom = 1.0;
            self.fit_requested = true;
        }
        let (canvas, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(canvas);
        let visuals = ui.visuals().clone();
        painter.rect_filled(canvas, 0.0, visuals.extreme_bg_color);

        if schema.tables.is_empty() {
            painter.text(canvas.center(), Align2::CENTER_CENTER, "No tables to draw", FontId::proportional(16.0), visuals.weak_text_color());
            return;
        }

        // Measure boxes in world units (zoom 1).
        let sizes: HashMap<String, Vec2> = schema
            .tables
            .iter()
            .map(|t| (t.name.clone(), box_size(&painter, t)))
            .collect();
        self.auto_place(schema, &sizes);
        let layouts: HashMap<String, BoxLayout> = schema
            .tables
            .iter()
            .map(|t| {
                let pos = self.positions[&t.name];
                let rect = Rect::from_min_size(pos, sizes[&t.name]);
                let rows = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (c.name.clone(), pos.y + HEADER_H + ROW_H * (i as f32 + 0.5)))
                    .collect();
                (t.name.clone(), BoxLayout { rect, rows })
            })
            .collect();

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

        if response.drag_started() {
            self.dragging = hovered_table.clone();
        }
        if response.dragged() {
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
                let world = to_world(pointer, self.pan, self.zoom);
                self.zoom = (self.zoom * factor).clamp(0.2, 3.0);
                self.pan = pointer - canvas.min - world.to_vec2() * self.zoom;
            }
        }
        if response.clicked() {
            self.selected = hovered_table.clone();
        }
        let focus = self.dragging.clone().or(hovered_table.clone()).or(self.selected.clone());
        if self.dragging.is_some() {
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
        for t in &schema.tables {
            for c in &t.columns {
                let Some(fk) = &c.references else { continue };
                let (Some(child), Some(parent)) = (layouts.get(&t.name), layouts.get(&fk.table)) else { continue };
                let (Some(&cy), Some(&py)) = (child.rows.get(&c.name), parent.rows.get(&fk.column)) else { continue };
                let related = focus.as_deref().is_some_and(|f| f == t.name || f == fk.table);
                let stroke = if related { Stroke::new(2.0, accent) } else { Stroke::new(1.3, line_color) };

                let (child_x, child_dir, parent_x, parent_dir) = if t.name == fk.table {
                    // Self reference: loop out of the right edge.
                    (child.rect.max.x, 1.0, parent.rect.max.x, 1.0)
                } else if parent.rect.center().x < child.rect.center().x {
                    (child.rect.min.x, -1.0, parent.rect.max.x, 1.0)
                } else {
                    (child.rect.max.x, 1.0, parent.rect.min.x, -1.0)
                };
                let a = to_screen(pos2(child_x, cy));
                let b = to_screen(pos2(parent_x, py));
                let bend = (40.0 + (a.x - b.x).abs() * 0.4) * self.zoom.max(0.5);
                let points = [a, a + vec2(child_dir * bend, 0.0), b + vec2(parent_dir * bend, 0.0), b];
                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(points, false, Color32::TRANSPARENT, stroke));

                // Crow's foot ("many") at the child, bar ("one") at the parent.
                let s = 8.0 * self.zoom;
                let foot_base = a + vec2(child_dir * s, 0.0);
                for dy in [-s * 0.7, 0.0, s * 0.7] {
                    painter.line_segment([foot_base, a + vec2(0.0, dy)], stroke);
                }
                let bar_x = b.x + parent_dir * s * 0.8;
                painter.line_segment([pos2(bar_x, b.y - s * 0.7), pos2(bar_x, b.y + s * 0.7)], stroke);
                if c.nullable {
                    painter.circle_stroke(a + vec2(child_dir * s * 1.6, 0.0), s * 0.4, stroke);
                }
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
                &t.name,
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
                let mut tag = String::new();
                if pk.contains(&c.name) {
                    tag.push_str("PK");
                } else if c.references.is_some() {
                    tag.push_str("FK");
                } else if c.unique {
                    tag.push_str("UQ");
                }
                let tag_color = match tag.as_str() {
                    "PK" => Color32::from_rgb(0xe0, 0xa0, 0x20),
                    "FK" => accent,
                    _ => visuals.weak_text_color(),
                };
                let x0 = rect.min.x + PAD_X * self.zoom;
                painter.text(pos2(x0, y), Align2::LEFT_CENTER, &tag, FontId::monospace(fs * 0.8), tag_color);
                let name_color = if c.nullable { visuals.text_color() } else { visuals.strong_text_color() };
                painter.text(pos2(x0 + 26.0 * self.zoom, y), Align2::LEFT_CENTER, &c.name, FontId::proportional(fs), name_color);
                let ty = format!("{}{}", c.data_type, if c.nullable { "" } else { " !" });
                painter.text(
                    pos2(rect.max.x - PAD_X * self.zoom, y),
                    Align2::RIGHT_CENTER,
                    ty,
                    FontId::monospace(fs * 0.85),
                    visuals.weak_text_color(),
                );
            }
        }

        painter.text(
            canvas.left_bottom() + vec2(8.0, -8.0),
            Align2::LEFT_BOTTOM,
            format!("{:.0}%  ·  drag tables to move, drag background to pan, scroll to zoom  ·  ! = not null", self.zoom * 100.0),
            FontId::proportional(11.0),
            visuals.weak_text_color(),
        );
    }

    /// Place tables that don't have a position yet: one column per FK depth,
    /// parents to the left of the tables that reference them.
    fn auto_place(&mut self, schema: &Schema, sizes: &HashMap<String, Vec2>) {
        let missing: Vec<&Table> = schema.tables.iter().filter(|t| !self.positions.contains_key(&t.name)).collect();
        if missing.is_empty() {
            return;
        }
        let mut depth: HashMap<&str, usize> = HashMap::new();
        fn depth_of<'a>(s: &'a Schema, t: &'a Table, memo: &mut HashMap<&'a str, usize>, stack: &mut Vec<&'a str>) -> usize {
            if let Some(&d) = memo.get(t.name.as_str()) {
                return d;
            }
            if stack.contains(&t.name.as_str()) {
                return 0;
            }
            stack.push(&t.name);
            let d = t
                .columns
                .iter()
                .filter_map(|c| c.references.as_ref())
                .filter(|fk| fk.table != t.name)
                .filter_map(|fk| s.table(&fk.table))
                .map(|p| depth_of(s, p, memo, stack) + 1)
                .max()
                .unwrap_or(0);
            stack.pop();
            memo.insert(&t.name, d);
            d
        }
        for t in &schema.tables {
            depth_of(schema, t, &mut depth, &mut Vec::new());
        }

        let max_depth = depth.values().copied().max().unwrap_or(0);
        let mut x = 0.0;
        for d in 0..=max_depth {
            let column: Vec<&Table> = schema.tables.iter().filter(|t| depth[t.name.as_str()] == d).collect();
            let width = column.iter().map(|t| sizes[&t.name].x).fold(0.0, f32::max);
            let mut y = 0.0;
            for t in column {
                if !self.positions.contains_key(&t.name) {
                    // Keep clear of tables the user has already placed.
                    let mut pos = pos2(x, y);
                    while self.positions.values().any(|p| (p.x - pos.x).abs() < 1.0 && (p.y - pos.y).abs() < 1.0) {
                        pos.y += ROW_GAP;
                    }
                    self.positions.insert(t.name.clone(), pos);
                }
                y += sizes[&t.name].y + ROW_GAP;
            }
            x += width + COL_GAP;
        }
    }

    fn fit(&mut self, canvas: Rect, layouts: &HashMap<String, BoxLayout>) {
        let bounds = layouts.values().map(|l| l.rect).reduce(|a, b| a.union(b));
        let Some(bounds) = bounds else { return };
        let margin = 40.0;
        let zx = (canvas.width() - margin * 2.0) / bounds.width();
        let zy = (canvas.height() - margin * 2.0) / bounds.height();
        self.zoom = zx.min(zy).clamp(0.2, 1.5);
        let content = bounds.size() * self.zoom;
        let offset = (canvas.size() - content) / 2.0;
        self.pan = offset - bounds.min.to_vec2() * self.zoom;
    }
}

fn box_size(painter: &egui::Painter, t: &Table) -> Vec2 {
    let measure = |s: String, font: FontId| painter.layout_no_wrap(s, font, Color32::WHITE).size().x;
    let title = measure(t.name.clone(), FontId::proportional(13.65)) + PAD_X * 2.0;
    let rows = t
        .columns
        .iter()
        .map(|c| {
            let ty = format!("{}{}", c.data_type, if c.nullable { "" } else { " !" });
            26.0 + measure(c.name.clone(), FontId::proportional(13.0)) + 24.0 + measure(ty, FontId::monospace(11.05)) + PAD_X * 2.0
        })
        .fold(0.0, f32::max);
    vec2(title.max(rows).max(160.0), HEADER_H + ROW_H * t.columns.len() as f32 + 6.0)
}
