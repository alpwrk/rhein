use std::rc::Rc;

use gpui::{
    App, ClickEvent, ElementId, FontWeight, IntoElement, MouseButton, ParentElement as _, RenderOnce,
    SharedString, Styled as _, Window, div, prelude::*, px,
};

use crate::hover::{Paint, hover_fade, hover_fade_with};
use crate::icon::{Icon, icon};
use crate::theme::{ActiveTheme as _, radius, size, text};
use crate::tooltip::tooltip;

type Handler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonKind {
    Primary,
    #[default]
    Secondary,
    /// No background until hovered; for toolbars and quiet actions.
    Ghost,
    /// For actions that end or remove something.
    Danger,
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<Icon>,
    kind: ButtonKind,
    disabled: bool,
    full_width: bool,
    small: bool,
    tooltip: Option<SharedString>,
    on_click: Option<Handler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: Some(label.into()),
            icon: None,
            kind: ButtonKind::Secondary,
            disabled: false,
            full_width: false,
            small: false,
            tooltip: None,
            on_click: None,
        }
    }

    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn primary(self) -> Self {
        self.kind(ButtonKind::Primary)
    }

    pub fn danger(self) -> Self {
        self.kind(ButtonKind::Danger)
    }

    pub fn ghost(self) -> Self {
        self.kind(ButtonKind::Ghost)
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// The compact height, for buttons inside list rows and inline notices.
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }

    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    pub fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let (bg, hover, fg) = match self.kind {
            ButtonKind::Primary => (t.accent, t.accent_hover, t.text_on_accent),
            ButtonKind::Secondary => (gpui::transparent_black().into(), t.bg_hover, t.text),
            ButtonKind::Ghost => (gpui::transparent_black().into(), t.bg_hover, t.text_muted),
            ButtonKind::Danger => (t.danger, t.danger, gpui::white().into()),
        };
        let hover_fg = if self.kind == ButtonKind::Ghost { t.text } else { fg };
        // Held down: a step past hover (filled buttons darken, quiet ones take the selection tint).
        let press_bg = match self.kind {
            ButtonKind::Primary | ButtonKind::Danger => crate::hover::darken(hover, 0.12),
            _ => t.bg_selected,
        };
        let disabled = self.disabled;
        let id = self.id.clone();
        let ring = t.link;
        let el = div()
            .id(self.id)
            .h(if self.small { size::CONTROL_SM } else { size::CONTROL })
            .flex_none()
            .px(if self.small { px(10.) } else { px(14.) })
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .rounded(radius::MD)
            .when(self.kind == ButtonKind::Secondary, |d| d.border_1().border_color(t.border_strong))
            .text_size(if self.small { text::SM } else { text::MD })
            .font_weight(FontWeight::MEDIUM)
            .when(self.full_width, |d| d.w_full())
            .when(disabled, |d| d.opacity(0.5).bg(bg).text_color(fg))
            .when(!disabled, |d| d.cursor_pointer().tab_index(0).focus_visible(|s| s.border_2().border_color(ring)));
        let el = if disabled {
            el
        } else {
            let (rest, lit) = (Paint { bg, text: fg }, Paint { bg: hover, text: hover_fg });
            hover_fade(el, &id, rest, lit, Paint { bg: press_bg, text: hover_fg }, window, cx)
        };
        el
            .when_some(self.tooltip, |d, text| d.tooltip(tooltip(text)))
            .when_some(self.icon, |d, i| d.child(icon(i).text_color(fg)))
            .when_some(self.label, |d, label| d.child(label))
            .when_some(self.on_click.filter(|_| !disabled), |d, handler| {
                d.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |event, window, cx| handler(event, window, cx))
            })
    }
}

#[derive(IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon: Icon,
    tooltip: Option<SharedString>,
    active: bool,
    danger: bool,
    large: bool,
    disabled: bool,
    on_click: Option<Handler>,
}

impl IconButton {
    pub fn new(id: impl Into<ElementId>, icon: Icon) -> Self {
        Self { id: id.into(), icon, tooltip: None, active: false, danger: false, large: false, disabled: false, on_click: None }
    }

    /// Dimmed and not pressable (its tooltip still says why).
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Shown in the accent colour, for toggles that are on.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// The larger size, for headers and the composer.
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }

    /// Shown in the danger colour, e.g. a muted microphone.
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    pub fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = cx.theme();
        let color = if self.danger {
            t.danger
        } else if self.active {
            t.accent
        } else {
            t.text_muted
        };
        let hover_color = if self.danger || self.active { color } else { t.text };
        let id = self.id.clone();
        let clear: gpui::Rgba = gpui::transparent_black().into();
        let ring = t.link;
        let el = div()
            .id(self.id)
            .size(if self.large { size::ICON_BUTTON_LG } else { size::ICON_BUTTON })
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(radius::MD)
            .cursor_pointer()
            .when(!self.disabled, |d| d.tab_index(0).focus_visible(|s| s.border_2().border_color(ring)));
        let (rest, lit, held) = (
            Paint { bg: clear, text: color },
            Paint { bg: t.bg_hover, text: hover_color },
            Paint { bg: t.bg_selected, text: hover_color },
        );
        let (el, now) = hover_fade_with(el, &id, rest, lit, held, window, cx);
        let disabled = self.disabled;
        el.when_some(self.tooltip, |d, text| d.tooltip(tooltip(text)))
            .when(disabled, |d| d.opacity(0.45).cursor_default())
            .child(icon(self.icon).text_color(now.text))
            .when_some(self.on_click.filter(|_| !disabled), |d, handler| {
                d.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |event, window, cx| handler(event, window, cx))
            })
    }
}
