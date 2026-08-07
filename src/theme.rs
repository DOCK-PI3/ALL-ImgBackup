use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2};

pub const BG: Color32 = Color32::from_rgb(7, 11, 23);
pub const SURFACE: Color32 = Color32::from_rgb(15, 22, 40);
pub const SURFACE_2: Color32 = Color32::from_rgb(22, 31, 52);
pub const BORDER: Color32 = Color32::from_rgb(42, 55, 82);
pub const TEXT: Color32 = Color32::from_rgb(239, 244, 255);
pub const MUTED: Color32 = Color32::from_rgb(142, 157, 184);
pub const BLUE: Color32 = Color32::from_rgb(90, 122, 255);
pub const CYAN: Color32 = Color32::from_rgb(89, 225, 210);
pub const RED: Color32 = Color32::from_rgb(255, 101, 124);
pub const AMBER: Color32 = Color32::from_rgb(255, 190, 92);
pub const GREEN: Color32 = Color32::from_rgb(74, 222, 154);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.visuals.dark_mode = true;
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = SURFACE;
    style.visuals.extreme_bg_color = Color32::from_rgb(9, 14, 27);
    style.visuals.faint_bg_color = SURFACE_2;
    style.visuals.widgets.noninteractive.bg_fill = SURFACE;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.bg_fill = SURFACE_2;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(31, 44, 73);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, BLUE);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(42, 55, 94);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, CYAN);
    style.visuals.selection.bg_fill = BLUE.gamma_multiply(0.65);
    style.visuals.selection.stroke = Stroke::new(1.0, CYAN);
    style.visuals.window_corner_radius = CornerRadius::same(18);
    style.visuals.menu_corner_radius = CornerRadius::same(12);
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(16.0, 10.0);
    style.spacing.interact_size.y = 42.0;
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(28.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        ),
    ]
    .into();
    ctx.set_global_style(style);
}
