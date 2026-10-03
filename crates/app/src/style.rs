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
use gpui_kit::component::{
    ActiveTheme, Colorize as _, Icon, Sizable as _, StyledExt as _, TitleBar,
};
use gpui_kit::prelude::FluentBuilder as _;

use crate::backdrop;
use crate::settings::ThemePreference;
use crate::state::AppState;
use gpui_kit::*;

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        ArrowLeftRight,
        Clipboard,
        Clock,
        Command,
        Droplet,
        FileUp,
        Gauge,
        Image,
        KeyRound,
        Layers,
        Laptop,
        Monitor,
        MonitorSmartphone,
        MousePointer2,
        Radar,
        ShieldCheck,
        Unplug,
        Volume2,
        VolumeX,
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

/// The theme the settings ask for, given the system's current appearance.
fn preferred_mode(system: WindowAppearance, cx: &App) -> ThemeMode {
    let preference = if cx.has_global::<AppState>() {
        AppState::settings(cx).theme
    } else {
        ThemePreference::System
    };
    match preference {
        ThemePreference::System => system.into(),
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::Dark => ThemeMode::Dark,
    }
}

/// Applies the theme the settings ask for. Call after the settings change.
pub(crate) fn sync_theme(cx: &mut App) {
    match_native_appearance(AppState::settings(cx).theme);
    let mode = preferred_mode(cx.window_appearance(), cx);
    apply(mode, cx);
}

/// Has macOS draw the windows' own parts, the blur behind them above all, in the theme the
/// settings ask for. Otherwise a light theme on a dark system sits on a dark blur and turns gray.
#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "reads two of AppKit's constant appearance names"
)]
fn match_native_appearance(preference: ThemePreference) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    };

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: AppKit's appearance names are immutable strings it defines for the whole process.
    let name = match preference {
        ThemePreference::System => None,
        ThemePreference::Light => Some(unsafe { NSAppearanceNameAqua }),
        ThemePreference::Dark => Some(unsafe { NSAppearanceNameDarkAqua }),
    };
    let appearance = name.and_then(NSAppearance::appearanceNamed);
    NSApplication::sharedApplication(main_thread).setAppearance(appearance.as_deref());
}

#[cfg(not(target_os = "macos"))]
fn match_native_appearance(_preference: ThemePreference) {}

/// Follows the system's light or dark appearance while `window` is open, when the settings
/// leave the theme to the system.
pub(crate) fn follow_appearance<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
) -> Subscription {
    cx.observe_window_appearance(window, |_, window, cx| {
        let mode = preferred_mode(window.appearance(), cx);
        apply(mode, cx);
    })
}

/// Whether the settings let what is behind the windows show through them.
pub(crate) fn translucent(cx: &App) -> bool {
    !cx.has_global::<AppState>() || AppState::settings(cx).translucent_window
}

/// How a window's background is drawn under the current settings.
pub(crate) fn window_background(cx: &App) -> WindowBackgroundAppearance {
    if translucent(cx) {
        WindowBackgroundAppearance::Blurred
    } else {
        WindowBackgroundAppearance::Opaque
    }
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
        window_background: window_background(cx),
        ..TitleBar::window_options()
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn brand(dark: bool) -> Hsla {
    if dark { hex(0x3B82F6) } else { hex(0x2563EB) }
}

/// The opaque color Dari's surfaces are made of. The theme's own background is transparent, so
/// gpui-kit's root leaves the window clear and what is behind it can show through.
fn base(dark: bool) -> Hsla {
    if dark { hex(0x1A1B1F) } else { hex(0xFFFFFF) }
}

/// An opaque surface, for cards and controls that float over translucent ones.
pub(crate) fn surface(cx: &App) -> Hsla {
    base(cx.theme().is_dark())
}

fn paint(theme: &mut Theme, dark: bool) {
    // gpui-kit's root sets each window's rem from this on every frame.
    theme.font_size = REM;
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
    // The default button is a frosted chip: a wash of the text color, so it reads on any
    // surface, a background picture included.
    let ink = if dark { hex(0xFFFFFF) } else { hex(0x000000) };
    colors.button = ink.opacity(if dark { 0.1 } else { 0.06 });
    colors.button_hover = ink.opacity(if dark { 0.15 } else { 0.1 });
    colors.button_active = ink.opacity(if dark { 0.2 } else { 0.14 });
    if dark {
        colors.background = transparent_black();
        colors.foreground = hex(0xE6E7EA);
        colors.border = hex(0x2B2D32);
        colors.input = hex(0x34363C);
        colors.muted = hex(0x232428);
        colors.muted_foreground = hex(0x8B8E96);
        colors.secondary = hex(0x26272C);
        colors.secondary_hover = hex(0x2D2F34);
        colors.secondary_active = hex(0x34363C);
        colors.secondary_foreground = colors.foreground;
        colors.button_foreground = colors.foreground;
        colors.switch = hex(0x3A3C42);
        colors.switch_thumb = hex(0xFFFFFF);
        colors.popover = hex(0x202125);
        colors.success = hex(0x4ADE80);
        colors.warning = hex(0xFBBF24);
        colors.danger = hex(0xF87171);
    } else {
        colors.background = transparent_black();
        colors.switch_thumb = hex(0xFFFFFF);
        colors.foreground = hex(0x18191C);
        colors.border = hex(0xE7E8EB);
        colors.input = hex(0xDCDEE2);
        colors.muted = hex(0xF4F4F6);
        colors.muted_foreground = hex(0x63666E);
        colors.secondary = hex(0xF2F3F5);
        colors.secondary_hover = hex(0xE9EAED);
        colors.secondary_active = hex(0xE1E2E6);
        colors.secondary_foreground = colors.foreground;
        colors.button_foreground = colors.foreground;
        colors.switch = hex(0xD8DADF);
        colors.popover = hex(0xFFFFFF);
        colors.success = hex(0x16A34A);
        colors.warning = hex(0xD97706);
        colors.danger = hex(0xDC2626);
    }
}

/// The sidebar's surface: the most translucent, so what is behind the window shows through.
pub(crate) fn sidebar_surface(cx: &App) -> Hsla {
    let dark = cx.theme().is_dark();
    let tint = if dark { hex(0x141518) } else { hex(0xEEEFF2) };
    // Gray text needs more cover on a light tint than on a dark one to stay readable: a dark
    // window behind turns a thin light tint gray.
    let cover = match (translucent(cx), dark) {
        (true, true) => 0.55,
        (true, false) => 0.9,
        (false, _) => 1.,
    };
    tint.opacity(cover)
}

/// The main content's surface: translucent enough to show what is behind it, opaque enough
/// that text stays crisp.
pub(crate) fn content_surface(cx: &App) -> Hsla {
    surface(cx).opacity(if translucent(cx) { 0.86 } else { 1. })
}

/// How opaque the background picture is. With translucency on, what is behind the window shows
/// through it a little; any more, and the blurred desktop washes the picture out.
pub(crate) fn picture_opacity(cx: &App) -> f32 {
    if translucent(cx) { 0.82 } else { 1. }
}

/// How opaque the blurred picture in the sidebar is, over the sidebar's own surface. The
/// further the picture is from the surface in lightness, the less of it shows, so the sidebar's
/// gray text stays readable.
pub(crate) fn frost_opacity(average: Hsla, cx: &App) -> f32 {
    let opacity = (0.4 - 0.5 * contrast(average, cx)).max(0.12);
    if translucent(cx) {
        opacity * 0.8
    } else {
        opacity
    }
}

/// The pages' surface under a background picture. Translucent, it clears where the picture is
/// whole, so the picture is all that stands between the desktop and the window's top half.
pub(crate) fn scenery_surface(cx: &App) -> Background {
    let surface = content_surface(cx);
    if translucent(cx) {
        linear_gradient(
            180.,
            linear_color_stop(surface.opacity(0.), backdrop::FADE_FROM),
            linear_color_stop(surface, backdrop::FADE_TO),
        )
    } else {
        surface.into()
    }
}

/// The veil over a background picture: a light wash of the surface's color, shifted toward the
/// picture's `average` color, behind the page title at the top, gone by the first panels. The
/// panels carry the rest of the page on their own glass, so the picture stays vivid around them.
/// A picture far from the surface in lightness, like a dark photo behind the light theme, gets
/// the most.
pub(crate) fn veil(average: Hsla, cx: &App) -> Background {
    let dark = cx.theme().is_dark();
    let color = surface(cx).mix_oklab(average, if dark { 0.78 } else { 0.84 });
    let cover = (0.1 + 0.4 * contrast(average, cx)).min(0.32);
    linear_gradient(
        180.,
        linear_color_stop(color.opacity(cover), 0.08),
        linear_color_stop(color.opacity(0.), 0.32),
    )
}

/// Frosted glass for a panel with corners of `radius`: over a background picture, the picture
/// blurred and cut to the panel where it lies in the window, under a tint of the surface, so
/// text on the panel reads the same over any picture. Without a picture, a faint wash.
///
/// Add it to a `relative` panel before the panel's content, which then draws over it.
pub(crate) fn glass(radius: Pixels, cx: &App) -> Div {
    let layer = div().absolute().inset_0().rounded(radius);
    let Some(scenery) = backdrop::shown(cx) else {
        return layer.bg(cx.theme().foreground.opacity(0.025));
    };
    let frost = scenery.frost.clone();
    let blurred = canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            // Placed as the sidebar and the pages place the picture: covering the window.
            let window_bounds = Bounds::new(Point::default(), window.viewport_size());
            let picture = ObjectFit::Cover.get_bounds(window_bounds, frost.size(0));
            let _painted = window.paint_image(
                bounds,
                picture,
                Corners::all(radius),
                frost.clone(),
                0,
                false,
            );
        },
    )
    .size_full();
    let dark = cx.theme().is_dark();
    let tint = surface(cx).mix_oklab(scenery.average, if dark { 0.86 } else { 0.9 });
    let cover = (0.56 + 0.3 * contrast(scenery.average, cx)).min(0.8);
    layer
        .child(
            div()
                .absolute()
                .inset_0()
                .opacity(picture_opacity(cx))
                .child(blurred),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .rounded(radius)
                .bg(tint.opacity(cover))
                .border_1()
                .border_color(
                    cx.theme()
                        .foreground
                        .opacity(if dark { 0.08 } else { 0.06 }),
                ),
        )
}

/// A panel on [`glass`], for a group of content.
pub(crate) fn panel(cx: &App) -> Div {
    let radius = cx.theme().radius_lg;
    div()
        .relative()
        .v_flex()
        .rounded(radius)
        .child(glass(radius, cx))
}

/// A titled group of rows on a [`panel`].
pub(crate) fn section(title: impl Into<SharedString>, rows: impl IntoElement, cx: &App) -> Div {
    panel(cx)
        .pt_3()
        .px_4()
        .pb_1()
        .child(eyebrow(title, cx))
        .child(rows)
}

/// How far apart a picture's `average` color and the surface are in lightness, from 0 to 1.
fn contrast(average: Hsla, cx: &App) -> f32 {
    (lightness(surface(cx)) - lightness(average)).abs()
}

/// Perceived lightness, from 0 for black to 1 for white.
fn lightness(color: Hsla) -> f32 {
    let color = color.to_rgb();
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    (0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)).cbrt()
}

/// A hairline between panels, visible on any background.
pub(crate) fn hairline(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.09)
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
                        // Over a picture, gray text needs more weight to stand out.
                        .text_color(if backdrop::shown(cx).is_some() {
                            cx.theme().foreground.opacity(0.78)
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(description.into()),
                ),
        )
        .child(div().flex_none().child(trailing))
}

/// A text field on a frosted fill, so it reads as a field over any surface.
pub(crate) fn field(input: impl IntoElement, cx: &App) -> Div {
    div()
        .rounded(cx.theme().radius)
        .bg(cx.theme().button)
        .child(input)
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
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .truncate()
                .child(label.into()),
        )
        .child(control)
}

/// A tinted message box on [`glass`], for warnings and errors.
pub(crate) fn callout(
    icon: impl Into<Icon>,
    color: Hsla,
    content: impl IntoElement,
    cx: &App,
) -> Div {
    let radius = cx.theme().radius_lg;
    div()
        .relative()
        .h_flex()
        .items_start()
        .gap_2p5()
        .px_3()
        .py_2p5()
        .rounded(radius)
        .child(glass(radius, cx))
        .child(tint(color, radius))
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

/// A wash of `color` over a [`glass`] panel with corners of `radius`, outlined in it.
pub(crate) fn tint(color: Hsla, radius: Pixels) -> Div {
    div()
        .absolute()
        .inset_0()
        .rounded(radius)
        .bg(color.opacity(0.1))
        .border_1()
        .border_color(color.opacity(0.35))
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
pub(crate) fn segmented(segments: impl IntoIterator<Item = impl IntoElement>, cx: &App) -> Div {
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
        segment.bg(surface(cx)).text_color(foreground).shadow_xs()
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
