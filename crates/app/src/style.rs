//! Dari's look: a quiet, mostly monochrome palette over gpui-kit's theme, translucent window
//! surfaces, the extra icons the app embeds, and the building blocks every window shares.
//!
//! Color is kept for meaning: the primary action and switches use Dari's blue, status dots
//! use green, amber, and red, and everything else is a shade of gray.

#![allow(
    clippy::unreadable_literal,
    reason = "colors are written like CSS #RRGGBB"
)]

use std::borrow::Cow;

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _, StyledExt as _, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
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

/// Where [`AppAssets`] serves the app icon, for [`logo`].
const APP_ICON: &str = "brand/icon.png";

/// The component icons, the few extra Lucide icons Dari uses, and the app icon.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == APP_ICON {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/icon-128.png"
            ))));
        }
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

/// The root font size. Spacing and control sizes are in rems, so this sets the app's density.
pub(crate) const REM: Pixels = px(14.);

/// Height of the strip at the top of every window that holds the window controls.
pub(crate) const TITLE_BAR_HEIGHT: Pixels = px(44.);

/// Width of the home window's sidebar.
pub(crate) const SIDEBAR_WIDTH: Pixels = px(232.);

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

/// Options for a Dari window: content runs up under a transparent title bar, and whatever is
/// behind the window shows through blurred where the surfaces are translucent.
pub(crate) fn window_options(
    title: impl Into<SharedString>,
    size: Size<Pixels>,
    min_size: Option<Size<Pixels>>,
    cx: &App,
) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::centered(size, cx)),
        window_min_size: min_size,
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            appears_transparent: true,
            // Centers the macOS traffic lights in the title bar strip.
            traffic_light_position: Some(point(px(16.), px(15.))),
        }),
        window_background: WindowBackgroundAppearance::Blurred,
        ..TitleBar::window_options()
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn brand(dark: bool) -> Hsla {
    if dark { hex(0x3B82F6) } else { hex(0x2563EB) }
}

fn paint(theme: &mut Theme, dark: bool) {
    theme.radius = px(7.);
    theme.radius_lg = px(12.);
    theme.shadow = false;
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
    colors.ring = primary.opacity(0.5);
    colors.caret = primary;
    colors.selection = primary.opacity(0.3);
    colors.link = primary;
    colors.link_hover = colors.primary_hover;
    colors.link_active = colors.primary_active;
    if dark {
        colors.background = hex(0x1A1B1F);
        colors.foreground = hex(0xE6E7EA);
        colors.border = hex(0x2B2D32);
        colors.input = hex(0x34363C);
        colors.muted = hex(0x232428);
        colors.muted_foreground = hex(0x8B8E96);
        colors.secondary = hex(0x26272C);
        colors.secondary_hover = hex(0x2D2F34);
        colors.secondary_active = hex(0x34363C);
        colors.secondary_foreground = colors.foreground;
        colors.switch = hex(0x3A3C42);
        colors.switch_thumb = hex(0xFFFFFF);
        colors.popover = hex(0x202125);
        colors.success = hex(0x4ADE80);
        colors.warning = hex(0xFBBF24);
        colors.danger = hex(0xF87171);
    } else {
        colors.background = hex(0xFFFFFF);
        colors.foreground = hex(0x18191C);
        colors.border = hex(0xE7E8EB);
        colors.input = hex(0xDCDEE2);
        colors.muted = hex(0xF4F4F6);
        colors.muted_foreground = hex(0x6E717A);
        colors.secondary = hex(0xF2F3F5);
        colors.secondary_hover = hex(0xE9EAED);
        colors.secondary_active = hex(0xE1E2E6);
        colors.secondary_foreground = colors.foreground;
        colors.switch = hex(0xD8DADF);
        colors.popover = hex(0xFFFFFF);
        colors.success = hex(0x16A34A);
        colors.warning = hex(0xD97706);
        colors.danger = hex(0xDC2626);
    }
}

/// The sidebar's surface: the most translucent, so the desktop shows through.
pub(crate) fn sidebar_surface(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        hex(0x141518).opacity(0.72)
    } else {
        hex(0xEEEFF2).opacity(0.78)
    }
}

/// The main content's surface: nearly opaque, so text stays crisp.
pub(crate) fn content_surface(cx: &App) -> Hsla {
    cx.theme().background.opacity(0.94)
}

/// A wash for hovered rows; [`selected_fill`] is one step stronger.
pub(crate) fn hover_fill(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.05)
}

/// The fill of the selected row in a list.
pub(crate) fn selected_fill(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.09)
}

/// A transparent title bar strip: drags the window, double-click zooms it, and on Windows it
/// draws the window buttons at its right end.
pub(crate) fn title_bar() -> TitleBar {
    TitleBar::new()
        .h(TITLE_BAR_HEIGHT)
        .pl_0()
        .bg(transparent_black())
        .border_color(transparent_black())
}

/// Keeps a press inside a title bar from turning into a window drag.
pub(crate) fn no_drag<E: InteractiveElement>(element: E) -> E {
    element.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
        .bg(color.opacity(0.14))
        .text_color(color)
        .child(Icon::new(icon).with_size(size * 0.5))
}

/// A small filled circle signalling state.
pub(crate) fn status_dot(color: Hsla) -> Div {
    div().flex_none().size(px(7.)).rounded_full().bg(color)
}

/// A page's heading: a title, a line of context, and optional controls on the right.
pub(crate) fn page_header(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    trailing: impl IntoElement,
    cx: &App,
) -> Div {
    div()
        .h_flex()
        .items_start()
        .justify_between()
        .gap_4()
        .child(
            div()
                .v_flex()
                .flex_1()
                .gap_0p5()
                .min_w_0()
                .child(div().text_xl().font_semibold().child(title.into()))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(description.into()),
                ),
        )
        .child(div().flex_none().child(trailing))
}

/// A small label above a value or a group.
pub(crate) fn eyebrow(content: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(content.into())
}

/// Rows separated by hairlines, with no box around them.
pub(crate) fn row_list<E: IntoElement + Styled>(
    rows: impl IntoIterator<Item = E>,
    cx: &App,
) -> Div {
    let border = cx.theme().border;
    let mut list = div().v_flex();
    for (index, row) in rows.into_iter().enumerate() {
        let row = row.py_2p5();
        list = list.child(if index > 0 {
            row.border_t_1().border_color(border)
        } else {
            row
        });
    }
    list
}

/// One row in a [`row_list`]: an icon, a label, and a control on the right.
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
pub(crate) fn callout(
    icon: impl Into<Icon>,
    color: Hsla,
    content: impl IntoElement,
    cx: &App,
) -> Div {
    div()
        .h_flex()
        .items_start()
        .gap_2p5()
        .px_3()
        .py_2p5()
        .rounded(cx.theme().radius)
        .bg(color.opacity(0.1))
        .child(
            div()
                .flex_none()
                .pt(px(2.))
                .text_color(color)
                .child(Icon::new(icon).small()),
        )
        // A flex item is never narrower than its content unless told so; without
        // `min_w_0`, long messages run past the box instead of wrapping.
        .child(div().flex_1().min_w_0().child(content))
}

/// A heading over a group of sidebar rows.
pub(crate) fn sidebar_heading(content: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px_2()
        .pt_4()
        .pb_1()
        .text_xs()
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(content.into())
}

/// A sidebar row: an icon, a label with an optional second line, and an optional trailing
/// element. The caller adds the click handler.
pub(crate) fn sidebar_row(
    id: impl Into<SharedString>,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    detail: Option<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let hover = hover_fill(cx);
    let mut about = div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .child(div().text_sm().truncate().child(label.into()));
    if let Some(detail) = detail {
        about = about.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(detail),
        );
    }
    div()
        .id(ElementId::Name(id.into()))
        .h_flex()
        .gap_2p5()
        .px_2()
        .py_1p5()
        .rounded(cx.theme().radius)
        .cursor_pointer()
        .map(|row| {
            if selected {
                row.bg(selected_fill(cx))
            } else {
                row.hover(move |row| row.bg(hover))
            }
        })
        .child(
            div()
                .flex_none()
                .text_color(if selected {
                    cx.theme().foreground
                } else {
                    cx.theme().muted_foreground
                })
                .child(Icon::new(icon).small()),
        )
        .child(about)
}

/// A segmented control: a tray holding [`segment`]s, one of them selected.
pub(crate) fn segmented(segments: impl IntoIterator<Item = Stateful<Div>>, cx: &App) -> Div {
    div()
        .h_flex()
        .flex_none()
        .gap_0p5()
        .p_0p5()
        .rounded(cx.theme().radius)
        .bg(cx.theme().foreground.opacity(0.06))
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
    let segment = no_drag(
        div()
            .id(ElementId::Name(id.into()))
            .h_flex()
            .h(px(24.))
            .px_2p5()
            .rounded(cx.theme().radius - px(2.))
            .text_xs()
            .font_medium()
            .cursor_pointer()
            .child(label.into()),
    );
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

/// The app icon. Its tile fills 7/8 of the image.
pub(crate) fn logo(size: Pixels) -> Img {
    img(APP_ICON).flex_none().size(size)
}
