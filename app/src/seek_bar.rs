//! The seek bar (#8): the track's envelope drawn as a hull curve, played part
//! in the theme's foreground colour and the rest dimmed. Without an envelope
//! it is a plain slider. Clicking or dragging seeks; the arrow keys step five
//! seconds. Colours come from the theme, never from code.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

/// Step of the arrow keys, in milliseconds.
const KEY_STEP_MS: u64 = 5_000;

/// Height of the waveform.
const HEIGHT: i32 = 48;

type SeekHandler = Rc<dyn Fn(u64)>;

#[derive(Clone)]
pub(crate) struct SeekBar {
    /// What the window packs: the waveform or the slider.
    pub(crate) widget: gtk::Stack,
    area: gtk::DrawingArea,
    slider: gtk::Scale,
    state: Rc<State>,
}

struct State {
    peaks: RefCell<Vec<u8>>,
    position_ms: Cell<u64>,
    length_ms: Cell<u64>,
    /// While the user drags, the position follows the pointer, not playback.
    dragging: Cell<Option<f64>>,
    /// Set while the slider is moved by code, so it does not seek.
    updating: Cell<bool>,
    on_seek: RefCell<Option<SeekHandler>>,
}

impl SeekBar {
    pub(crate) fn new() -> Self {
        let state = Rc::new(State {
            peaks: RefCell::default(),
            position_ms: Cell::new(0),
            length_ms: Cell::new(0),
            dragging: Cell::new(None),
            updating: Cell::new(false),
            on_seek: RefCell::default(),
        });
        let area = gtk::DrawingArea::builder()
            .content_height(HEIGHT)
            .hexpand(true)
            .focusable(true)
            .can_focus(true)
            .tooltip_text("Position: click or drag to seek")
            .build();
        area.update_property(&[gtk::accessible::Property::Label("Position")]);
        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        slider.set_hexpand(true);
        slider.set_draw_value(false);
        slider.update_property(&[gtk::accessible::Property::Label("Position")]);

        let widget = gtk::Stack::new();
        widget.add_named(&slider, Some("slider"));
        widget.add_named(&area, Some("wave"));
        widget.set_visible_child_name("slider");

        let draw_state = Rc::clone(&state);
        area.set_draw_func(move |area, cr, width, height| {
            draw(area, cr, width, height, &draw_state)
        });

        let drag = gtk::GestureDrag::new();
        {
            let state = Rc::clone(&state);
            let area_ref = area.clone();
            drag.connect_drag_begin(move |_, x, _| {
                area_ref.grab_focus();
                state.dragging.set(Some(fraction(x, area_ref.width())));
                area_ref.queue_draw();
            });
        }
        {
            let state = Rc::clone(&state);
            let area_ref = area.clone();
            drag.connect_drag_update(move |gesture, dx, _| {
                if let Some((x, _)) = gesture.start_point() {
                    state.dragging.set(Some(fraction(x + dx, area_ref.width())));
                    area_ref.queue_draw();
                }
            });
        }
        {
            let state = Rc::clone(&state);
            let area_ref = area.clone();
            drag.connect_drag_end(move |_, _, _| {
                if let Some(f) = state.dragging.take() {
                    let target = (f * state.length_ms.get() as f64) as u64;
                    state.position_ms.set(target);
                    state.seek(target);
                }
                area_ref.queue_draw();
            });
        }
        area.add_controller(drag);

        let keys = gtk::EventControllerKey::new();
        {
            let state = Rc::clone(&state);
            keys.connect_key_pressed(move |_, key, _, _| {
                let position = state.position_ms.get();
                let target = match key {
                    gdk::Key::Left => position.saturating_sub(KEY_STEP_MS),
                    gdk::Key::Right => (position + KEY_STEP_MS).min(state.length_ms.get()),
                    gdk::Key::Home => 0,
                    _ => return glib::Propagation::Proceed,
                };
                state.seek(target);
                glib::Propagation::Stop
            });
        }
        area.add_controller(keys);

        {
            let state = Rc::clone(&state);
            slider.connect_value_changed(move |slider| {
                if !state.updating.get() {
                    state.seek(slider.value().max(0.0) as u64);
                }
            });
        }

        Self {
            widget,
            area,
            slider,
            state,
        }
    }

    /// Called with the target position when the user seeks.
    pub(crate) fn connect_seek(&self, handler: impl Fn(u64) + 'static) {
        *self.state.on_seek.borrow_mut() = Some(Rc::new(handler));
    }

    /// Shows the envelope, or the plain slider for `None`.
    pub(crate) fn set_waveform(&self, peaks: Option<&[u8]>) {
        match peaks {
            Some(peaks) => {
                *self.state.peaks.borrow_mut() = peaks.to_vec();
                self.widget.set_visible_child_name("wave");
            }
            None => {
                self.state.peaks.borrow_mut().clear();
                self.widget.set_visible_child_name("slider");
            }
        }
        self.area.queue_draw();
    }

    /// Moves the bar to the playback position.
    pub(crate) fn set_position(&self, position_ms: u64, length_ms: u64) {
        self.state.position_ms.set(position_ms);
        self.state.length_ms.set(length_ms);
        self.state.updating.set(true);
        self.slider.set_range(0.0, length_ms.max(1) as f64);
        self.slider.set_value(position_ms as f64);
        self.state.updating.set(false);
        self.area.queue_draw();
    }
}

impl State {
    fn seek(&self, target_ms: u64) {
        let handler = self.on_seek.borrow().clone();
        if let Some(handler) = handler {
            handler(target_ms);
        }
    }
}

fn fraction(x: f64, width: i32) -> f64 {
    if width <= 0 {
        return 0.0;
    }
    (x / f64::from(width)).clamp(0.0, 1.0)
}

/// Draws the envelope mirrored around the middle line, one bar per pixel
/// column, with the played part opaque.
fn draw(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, width: i32, height: i32, state: &State) {
    let peaks = state.peaks.borrow();
    if peaks.is_empty() || width <= 0 || height <= 0 {
        return;
    }
    let colour = area.color();
    let played = state.dragging.get().unwrap_or_else(|| {
        let length = state.length_ms.get();
        if length == 0 {
            0.0
        } else {
            state.position_ms.get() as f64 / length as f64
        }
    });
    let middle = f64::from(height) / 2.0;
    let split = played * f64::from(width);
    for column in 0..width {
        let start = column as usize * peaks.len() / width as usize;
        let end = ((column as usize + 1) * peaks.len() / width as usize).max(start + 1);
        let peak = peaks[start..end.min(peaks.len())]
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        let half = (f64::from(peak) / 255.0 * middle).max(0.5);
        let alpha = if f64::from(column) < split { 1.0 } else { 0.35 };
        cr.set_source_rgba(
            f64::from(colour.red()),
            f64::from(colour.green()),
            f64::from(colour.blue()),
            f64::from(colour.alpha()) * alpha,
        );
        cr.rectangle(f64::from(column), middle - half, 1.0, half * 2.0);
        // Ignored: a failed fill only leaves a gap in the drawing.
        let _ = cr.fill();
    }
    // The play head, in the same colour.
    cr.set_source_rgba(
        f64::from(colour.red()),
        f64::from(colour.green()),
        f64::from(colour.blue()),
        f64::from(colour.alpha()),
    );
    cr.rectangle(
        split.floor().min(f64::from(width - 1)),
        0.0,
        2.0,
        f64::from(height),
    );
    // Ignored as above.
    let _ = cr.fill();
}
