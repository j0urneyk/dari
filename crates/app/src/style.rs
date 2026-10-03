//! Dari's look: the brand palette over gpui-kit's theme, extra icons, and shared building blocks.

#![allow(
    clippy::unreadable_literal,
    reason = "colors are written like CSS #RRGGBB"
)]

use std::borrow::Cow;

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _, StyledExt as _};
use gpui_kit::*;

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Clipboard,
        Clock,
        Command,
        Gauge,
        KeyRound,
        Laptop,
        Monitor,
        MonitorSmartphone,
        MousePointer2,
        Radar,
        ShieldCheck,
        Unplug,
        Waypoints,
    ]
);

/// The component icons plus the few extra Lucide icons Dari uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(data) => Ok(Some(data)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        Ok(paths)
    }
}

/// Applies the theme for the system appearance. Call after `gpui_kit::init`.
pub(crate) fn init(cx: &mut App) {
    apply(cx.window_appearance().into(), cx);
}

/// Follows the system's light or dark appearance while `window` is open.
pub(crate) fn follow_appearance<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
) -> Subscription {
    cx.observe_window_appearance(window, |_, window, cx| {
        apply(window.appearance().into(), cx);
    })
}

/// Loads gpui-kit's theme for `mode`, then paints it with Dari's colors.
pub fn apply(mode: ThemeMode, cx: &mut App) {
    if cx.theme().mode != mode || cx.theme().primary != brand(mode.is_dark()) {
        Theme::change(mode, None, cx);
        Theme::update(cx, |theme| paint(theme, mode.is_dark()));
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn brand(dark: bool) -> Hsla {
    if dark { hex(0x3B82F6) } else { hex(0x2563EB) }
}

fn paint(theme: &mut Theme, dark: bool) {
    theme.radius = px(8.);
    theme.radius_lg = px(14.);
    let primary = brand(dark);
    let colors = &mut theme.colors;
    colors.primary = primary;
    colors.primary_hover = if dark { hex(0x60A5FA) } else { hex(0x1D4ED8) };
    colors.primary_active = if dark { hex(0x2563EB) } else { hex(0x1E40AF) };
    colors.primary_foreground = hex(0xFFFFFF);
    colors.button_primary = colors.primary;
    colors.button_primary_hover = colors.primary_hover;
    colors.button_primary_active = colors.primary_active;
    colors.button_primary_foreground = colors.primary_foreground;
    colors.ring = primary.opacity(0.55);
    colors.caret = primary;
    colors.selection = primary.opacity(0.25);
    colors.link = primary;
    colors.link_hover = colors.primary_hover;
    colors.link_active = colors.primary_active;
    if dark {
        colors.background = hex(0x161A22);
        colors.foreground = hex(0xE8EAF0);
        colors.border = hex(0x262B36);
        colors.input = hex(0x2E3440);
        colors.muted = hex(0x1D222C);
        colors.muted_foreground = hex(0x8C93A4);
        colors.secondary = hex(0x222834);
        colors.secondary_hover = hex(0x2A3140);
        colors.secondary_active = hex(0x323A4B);
        colors.switch = hex(0x343B4A);
        colors.switch_thumb = hex(0xFFFFFF);
        colors.popover = hex(0x1B2029);
        colors.success = hex(0x34D399);
        colors.warning = hex(0xFBBF24);
        colors.danger = hex(0xF87171);
    } else {
        colors.background = hex(0xFFFFFF);
        colors.foreground = hex(0x0F172A);
        colors.border = hex(0xE4E7EE);
        colors.input = hex(0xD9DEE7);
        colors.muted = hex(0xF3F5F9);
        colors.muted_foreground = hex(0x667085);
        colors.secondary = hex(0xF1F4F9);
        colors.secondary_hover = hex(0xE7EBF2);
        colors.secondary_active = hex(0xDDE2EB);
        colors.switch = hex(0xD5DAE3);
        colors.popover = hex(0xFFFFFF);
        colors.success = hex(0x16A34A);
        colors.warning = hex(0xD97706);
        colors.danger = hex(0xDC2626);
    }
}

/// The window backdrop the cards sit on.
pub(crate) fn canvas(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        hex(0x0E1117)
    } else {
        hex(0xF4F6FA)
    }
}

/// A rounded square holding an icon, tinted with `color`.
pub(crate) fn icon_badge(icon: impl Into<Icon>, color: Hsla, size: Pixels) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size)
        .rounded(size * 0.3)
        .bg(color.opacity(0.12))
        .text_color(color)
        .child(Icon::new(icon).with_size(size * 0.5))
}

/// A small filled circle signalling state.
pub(crate) fn status_dot(color: Hsla) -> Div {
    div()
        .flex_none()
        .size(px(8.))
        .rounded_full()
        .bg(color)
        .border_2()
        .border_color(color.opacity(0.25))
}

/// A card on the canvas.
pub(crate) fn card(cx: &App) -> Div {
    div()
        .v_flex()
        .gap_5()
        .p_6()
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius_lg)
        .shadow_sm()
}

/// A card's heading: icon, title, and a one-line description.
pub(crate) fn card_header(
    icon: impl Into<Icon>,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    cx: &App,
) -> Div {
    div()
        .h_flex()
        .gap_3()
        .min_w_0()
        .child(icon_badge(icon, cx.theme().primary, px(40.)))
        .child(
            div()
                .v_flex()
                .min_w_0()
                .child(div().text_base().font_semibold().child(title.into()))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(description.into()),
                ),
        )
}

/// A small upper-level label above a value or a group.
pub(crate) fn eyebrow(content: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(content.into())
}

/// A bordered group of rows separated by hairlines, like a settings list.
pub(crate) fn row_group<E: IntoElement + Styled>(
    rows: impl IntoIterator<Item = E>,
    cx: &App,
) -> Div {
    let border = cx.theme().border;
    let mut group = div()
        .v_flex()
        .border_1()
        .border_color(border)
        .rounded(cx.theme().radius)
        .overflow_hidden();
    for (index, row) in rows.into_iter().enumerate() {
        let row = row.px_3().py_2p5();
        group = group.child(if index > 0 {
            row.border_t_1().border_color(border)
        } else {
            row
        });
    }
    group
}

/// One row in a [`row_group`]: an icon, a label, and a control on the right.
pub(crate) fn setting_row(
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    div()
        .h_flex()
        .gap_3()
        .child(
            div()
                .flex_none()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(icon).small()),
        )
        .child(div().flex_1().min_w_0().text_sm().child(label.into()))
        .child(control)
}

/// A tinted message box, for warnings and errors.
pub(crate) fn callout(icon: impl Into<Icon>, color: Hsla, cx: &App) -> Div {
    div()
        .h_flex()
        .items_start()
        .gap_3()
        .p_3()
        .rounded(cx.theme().radius)
        .bg(color.opacity(0.08))
        .border_1()
        .border_color(color.opacity(0.3))
        .child(
            div()
                .flex_none()
                .pt(px(2.))
                .text_color(color)
                .child(Icon::new(icon).small()),
        )
}

/// A segmented control: a tray holding [`segment`]s, one of them selected.
pub(crate) fn segmented(segments: impl IntoIterator<Item = Stateful<Div>>, cx: &App) -> Div {
    div()
        .h_flex()
        .flex_none()
        .gap_0p5()
        .p_0p5()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .border_1()
        .border_color(cx.theme().border)
        .children(segments)
}

/// One choice in a [`segmented`] control; the caller adds the click handler.
pub(crate) fn segment(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let foreground = cx.theme().foreground;
    let segment = div()
        .id(ElementId::Name(id.into()))
        .h_flex()
        .h(px(24.))
        .px_2p5()
        .rounded(cx.theme().radius - px(2.))
        .text_xs()
        .font_medium()
        .cursor_pointer()
        .child(label.into());
    if selected {
        segment
            .bg(cx.theme().background)
            .text_color(foreground)
            .shadow_xs()
    } else {
        segment
            .text_color(cx.theme().muted_foreground)
            .hover(move |segment| segment.text_color(foreground))
    }
}

/// The app mark: the icon's blue tile with a monitor.
pub(crate) fn logo() -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(38.))
        .rounded(px(11.))
        .bg(linear_gradient(
            135.,
            linear_color_stop(rgb(0x3B82F6), 0.),
            linear_color_stop(rgb(0x1D4ED8), 1.),
        ))
        .shadow_sm()
        .text_color(gpui_kit::white())
        .child(Icon::new(gpui_kit::assets::IconName::Monitor).with_size(px(20.)))
}
