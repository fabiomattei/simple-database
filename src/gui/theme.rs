//! The app's look: a macOS-style light and dark theme (grey sidebar, white
//! content, blue accent, rounded controls) and the widgets the window chrome
//! is drawn with (segmented controls, pill buttons, section headers).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Shadow, Stroke, TextStyle, Theme, Vec2};
use std::sync::Arc;

/// Colors the chrome uses beyond egui's own visuals.
#[derive(Clone, Copy)]
pub struct Palette {
    pub sidebar: Color32,
    pub content: Color32,
    pub separator: Color32,
    pub accent: Color32,
    /// The selected row in the sidebar.
    pub selected: Color32,
    /// Behind a grid's header row and the filter field.
    pub well: Color32,
    pub muted: Color32,
}

pub fn palette(ui: &egui::Ui) -> Palette {
    palette_for(ui.visuals().dark_mode)
}

pub fn palette_for_ctx(ctx: &egui::Context) -> Palette {
    palette_for(ctx.global_style().visuals.dark_mode)
}

fn palette_for(dark: bool) -> Palette {
    if dark {
        Palette {
            sidebar: Color32::from_rgb(0x2b, 0x2b, 0x2d),
            content: Color32::from_rgb(0x1e, 0x1e, 0x1f),
            separator: Color32::from_rgb(0x3a, 0x3a, 0x3c),
            accent: Color32::from_rgb(0x0a, 0x84, 0xff),
            selected: Color32::from_rgb(0x46, 0x46, 0x4a),
            well: Color32::from_rgb(0x38, 0x38, 0x3b),
            muted: Color32::from_gray(0x98),
        }
    } else {
        Palette {
            sidebar: Color32::from_rgb(0xec, 0xec, 0xec),
            content: Color32::WHITE,
            separator: Color32::from_gray(0xdd),
            accent: Color32::from_rgb(0x00, 0x7a, 0xff),
            selected: Color32::from_gray(0xd8),
            well: Color32::from_gray(0xe2),
            muted: Color32::from_gray(0x86),
        }
    }
}

/// The semibold face, for titles and section headers.
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// Fonts and both themes' styles; egui picks one by the system or the View menu.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("inter".into(), Arc::new(egui::FontData::from_static(include_bytes!("../../assets/fonts/Inter-Regular.ttf"))));
    fonts
        .font_data
        .insert("inter-semibold".into(), Arc::new(egui::FontData::from_static(include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"))));
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, "inter".into());
    // Symbols Inter lacks come from egui's own fonts, after it.
    let fallbacks: Vec<String> = proportional.iter().skip(1).cloned().collect();
    fonts.families.insert(FontFamily::Name("bold".into()), std::iter::once("inter-semibold".to_string()).chain(fallbacks).collect());
    ctx.set_fonts(fonts);

    for theme in [Theme::Light, Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Small, FontId::proportional(10.5)),
                (TextStyle::Body, FontId::proportional(13.0)),
                (TextStyle::Button, FontId::proportional(13.0)),
                (TextStyle::Heading, bold(18.0)),
                (TextStyle::Monospace, FontId::monospace(12.5)),
            ]
            .into();
            let s = &mut style.spacing;
            s.item_spacing = Vec2::new(8.0, 5.0);
            s.button_padding = Vec2::new(9.0, 3.0);
            s.interact_size.y = 22.0;
            s.menu_margin = Margin::same(6);
            s.window_margin = Margin::same(16);
            s.scroll = egui::style::ScrollStyle::floating();
            style.visuals = visuals(theme == Theme::Dark);
        });
    }
}

fn visuals(dark: bool) -> egui::Visuals {
    let p = palette_for(dark);
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let radius = CornerRadius::same(6);
    let gray = Color32::from_gray;
    let w = &mut v.widgets;
    if dark {
        w.noninteractive.bg_fill = p.content;
        w.noninteractive.weak_bg_fill = p.content;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.separator);
        w.noninteractive.fg_stroke = Stroke::new(1.0, gray(0xdc));
        w.inactive.weak_bg_fill = gray(0x3a);
        w.inactive.bg_fill = gray(0x46);
        w.inactive.bg_stroke = Stroke::new(1.0, gray(0x48));
        w.inactive.fg_stroke = Stroke::new(1.0, gray(0xe4));
        w.hovered.weak_bg_fill = gray(0x46);
        w.hovered.bg_fill = gray(0x50);
        w.hovered.bg_stroke = Stroke::new(1.0, gray(0x5a));
        w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        w.active.weak_bg_fill = gray(0x55);
        w.active.bg_fill = gray(0x5c);
        w.active.bg_stroke = Stroke::new(1.0, gray(0x6a));
        w.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        w.open.weak_bg_fill = gray(0x40);
        w.open.bg_fill = p.content;
        w.open.bg_stroke = Stroke::new(1.0, gray(0x50));
        v.selection.bg_fill = Color32::from_rgb(0x1f, 0x4f, 0x86);
        v.selection.stroke = Stroke::new(1.0, Color32::from_rgb(0xb4, 0xd5, 0xff));
        v.faint_bg_color = Color32::from_rgb(0x26, 0x26, 0x28);
        v.code_bg_color = gray(0x33);
        v.warn_fg_color = Color32::from_rgb(0xff, 0x9f, 0x0a);
        v.error_fg_color = Color32::from_rgb(0xff, 0x45, 0x3a);
        v.window_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(110) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 16, spread: 0, color: Color32::from_black_alpha(90) };
        v.window_stroke = Stroke::new(1.0, gray(0x48));
        v.window_fill = Color32::from_rgb(0x28, 0x28, 0x2a);
    } else {
        w.noninteractive.bg_fill = p.content;
        w.noninteractive.weak_bg_fill = p.content;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.separator);
        w.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(0x1d, 0x1d, 0x1f));
        w.inactive.weak_bg_fill = Color32::WHITE;
        w.inactive.bg_fill = gray(0xe6);
        w.inactive.bg_stroke = Stroke::new(1.0, gray(0xd2));
        w.inactive.fg_stroke = Stroke::new(1.0, gray(0x26));
        w.hovered.weak_bg_fill = gray(0xf2);
        w.hovered.bg_fill = gray(0xdc);
        w.hovered.bg_stroke = Stroke::new(1.0, gray(0xb8));
        w.hovered.fg_stroke = Stroke::new(1.5, Color32::BLACK);
        w.active.weak_bg_fill = gray(0xe0);
        w.active.bg_fill = gray(0xcc);
        w.active.bg_stroke = Stroke::new(1.0, gray(0xa8));
        w.active.fg_stroke = Stroke::new(1.5, Color32::BLACK);
        w.open.weak_bg_fill = gray(0xea);
        w.open.bg_fill = Color32::WHITE;
        w.open.bg_stroke = Stroke::new(1.0, gray(0xc8));
        v.selection.bg_fill = Color32::from_rgb(0xb3, 0xd7, 0xff);
        v.selection.stroke = Stroke::new(1.0, Color32::from_rgb(0x00, 0x4f, 0xb0));
        v.faint_bg_color = gray(0xf5);
        v.code_bg_color = gray(0xee);
        v.warn_fg_color = Color32::from_rgb(0xc4, 0x6a, 0x00);
        v.error_fg_color = Color32::from_rgb(0xd7, 0x00, 0x15);
        v.window_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(45) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 16, spread: 0, color: Color32::from_black_alpha(35) };
        v.window_stroke = Stroke::new(1.0, gray(0xd0));
        v.window_fill = Color32::WHITE;
    }
    for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        wv.corner_radius = radius;
        wv.expansion = 0.0;
    }
    v.panel_fill = p.content;
    v.extreme_bg_color = p.content;
    v.text_edit_bg_color = Some(p.content);
    v.hyperlink_color = p.accent;
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.indent_has_left_vline = false;
    v
}

/// A segmented control: one rounded track, the chosen segment filled with the
/// accent; `fill` stretches it to the full width. Returns the segment clicked,
/// if any, and each segment's response.
pub fn segmented<T: Copy + PartialEq>(ui: &mut egui::Ui, current: T, items: &[(T, RichText, &str)], fill: bool) -> (Option<T>, Vec<egui::Response>) {
    let p = palette(ui);
    let mut clicked = None;
    let mut responses = Vec::new();
    egui::Frame::new().fill(p.well).corner_radius(CornerRadius::same(7)).inner_margin(Margin::same(2)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let width = if fill { (ui.available_width() - 2.0 * (items.len() as f32 - 1.0)) / items.len() as f32 } else { 0.0 };
        ui.horizontal(|ui| {
            for (value, text, hover) in items {
                let on = *value == current;
                let text = if on { text.clone().color(Color32::WHITE) } else { text.clone() };
                let b = egui::Button::new(text)
                    .corner_radius(CornerRadius::same(5))
                    .stroke(Stroke::NONE)
                    .min_size(Vec2::new(width, 22.0))
                    .fill(if on { p.accent } else { Color32::TRANSPARENT });
                let resp = ui.add(b);
                let resp = if hover.is_empty() { resp } else { resp.on_hover_text(*hover) };
                if resp.clicked() {
                    clicked = Some(*value);
                }
                responses.push(resp);
            }
        });
    });
    (clicked, responses)
}

/// A round-cornered toolbar button with a light border, like the ones in a macOS title bar.
pub fn pill(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) -> egui::Response {
    ui.add(egui::Button::new(text).corner_radius(CornerRadius::same(13)).min_size(Vec2::new(30.0, 26.0)))
}

/// A pill holding any widgets (an icon and a label); the whole pill is clickable.
pub fn pill_with(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let id = ui.next_auto_id();
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let w = if hovered { ui.visuals().widgets.hovered } else { ui.visuals().widgets.inactive };
    let inner = egui::Frame::new()
        .fill(w.weak_bg_fill)
        .stroke(w.bg_stroke)
        .corner_radius(CornerRadius::same(13))
        .inner_margin(Margin::symmetric(10, 2))
        .show(ui, |ui| {
            ui.set_min_height(20.0);
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.horizontal(|ui| add(ui));
        });
    ui.interact(inner.response.rect, id, egui::Sense::click())
}

/// The show/hide sidebar button: a window outline with its left pane drawn.
pub fn sidebar_button(ui: &mut egui::Ui) -> egui::Response {
    let resp = ui.add(egui::Button::new("").frame_when_inactive(false).corner_radius(CornerRadius::same(6)).min_size(Vec2::new(28.0, 24.0)));
    let color = ui.style().interact(&resp).fg_stroke.color;
    let r = egui::Rect::from_center_size(resp.rect.center(), Vec2::new(16.0, 12.0));
    let painter = ui.painter();
    painter.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.3, color), egui::StrokeKind::Inside);
    painter.vline(r.left() + 5.5, r.y_range(), Stroke::new(1.3, color));
    resp
}

/// A rounded search field with a magnifier, on the sidebar's grey.
pub fn search_field(ui: &mut egui::Ui, text: &mut String, hint: &str) -> egui::Response {
    let p = palette(ui);
    egui::Frame::new()
        .fill(p.well)
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            // Left to right even inside a right-to-left row.
            let row = Vec2::new(ui.available_width(), ui.spacing().interact_size.y);
            ui.allocate_ui_with_layout(row, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new("🔍").color(p.muted));
                let resp = ui.add(egui::TextEdit::singleline(text).hint_text(hint).frame(egui::Frame::NONE).desired_width(ui.available_width()));
                if !text.is_empty() && resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    text.clear();
                }
                resp
            })
            .inner
        })
        .inner
}

/// One full-width row of a sidebar list: rounded highlight when selected or
/// hovered, `add` laid out left to right inside. The row takes the clicks.
pub fn list_row(ui: &mut egui::Ui, selected: bool, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let p = palette(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), egui::Sense::click());
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.selected);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.selected.gamma_multiply(0.45));
    }
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(6.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    child.spacing_mut().item_spacing.x = 6.0;
    add(&mut child);
    resp
}

/// A small rounded count over the top-right corner of `rect`.
pub fn badge(ui: &egui::Ui, rect: egui::Rect, text: &str, color: Color32) {
    let font = bold(9.5);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let size = Vec2::new((galley.size().x + 8.0).max(15.0), 15.0);
    let r = egui::Rect::from_center_size(egui::pos2(rect.right() - 3.0, rect.top() + 1.0), size);
    ui.painter().rect_filled(r, CornerRadius::same(8), color);
    ui.painter().galley(r.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// A small grey section title (Tables, Connections …) in the sidebar.
pub fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).font(bold(11.0)).color(palette(ui).muted));
}

/// A table glyph: a rounded grid in `color`, drawn rather than taken from a font.
pub fn table_icon(ui: &mut egui::Ui, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), egui::Sense::hover());
    let r = egui::Rect::from_center_size(rect.center(), Vec2::new(14.0, 11.0));
    let painter = ui.painter();
    let stroke = Stroke::new(1.2, color);
    painter.rect_stroke(r, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);
    painter.rect_filled(egui::Rect::from_min_max(r.min, egui::pos2(r.max.x, r.min.y + 3.5)), CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 }, color);
    let y = r.min.y + 3.5 + (r.height() - 3.5) / 2.0;
    painter.hline(r.x_range(), y, stroke);
    let x = r.min.x + r.width() * 0.4;
    painter.vline(x, egui::Rangef::new(r.min.y + 3.5, r.max.y), stroke);
    resp
}

/// A database glyph: a small cylinder in `color`.
pub fn database_icon(ui: &mut egui::Ui, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), egui::Sense::hover());
    paint_glyph(ui.painter(), Glyph::Database, rect.center(), 1.0, color);
    resp
}

/// Simple drawn icons, so they don't depend on what the fonts hold.
#[derive(Clone, Copy)]
pub enum Glyph {
    Database,
    Plus,
    Folder,
    Arrow,
}

/// `glyph` centred on `c`, `scale` times its 16 px size.
pub fn paint_glyph(painter: &egui::Painter, glyph: Glyph, c: egui::Pos2, scale: f32, color: Color32) {
    let stroke = Stroke::new(1.3 * scale.sqrt(), color);
    let v = |x: f32, y: f32| c + Vec2::new(x, y) * scale;
    match glyph {
        Glyph::Database => {
            let (w, h, ry) = (6.0 * scale, 5.4 * scale, 2.0 * scale);
            painter.add(egui::epaint::EllipseShape::stroke(c - Vec2::new(0.0, h), Vec2::new(w, ry), stroke));
            for dy in [0.0, h] {
                // The lower arcs: the bottom half of an ellipse.
                let pts: Vec<egui::Pos2> =
                    (0..=16).map(|i| std::f32::consts::PI * i as f32 / 16.0).map(|a| egui::pos2(c.x + w * a.cos(), c.y + dy + ry * a.sin())).collect();
                painter.add(egui::Shape::line(pts, stroke));
            }
            painter.vline(c.x - w, egui::Rangef::new(c.y - h, c.y + h), stroke);
            painter.vline(c.x + w, egui::Rangef::new(c.y - h, c.y + h), stroke);
        }
        Glyph::Plus => {
            painter.line_segment([v(-6.0, 0.0), v(6.0, 0.0)], stroke);
            painter.line_segment([v(0.0, -6.0), v(0.0, 6.0)], stroke);
        }
        Glyph::Folder => {
            let pts = vec![v(-7.0, -5.0), v(-2.5, -5.0), v(-1.0, -3.0), v(7.0, -3.0), v(7.0, 5.5), v(-7.0, 5.5)];
            painter.add(egui::Shape::closed_line(pts, stroke));
            painter.line_segment([v(-7.0, -1.0), v(7.0, -1.0)], stroke);
        }
        Glyph::Arrow => {
            painter.line_segment([v(-6.0, 0.0), v(6.0, 0.0)], stroke);
            painter.add(egui::Shape::line(vec![v(1.5, -4.5), v(6.0, 0.0), v(1.5, 4.5)], stroke));
        }
    }
}

/// A large clickable card: `glyph` on a rounded square of `tint`, a title
/// and a line under it. Fills the width it is given.
pub fn card(ui: &mut egui::Ui, glyph: Glyph, tint: Color32, title: &str, subtitle: &str) -> egui::Response {
    let height = if subtitle.is_empty() { 44.0 } else { 54.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), egui::Sense::click());
    let p = palette(ui);
    let w = ui.style().interact(&resp);
    let fill = if resp.hovered() { p.selected.gamma_multiply(0.6) } else { Color32::TRANSPARENT };
    ui.painter().rect(rect, CornerRadius::same(10), fill, if resp.hovered() { w.bg_stroke } else { Stroke::NONE }, egui::StrokeKind::Inside);
    let icon = egui::Rect::from_min_size(rect.left_center() + Vec2::new(8.0, -17.0), Vec2::splat(34.0));
    ui.painter().rect_filled(icon, CornerRadius::same(8), tint);
    paint_glyph(ui.painter(), glyph, icon.center(), 1.3, Color32::WHITE);
    let x = icon.right() + 12.0;
    let text = ui.visuals().text_color();
    // One line each, cut with "…" when too long.
    let line = |text: &str, font: FontId, color: Color32| {
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.right() - x - 10.0);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let title_galley = line(title, bold(14.0), text);
    let sub_galley = (!subtitle.is_empty()).then(|| line(subtitle, FontId::proportional(11.5), p.muted));
    let total = title_galley.size().y + sub_galley.as_ref().map_or(0.0, |g| g.size().y + 2.0);
    let mut y = rect.center().y - total / 2.0;
    let th = title_galley.size().y;
    ui.painter().galley(egui::pos2(x, y), title_galley, text);
    if let Some(g) = sub_galley {
        y += th + 2.0;
        ui.painter().galley(egui::pos2(x, y), g, p.muted);
    }
    resp
}

/// A padlock centred on `c`, open when not `locked`.
pub fn paint_lock(ui: &egui::Ui, c: egui::Pos2, locked: bool, color: Color32) {
    let painter = ui.painter();
    let body = egui::Rect::from_center_size(c + Vec2::new(0.0, 2.5), Vec2::new(11.0, 8.0));
    painter.rect_filled(body, CornerRadius::same(2), color);
    // The shackle: a half circle over two legs, the right leg lifted out when open.
    let (r, top) = (3.2, body.top() - 3.0);
    let cx = if locked { c.x } else { c.x + 4.5 };
    let arc: Vec<egui::Pos2> = (0..=12).map(|i| std::f32::consts::PI * i as f32 / 12.0).map(|a| egui::pos2(cx - r * a.cos(), top - r * a.sin())).collect();
    let stroke = Stroke::new(1.6, color);
    painter.add(egui::Shape::line(arc, stroke));
    painter.vline(cx - r, egui::Rangef::new(top, body.top()), stroke);
    if locked {
        painter.vline(cx + r, egui::Rangef::new(top, body.top()), stroke);
    }
}

/// Shade a grid header cell, gaps included, so the header reads as one bar.
pub fn header_cell(ui: &mut egui::Ui) {
    let gap = ui.spacing().item_spacing.x;
    let fill = if ui.visuals().dark_mode { Color32::from_rgb(0x2a, 0x2a, 0x2c) } else { Color32::from_gray(0xf3) };
    ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(gap, 2.0)), 0.0, fill);
}

/// A grid's column title.
pub fn header_text(text: impl Into<String>) -> RichText {
    RichText::new(text).font(bold(12.0))
}

/// A grid's row number, right-aligned and faint.
pub fn row_number(ui: &mut egui::Ui, text: RichText) -> egui::Response {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.add(egui::Label::new(text).selectable(false).sense(egui::Sense::click())))
        .inner
}
