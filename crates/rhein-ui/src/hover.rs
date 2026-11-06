//! Hover and press that ease instead of snapping. GPUI's `.hover()` swaps styles in one frame;
//! `hover_fade` keeps how hovered and how pressed an element is (0..1) in keyed element state,
//! moves both towards where the pointer says they should be over about 90 ms, and blends the
//! background and text colours by them. With reduce motion the levels jump.

use std::time::Instant;

use gpui::{App, ElementId, InteractiveElement as _, MouseButton, Rgba, StatefulInteractiveElement as _, Styled as _, Window};

/// How long a full fade takes.
const FADE_SECS: f32 = 0.09;

/// The colours of an element at rest, hovered and pressed.
#[derive(Clone, Copy)]
pub struct Paint {
    pub bg: Rgba,
    pub text: Rgba,
}

#[doc(hidden)]
pub struct Levels {
    hovered: bool,
    pressed: bool,
    hover: f32,
    press: f32,
    last: Option<Instant>,
}

/// `color` a little darker (`amount` of the way to black), keeping its alpha.
pub fn darken(color: Rgba, amount: f32) -> Rgba {
    Rgba { r: color.r * (1. - amount), g: color.g * (1. - amount), b: color.b * (1. - amount), a: color.a }
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    // A fully clear end takes the other's hue, so fading in from nothing does not pass through
    // grey (which shows on a light surface).
    let (a, b) = match (a.a == 0., b.a == 0.) {
        (true, false) => (Rgba { a: 0., ..b }, b),
        (false, true) => (a, Rgba { a: 0., ..a }),
        _ => (a, b),
    };
    Rgba { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: a.a + (b.a - a.a) * t }
}

fn approach(level: &mut f32, target: bool, step: f32) -> bool {
    let goal = if target { 1. } else { 0. };
    if (*level - goal).abs() <= step {
        *level = goal;
        false
    } else {
        *level += if target { step } else { -step };
        true
    }
}

/// Gives `el` eased hover and press colours: `rest` normally, `hover` under the pointer,
/// `press` while the button is held.
pub fn hover_fade(el: gpui::Stateful<gpui::Div>, id: &ElementId, rest: Paint, hover: Paint, press: Paint, window: &mut Window, cx: &mut App) -> gpui::Stateful<gpui::Div> {
    hover_fade_with(el, id, rest, hover, press, window, cx).0
}

/// The same, also returning the colours of this frame (for children that do not inherit them,
/// such as icons).
pub fn hover_fade_with(el: gpui::Stateful<gpui::Div>, id: &ElementId, rest: Paint, hover: Paint, press: Paint, window: &mut Window, cx: &mut App) -> (gpui::Stateful<gpui::Div>, Paint) {
    let state = window.use_keyed_state(ElementId::from((id.clone(), "hover-fade")), cx, |_, _| Levels {
        hovered: false,
        pressed: false,
        hover: 0.,
        press: 0.,
        last: None,
    });
    let (hover_level, press_level) = state.update(cx, |s, cx| {
        let now = Instant::now();
        let step = if cx.reduce_motion() {
            1.
        } else {
            s.last.map_or(0., |at| now.duration_since(at).as_secs_f32() / FADE_SECS).min(1.)
        };
        let moving = approach(&mut s.hover, s.hovered, step) | approach(&mut s.press, s.pressed, step);
        s.last = moving.then_some(now);
        if moving {
            window.request_animation_frame();
        }
        (s.hover, s.press)
    });
    let paint = |a: Paint, b: Paint, t: f32| Paint { bg: mix(a.bg, b.bg, t), text: mix(a.text, b.text, t) };
    let now = paint(paint(rest, hover, hover_level), press, press_level);
    let (on_hover, on_down, on_up, on_up_out) = (state.clone(), state.clone(), state.clone(), state);
    // Repaint only the view this element is drawn by (and those around it), not the whole
    // window: `window.refresh()` also throws away every cached view.
    let view = window.current_view();
    let el = el
        .bg(now.bg)
        .text_color(now.text)
        .on_hover(move |hovered, _, cx| {
            on_hover.update(cx, |s, _| {
                s.hovered = *hovered;
                if !*hovered {
                    s.pressed = false;
                }
                s.last = Some(Instant::now());
            });
            cx.notify(view);
        })
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            on_down.update(cx, |s, _| {
                s.pressed = true;
                s.last = Some(Instant::now());
            });
            cx.notify(view);
        })
        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
            on_up.update(cx, |s, _| {
                s.pressed = false;
                s.last = Some(Instant::now());
            });
            cx.notify(view);
        })
        .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
            on_up_out.update(cx, |s, _| s.pressed = false);
            cx.notify(view);
        });
    (el, now)
}

/// The keyboard focus ring: a 2px halo outside the element, so showing it moves nothing.
pub fn focus_ring(s: gpui::StyleRefinement, color: Rgba) -> gpui::StyleRefinement {
    s.shadow(vec![gpui::BoxShadow {
        color: color.into(),
        offset: gpui::point(gpui::px(0.), gpui::px(0.)),
        blur_radius: gpui::px(0.),
        spread_radius: gpui::px(2.),
        inset: false,
    }])
}
