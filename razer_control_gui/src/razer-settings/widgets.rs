use gtk4 as gtk;
use libadwaita as adw;
use gtk::prelude::*;
use adw::prelude::*;
use std::rc::Rc;
use std::cell::{Cell, RefCell};
use crate::comms::FanCurvePoint;

pub struct SettingsPage {
    pub page: adw::PreferencesPage,
}

impl SettingsPage {
    pub fn new() -> Self {
        SettingsPage {
            page: adw::PreferencesPage::new(),
        }
    }

    pub fn add_section(&self, title: Option<&str>) -> SettingsSection {
        let group = adw::PreferencesGroup::new();
        if let Some(t) = title {
            group.set_title(t);
        }
        self.page.add(&group);
        SettingsSection { group }
    }
}

pub struct SettingsSection {
    pub group: adw::PreferencesGroup,
}

impl SettingsSection {
    pub fn add_row(&self, row: &impl IsA<gtk::Widget>) {
        self.group.add(row);
    }
}

pub struct SettingsRow {
    pub row: adw::ActionRow,
}

impl SettingsRow {
    pub fn new(title: &str, widget: &impl IsA<gtk::Widget>) -> Self {
        let row = adw::ActionRow::new();
        row.set_title(title);
        row.add_suffix(widget);
        SettingsRow { row }
    }

    pub fn set_subtitle(&self, subtitle: &str) {
        self.row.set_subtitle(subtitle);
    }
}

/// A row specifically designed for sliders/scales that need full width
pub struct SliderRow {
    pub container: gtk::Box,
    pub scale: gtk::Scale,
}

impl SliderRow {
    pub fn new(title: &str, subtitle: &str, min: f64, max: f64, step: f64, value: f64) -> Self {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
        container.set_margin_top(12);
        container.set_margin_bottom(12);
        container.set_margin_start(12);
        container.set_margin_end(12);

        // Title label
        let title_label = gtk::Label::new(Some(title));
        title_label.set_halign(gtk::Align::Start);
        title_label.add_css_class("heading");
        container.append(&title_label);

        // Subtitle label
        if !subtitle.is_empty() {
            let subtitle_label = gtk::Label::new(Some(subtitle));
            subtitle_label.set_halign(gtk::Align::Start);
            subtitle_label.add_css_class("dim-label");
            subtitle_label.add_css_class("caption");
            container.append(&subtitle_label);
        }

        // Scale widget
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
        scale.set_value(value);
        scale.set_hexpand(true);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_digits(0);
        container.append(&scale);

        SliderRow { container, scale }
    }

    pub fn add_mark(&self, value: f64, label: Option<&str>) {
        self.scale.add_mark(value, gtk::PositionType::Bottom, label);
    }
}

/// Helper to create an adw::ComboRow with a StringList model
pub fn make_combo_row(title: &str, subtitle: &str, options: &[&str], active: u32) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title(title);
    row.set_subtitle(subtitle);
    let model = gtk::StringList::new(options);
    row.set_model(Some(&model));
    row.set_selected(active);
    row
}

/// Helper to create an adw::SwitchRow
pub fn make_switch_row(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_active(active);
    row
}

/// A row with a color dialog button (replaces deprecated ColorButton)
pub struct ColorRow {
    pub row: adw::ActionRow,
    pub button: gtk::ColorDialogButton,
}

impl ColorRow {
    pub fn new(title: &str, subtitle: &str) -> Self {
        let row = adw::ActionRow::new();
        row.set_title(title);
        row.set_subtitle(subtitle);

        let dialog = gtk::ColorDialog::new();
        dialog.set_with_alpha(false);
        let button = gtk::ColorDialogButton::new(Some(dialog));
        button.set_valign(gtk::Align::Center);
        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));

        ColorRow { row, button }
    }
}

/// AC/Battery profile toggle — two linked ToggleButtons sharing an Rc<Cell<bool>> for the current AC state.
/// Returns (toggle_box, is_ac) where toggle_box is the widget to insert and is_ac tracks the state.
pub fn make_profile_toggle() -> (gtk::Box, Rc<Cell<bool>>) {
    let on_ac = super::util::check_if_running_on_ac_power().unwrap_or(true);
    let is_ac = Rc::new(Cell::new(on_ac));

    let ac_btn = gtk::ToggleButton::with_label("AC Power");
    let bat_btn = gtk::ToggleButton::with_label("Battery");
    bat_btn.set_group(Some(&ac_btn));

    if on_ac {
        ac_btn.set_active(true);
    } else {
        bat_btn.set_active(true);
    }

    let toggle_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    toggle_box.add_css_class("linked");
    toggle_box.set_halign(gtk::Align::Center);
    toggle_box.set_margin_top(8);
    toggle_box.set_margin_bottom(8);
    toggle_box.append(&ac_btn);
    toggle_box.append(&bat_btn);

    // Store references for the callback
    let is_ac_ref = is_ac.clone();
    ac_btn.connect_toggled(move |btn| {
        if btn.is_active() {
            is_ac_ref.set(true);
        }
    });
    let is_ac_ref = is_ac.clone();
    bat_btn.connect_toggled(move |btn| {
        if btn.is_active() {
            is_ac_ref.set(false);
        }
    });

    (toggle_box, is_ac)
}

// --- Blade 16 2025 power profiles (real Synapse names), partitioned by domain ---
// Dropdown index != EC wire value; map through these helpers. Order = dropdown
// order; Balanced differs by domain (AC=0, DC=6). Wires 1/7 appear only behind
// the experimental unlock, APPENDED so default indices stay stable.

/// Exposed profiles for a power domain as (Synapse name, EC wire value).
pub fn power_profiles(ac: bool, experimental: bool) -> &'static [(&'static str, u8)] {
    match (ac, experimental) {
        (true, false) => &[("Balanced", 0), ("Performance", 2), ("Silent", 5), ("Custom", 4)],
        (true, true) => &[
            ("Balanced", 0),
            ("Performance", 2),
            ("Silent", 5),
            ("Custom", 4),
            ("Gaming (legacy)", 1),
            ("HyperBoost", 7),
        ],
        // Battery: Synapse parity, no experimental additions.
        (false, _) => &[("Balanced", 6), ("Battery Saver", 3)],
    }
}

/// Display names for building the ComboRow model.
pub fn power_profile_names(ac: bool, experimental: bool) -> Vec<&'static str> {
    power_profiles(ac, experimental).iter().map(|(n, _)| *n).collect()
}

/// Dropdown index -> EC wire value (falls back to that domain's Balanced).
pub fn profile_index_to_wire(ac: bool, experimental: bool, index: u32) -> u8 {
    power_profiles(ac, experimental)
        .get(index as usize)
        .map(|(_, w)| *w)
        .unwrap_or(if ac { 0 } else { 6 })
}

/// EC wire value -> dropdown index (falls back to 0 = Balanced).
pub fn profile_wire_to_index(ac: bool, experimental: bool, wire: u8) -> u32 {
    power_profiles(ac, experimental)
        .iter()
        .position(|(_, w)| *w == wire)
        .unwrap_or(0) as u32
}

/// Replace a ComboRow's options in place (caller sets the selection afterwards).
pub fn set_combo_options(row: &adw::ComboRow, options: &[&str]) {
    row.set_model(Some(&gtk::StringList::new(options)));
}

/// Human-readable description keyed on the EC **wire value** (not dropdown index).
pub fn profile_description(wire: u32) -> &'static str {
    match wire {
        0 => "Balanced performance and acoustics",
        6 => "Good mix of performance and battery life",
        2 => "Maximum performance, higher power draw",
        3 => "Extends battery life, lower power draw",
        4 => "Manually tune CPU and GPU levels",
        5 => "Minimal fan noise, reduced performance",
        1 => "Legacy ghost slot — measured \u{2248} Performance alias",
        7 => "HyperBoost, 175 W: Blade 18 native; needs the cooling pad on the 16",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Smart fan-curve editor (custom DrawingArea widget)
// ---------------------------------------------------------------------------

const CURVE_LEFT: f64 = 44.0;
const CURVE_RIGHT: f64 = 12.0;
const CURVE_TOP: f64 = 12.0;
const CURVE_BOTTOM: f64 = 24.0;
const CURVE_HANDLE_RADIUS: f64 = 5.0;
const CURVE_HIT_RADIUS: f64 = 16.0;

fn temp_to_x(temp: f64, w: f64) -> f64 {
    CURVE_LEFT + (temp / 100.0) * (w - CURVE_LEFT - CURVE_RIGHT)
}

fn rpm_to_y(rpm: f64, h: f64, min: f64, max: f64) -> f64 {
    let span = (max - min).max(1.0);
    CURVE_TOP + (1.0 - (rpm - min) / span) * (h - CURVE_TOP - CURVE_BOTTOM)
}

fn x_to_temp(x: f64, w: f64) -> f64 {
    (((x - CURVE_LEFT) / (w - CURVE_LEFT - CURVE_RIGHT)) * 100.0).clamp(0.0, 100.0)
}

fn y_to_rpm(y: f64, h: f64, min: f64, max: f64) -> f64 {
    let span = (max - min).max(1.0);
    (min + (1.0 - (y - CURVE_TOP) / (h - CURVE_TOP - CURVE_BOTTOM)) * span).clamp(min, max)
}

fn nearest_curve_point(pts: &[FanCurvePoint], x: f64, y: f64, w: f64, h: f64, min: f64, max: f64) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, p) in pts.iter().enumerate() {
        let px = temp_to_x(f64::from(p.temp_c), w);
        let py = rpm_to_y(f64::from(p.rpm), h, min, max);
        let dist = ((px - x).powi(2) + (py - y).powi(2)).sqrt();
        if dist <= CURVE_HIT_RADIUS && best.map_or(true, |(_, bd)| dist < bd) {
            best = Some((i, dist));
        }
    }
    best.map(|(i, _)| i)
}

fn draw_curve(cr: &gtk::cairo::Context, w: f64, h: f64, pts: &[FanCurvePoint], min: f64, max: f64) {
    cr.set_line_width(1.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
    for t in (0..=100).step_by(20) {
        let x = temp_to_x(f64::from(t), w);
        cr.move_to(x, CURVE_TOP);
        cr.line_to(x, h - CURVE_BOTTOM);
    }
    for i in 0..=4 {
        let rpm = min + (max - min) * (f64::from(i) / 4.0);
        let y = rpm_to_y(rpm, h, min, max);
        cr.move_to(CURVE_LEFT, y);
        cr.line_to(w - CURVE_RIGHT, y);
    }
    let _ = cr.stroke();

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.5);
    cr.set_font_size(10.0);
    for t in (0..=100).step_by(20) {
        let x = temp_to_x(f64::from(t), w);
        cr.move_to(x - 8.0, h - CURVE_BOTTOM + 14.0);
        let _ = cr.show_text(&format!("{}\u{00B0}", t));
    }
    for i in 0..=4 {
        let rpm = min + (max - min) * (f64::from(i) / 4.0);
        let y = rpm_to_y(rpm, h, min, max);
        cr.move_to(4.0, y + 3.0);
        let _ = cr.show_text(&format!("{}", rpm.round() as i32));
    }

    if pts.is_empty() {
        return;
    }

    cr.set_line_width(2.0);
    cr.set_source_rgba(0.4, 0.6, 1.0, 0.9);
    let first = &pts[0];
    cr.move_to(CURVE_LEFT, rpm_to_y(f64::from(first.rpm), h, min, max));
    for p in pts {
        cr.line_to(temp_to_x(f64::from(p.temp_c), w), rpm_to_y(f64::from(p.rpm), h, min, max));
    }
    let last = &pts[pts.len() - 1];
    cr.line_to(w - CURVE_RIGHT, rpm_to_y(f64::from(last.rpm), h, min, max));
    let _ = cr.stroke();

    cr.set_source_rgba(0.55, 0.72, 1.0, 1.0);
    for p in pts {
        let px = temp_to_x(f64::from(p.temp_c), w);
        let py = rpm_to_y(f64::from(p.rpm), h, min, max);
        cr.arc(px, py, CURVE_HANDLE_RADIUS, 0.0, std::f64::consts::PI * 2.0);
        let _ = cr.fill();
    }
}

type CurveChangeCb = Rc<RefCell<Option<Box<dyn Fn(Vec<FanCurvePoint>)>>>>;

fn fire_curve_change(cb: &CurveChangeCb, pts: &[FanCurvePoint]) {
    if let Some(f) = cb.borrow().as_ref() {
        f(pts.to_vec());
    }
}

/// A graphical fan-curve editor: temperature (x, 0-100 °C) vs RPM (y, model
/// min-max). Drag a point to move it, left-click empty space to add one,
/// right-click a point to remove it (minimum two points). Edits fire the
/// `connect_changed` callback with the new, temperature-sorted point list.
pub struct CurveEditor {
    pub widget: gtk::DrawingArea,
    points: Rc<RefCell<Vec<FanCurvePoint>>>,
    on_change: CurveChangeCb,
}

impl CurveEditor {
    pub fn new(rpm_min: u16, rpm_max: u16, initial: Vec<FanCurvePoint>) -> Self {
        let area = gtk::DrawingArea::new();
        area.set_content_height(220);
        area.set_hexpand(true);
        area.set_margin_top(8);
        area.set_margin_bottom(8);
        area.set_margin_start(12);
        area.set_margin_end(12);

        let points = Rc::new(RefCell::new(initial));
        let on_change: CurveChangeCb = Rc::new(RefCell::new(None));
        let min = f64::from(rpm_min);
        let max = f64::from(rpm_max);

        {
            let points = points.clone();
            area.set_draw_func(move |_, cr, w, h| {
                draw_curve(cr, f64::from(w), f64::from(h), &points.borrow(), min, max);
            });
        }

        let drag_idx = Rc::new(Cell::new(None::<usize>));
        let drag = gtk::GestureDrag::new();
        {
            let points = points.clone();
            let drag_idx = drag_idx.clone();
            let area_ref = area.clone();
            drag.connect_drag_begin(move |_, x, y| {
                let w = f64::from(area_ref.width());
                let h = f64::from(area_ref.height());
                drag_idx.set(nearest_curve_point(&points.borrow(), x, y, w, h, min, max));
            });
        }
        {
            let points = points.clone();
            let drag_idx = drag_idx.clone();
            let area_ref = area.clone();
            drag.connect_drag_update(move |g, ox, oy| {
                let idx = match drag_idx.get() {
                    Some(i) => i,
                    None => return,
                };
                let (sx, sy) = match g.start_point() {
                    Some(p) => p,
                    None => return,
                };
                let w = f64::from(area_ref.width());
                let h = f64::from(area_ref.height());
                let mut pts = points.borrow_mut();
                let len = pts.len();
                let lo = if idx > 0 { f64::from(pts[idx - 1].temp_c) + 1.0 } else { 0.0 };
                let hi = if idx + 1 < len { f64::from(pts[idx + 1].temp_c) - 1.0 } else { 100.0 };
                let temp = x_to_temp(sx + ox, w).clamp(lo, hi.max(lo));
                let rpm = y_to_rpm(sy + oy, h, min, max);
                pts[idx].temp_c = temp.round() as u8;
                pts[idx].rpm = rpm.round() as u16;
                drop(pts);
                area_ref.queue_draw();
            });
        }
        {
            let points = points.clone();
            let drag_idx = drag_idx.clone();
            let on_change = on_change.clone();
            drag.connect_drag_end(move |_, _, _| {
                if drag_idx.get().is_some() {
                    drag_idx.set(None);
                    fire_curve_change(&on_change, &points.borrow());
                }
            });
        }
        area.add_controller(drag);

        let click = gtk::GestureClick::new();
        click.set_button(0); // listen to all buttons
        {
            let points = points.clone();
            let on_change = on_change.clone();
            let area_ref = area.clone();
            click.connect_pressed(move |g, _n, x, y| {
                let w = f64::from(area_ref.width());
                let h = f64::from(area_ref.height());
                let near = nearest_curve_point(&points.borrow(), x, y, w, h, min, max);
                match g.current_button() {
                    3 => {
                        if let Some(idx) = near {
                            let mut pts = points.borrow_mut();
                            if pts.len() > 2 {
                                pts.remove(idx);
                            }
                            drop(pts);
                            area_ref.queue_draw();
                            fire_curve_change(&on_change, &points.borrow());
                        }
                    }
                    1 => {
                        if near.is_none() {
                            let temp = x_to_temp(x, w).round() as u8;
                            let rpm = y_to_rpm(y, h, min, max).round() as u16;
                            let mut pts = points.borrow_mut();
                            pts.push(FanCurvePoint { temp_c: temp, rpm });
                            pts.sort_by_key(|p| p.temp_c);
                            drop(pts);
                            area_ref.queue_draw();
                            fire_curve_change(&on_change, &points.borrow());
                        }
                    }
                    _ => {}
                }
            });
        }
        area.add_controller(click);

        CurveEditor { widget: area, points, on_change }
    }

    pub fn set_points(&self, pts: Vec<FanCurvePoint>) {
        *self.points.borrow_mut() = pts;
        self.widget.queue_draw();
    }

    pub fn connect_changed<F: Fn(Vec<FanCurvePoint>) + 'static>(&self, f: F) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }
}
