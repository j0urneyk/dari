//! Home window: share this device, or connect to another one.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use dari_media::StreamSettings;
use dari_net::{
    AccessPassword, Advertisement, Browser, ConnectError, DiscoveryEvent, HandshakeError,
    NearbyDevice, PeerInfo, fingerprint_hint,
};
use dari_proto::{Availability, HostStatus};
use dari_session::{
    ApprovalDecision, ApprovalRequest, HostConfig, HostEvent, HostHandle, RelayStatus,
    SystemClipboard, SystemPlatform, ViewerConfig, connect_viewer, start_host,
};
use gpui_kit::TestSupportExt as _;
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::backdrop;
use crate::config::{device_name, local_addresses, resolve_target};
use crate::permissions::{self, LocalPermissions};
use crate::runtime::TokioRuntime;
use crate::settings::ThemePreference;
use crate::state::AppState;
use crate::style;
use crate::text::text;
use crate::viewer::open_viewer_window;

/// How often permissions and network addresses are re-read while the window is open.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// The page shown next to the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    /// Sharing this device: password, addresses, the session, settings.
    Device,
    /// The form for connecting to another device.
    Connect,
    /// How Dari looks: theme, translucency, the background picture.
    Settings,
}

pub struct Home {
    host: Entity<HostPanel>,
    connect: Entity<ConnectPanel>,
    page: Page,
    /// Whether the host panel was waiting on the user's answer when it last changed.
    approval_pending: bool,
    /// The background picture from the settings, softened, once it has loaded.
    backdrop: Option<Scenery>,
    /// Whether the background picture in the settings could not be read.
    backdrop_unreadable: bool,
    /// Loads the background picture; replacing it cancels the load.
    backdrop_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for Home {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Home")
            .field("page", &self.page)
            .finish_non_exhaustive()
    }
}

impl Home {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let host = cx.new(|cx| HostPanel::new(window, cx));
        let connect = cx.new(|cx| ConnectPanel::new(window, cx));
        let subscriptions = vec![
            style::follow_appearance(window, cx),
            cx.observe(&host, |this, host, cx| {
                // A connection request times out in 30 seconds, so it takes over the window.
                let pending = host.read(cx).approval.is_some();
                if pending && !this.approval_pending {
                    this.page = Page::Device;
                }
                this.approval_pending = pending;
                cx.notify();
            }),
            cx.observe(&connect, |_, _, cx| cx.notify()),
        ];
        let mut home = Self {
            host,
            connect,
            page: Page::Device,
            approval_pending: false,
            backdrop: None,
            backdrop_unreadable: false,
            backdrop_task: None,
            _subscriptions: subscriptions,
        };
        home.load_backdrop(window, cx);
        home
    }

    /// Loads the background picture named in the settings, replacing the current one.
    fn load_backdrop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.backdrop_unreadable = false;
        let Some(path) = AppState::settings(cx).background_image.clone() else {
            self.backdrop_task = None;
            self.set_backdrop(None, window, cx);
            return;
        };
        let blur = AppState::settings(cx).blur_background;
        let prepared = cx.background_spawn(async move { backdrop::prepare(&path, blur) });
        self.backdrop_task = Some(cx.spawn_in(window, async move |this, cx| {
            let picture = prepared.await;
            let _updated = this.update_in(cx, |this, window, cx| match picture {
                Ok(prepared) => this.set_backdrop(Some(Scenery::new(prepared)), window, cx),
                Err(error) => {
                    tracing::warn!("cannot show the background picture: {error:#}");
                    this.backdrop_unreadable = true;
                    this.set_backdrop(None, window, cx);
                }
            });
        }));
    }

    fn set_backdrop(
        &mut self,
        scenery: Option<Scenery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = std::mem::replace(&mut self.backdrop, scenery) {
            // Each layer is a GPU texture; release the ones being replaced.
            for layer in [previous.picture, previous.frost, previous.glow] {
                let _dropped = window.drop_image(layer);
            }
        }
        cx.notify();
    }

    fn choose_backdrop(window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _updated = this.update_in(cx, |this, window, cx| {
                AppState::update_settings(cx, |settings| settings.background_image = Some(path));
                this.load_backdrop(window, cx);
            });
        })
        .detach();
    }

    fn clear_backdrop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        AppState::update_settings(cx, |settings| settings.background_image = None);
        self.load_backdrop(window, cx);
    }
}

/// Hooks for the headless GUI tests.
#[doc(hidden)]
impl Home {
    pub fn has_password(&self, cx: &App) -> bool {
        self.host.read(cx).password.is_some()
    }

    pub fn password_text(&self, cx: &App) -> Option<String> {
        let password = self.host.read(cx).password.as_ref()?;
        Some(password.display_text().as_str().to_owned())
    }

    pub fn host_port(&self, cx: &App) -> Option<u16> {
        match &self.host.read(cx).hosting {
            Hosting::Running(handle) => Some(handle.local_address().port()),
            Hosting::Off | Hosting::Failed(_) => None,
        }
    }

    pub fn relay_id(&self, cx: &App) -> Option<String> {
        match &self.host.read(cx).relay {
            Some(RelayStatus::Registered(id)) => Some(id.to_string()),
            _ => None,
        }
    }

    pub fn has_pending_approval(&self, cx: &App) -> bool {
        self.host.read(cx).approval.is_some()
    }

    pub fn admitted_session_status(&self, cx: &App) -> Option<HostStatus> {
        self.host.read(cx).session_status
    }

    pub fn has_backdrop(&self) -> bool {
        self.backdrop.is_some()
    }
}

impl Home {
    /// This device's state in a few words, with the color of its status dot.
    fn device_status(&self, cx: &App) -> (Hsla, String) {
        let host = self.host.read(cx);
        match &host.hosting {
            Hosting::Running(_) if host.approval.is_some() => {
                (cx.theme().warning, text().approval_title.to_owned())
            }
            Hosting::Running(_) if host.viewer.is_some() => (
                cx.theme().success,
                text().viewer_connected(&host.viewer_name()),
            ),
            Hosting::Running(_) => (cx.theme().success, text().hosting_on.to_owned()),
            Hosting::Off => (cx.theme().muted_foreground, text().hosting_off.to_owned()),
            Hosting::Failed(_) => (cx.theme().danger, text().hosting_failed.to_owned()),
        }
    }

    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    /// Opens the connect page with `address` filled in, ready for the password.
    fn connect_to(&mut self, address: String, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Connect;
        self.connect
            .update(cx, |connect, cx| connect.pick(address, window, cx));
        cx.notify();
    }

    fn render_sidebar(&self, viewport: Size<Pixels>, cx: &mut Context<Self>) -> impl IntoElement {
        let (status_color, status) = self.device_status(cx);
        let navigation = div()
            .v_flex()
            .gap_0p5()
            .child(
                style::sidebar_row(
                    "nav-device",
                    AssetIcon::Monitor,
                    text().this_device,
                    Some(status.into()),
                    self.page == Page::Device,
                    cx,
                )
                .child(style::status_dot(status_color))
                .on_click(cx.listener(|this, _, _, cx| this.show(Page::Device, cx)))
                .test_support(),
            )
            .child(
                style::sidebar_row(
                    "nav-connect",
                    AssetIcon::MousePointer2,
                    text().control_remote_device,
                    None,
                    self.page == Page::Connect,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.show(Page::Connect, cx)))
                .test_support(),
            )
            .child(self.render_devices(cx));

        let frosted = self.backdrop.as_ref().map(|scenery| {
            // The picture, blurred, drawn where it lies in the window: the sidebar reads as
            // frosted glass over it.
            div()
                .absolute()
                .inset_0()
                .opacity(style::picture_opacity(cx))
                .child(
                    img(scenery.frost.clone())
                        .absolute()
                        .top_0()
                        .left_0()
                        .w(viewport.width)
                        .h(viewport.height)
                        .object_fit(ObjectFit::Cover),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(style::frost_tint(scenery.average, cx)),
                )
        });
        div()
            .relative()
            .overflow_hidden()
            .v_flex()
            .flex_none()
            .w(style::SIDEBAR_WIDTH)
            .h_full()
            .pt(style::TITLE_BAR_HEIGHT)
            .when(frosted.is_none(), |sidebar| {
                sidebar.bg(style::sidebar_surface(cx))
            })
            .children(frosted)
            .border_r_1()
            .border_color(style::hairline(cx))
            .child(
                div()
                    .id("sidebar")
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .overflow_y_scrollbar()
                    .child(navigation),
            )
            .child(
                div().px_2().pt_2().pb_3().child(
                    style::sidebar_row(
                        "nav-settings",
                        IconName::Settings,
                        text().settings_title,
                        None,
                        self.page == Page::Settings,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.show(Page::Settings, cx)))
                    .test_support(),
                ),
            )
    }

    /// Nearby and recent devices; picking one opens the connect page for it.
    fn render_devices(&self, cx: &mut Context<Self>) -> Div {
        let nearby: Vec<(String, String, String)> = self
            .connect
            .read(cx)
            .nearby
            .iter()
            .map(|device| {
                (
                    format!("nearby-{}", device.id),
                    device.name.clone(),
                    nearby_address(device),
                )
            })
            .collect();
        let recent = AppState::settings(cx).recent_addresses.clone();

        let mut devices = div()
            .v_flex()
            .gap_0p5()
            .child(style::sidebar_heading(text().nearby_devices, cx));
        if nearby.is_empty() {
            devices = devices.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(text().nearby_empty),
            );
        }
        for (id, name, address) in nearby {
            let picked = address.clone();
            devices = devices.child(
                style::sidebar_row(id, AssetIcon::Laptop, name, Some(address.into()), false, cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.connect_to(picked.clone(), window, cx);
                    })),
            );
        }
        if !recent.is_empty() {
            devices = devices.child(style::sidebar_heading(text().recent, cx));
        }
        for address in recent {
            let picked = address.clone();
            devices = devices.child(
                style::sidebar_row(
                    format!("recent-{address}"),
                    AssetIcon::Clock,
                    address,
                    None,
                    false,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.connect_to(picked.clone(), window, cx);
                })),
            );
        }
        devices
    }

    fn set_theme(theme: ThemePreference, cx: &mut Context<Self>) {
        AppState::update_settings(cx, |settings| settings.theme = theme);
        style::sync_theme(cx);
        cx.notify();
    }

    fn set_translucent(translucent: bool, window: &mut Window, cx: &mut Context<Self>) {
        AppState::update_settings(cx, |settings| settings.translucent_window = translucent);
        window.set_background_appearance(style::window_background(cx));
        cx.notify();
    }

    fn set_blur(&mut self, blur: bool, window: &mut Window, cx: &mut Context<Self>) {
        AppState::update_settings(cx, |settings| settings.blur_background = blur);
        self.load_backdrop(window, cx);
    }

    /// The settings page: how Dari looks.
    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_7()
            .child(style::page_header(
                text().settings_title,
                text().settings_subtitle,
                div(),
                cx,
            ))
            .child(Self::render_appearance(cx))
            .child(self.render_picture_settings(cx))
    }

    /// Theme and window translucency.
    fn render_appearance(cx: &mut Context<Self>) -> Div {
        let settings = AppState::settings(cx).clone();
        let themes = [
            (ThemePreference::System, "theme-system", text().theme_system),
            (ThemePreference::Light, "theme-light", text().theme_light),
            (ThemePreference::Dark, "theme-dark", text().theme_dark),
        ];
        let theme_choice = style::segmented(
            themes.into_iter().map(|(theme, id, label)| {
                style::segment(id, label, settings.theme == theme, cx)
                    .on_click(cx.listener(move |_, _, _, cx| Self::set_theme(theme, cx)))
                    .test_support()
            }),
            cx,
        );
        let translucency = div()
            .v_flex()
            .gap_1()
            .child(style::setting_row(
                AssetIcon::Layers,
                text().translucent_window,
                Switch::new("translucent-window")
                    .accessibility_label(text().translucent_window)
                    .checked(settings.translucent_window)
                    .on_change(cx.listener(|_, checked: &bool, window, cx| {
                        Self::set_translucent(*checked, window, cx);
                    })),
                cx,
            ))
            .child(
                div()
                    .pl_7()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(text().translucent_window_hint),
            );
        div()
            .v_flex()
            .gap_2()
            .child(style::eyebrow(text().appearance, cx))
            .child(style::row_list(
                [
                    style::setting_row(IconName::Palette, text().theme, theme_choice, cx),
                    translucency,
                ],
                cx,
            ))
    }

    /// The background picture: choose, remove, blur.
    fn render_picture_settings(&self, cx: &mut Context<Self>) -> Div {
        let settings = AppState::settings(cx).clone();
        let picture_name: SharedString = match &settings.background_image {
            None => text().background_none.into(),
            Some(path) => path
                .file_name()
                .map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
                .into(),
        };
        let buttons = div()
            .h_flex()
            .flex_none()
            .gap_1()
            .when(settings.background_image.is_some(), |buttons| {
                buttons.child(
                    Button::new("background-remove")
                        .ghost()
                        .small()
                        .label(text().background_remove)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.clear_backdrop(window, cx)),
                        ),
                )
            })
            .child(
                Button::new("background-choose")
                    .small()
                    .label(text().background_choose)
                    .on_click(cx.listener(|_, _, window, cx| Self::choose_backdrop(window, cx))),
            );
        div()
            .v_flex()
            .gap_2()
            .child(style::eyebrow(text().background_picture, cx))
            .child(style::row_list(
                [
                    style::setting_row(AssetIcon::Image, picture_name, buttons, cx),
                    style::setting_row(
                        AssetIcon::Droplet,
                        text().background_blur,
                        Switch::new("background-blur")
                            .accessibility_label(text().background_blur)
                            .checked(settings.blur_background)
                            .on_change(cx.listener(|this, checked: &bool, window, cx| {
                                this.set_blur(*checked, window, cx);
                            })),
                        cx,
                    ),
                ],
                cx,
            ))
            .when(self.backdrop_unreadable, |section| {
                section.child(style::callout(
                    IconName::TriangleAlert,
                    cx.theme().warning,
                    div().text_sm().child(text().background_unreadable),
                    cx,
                ))
            })
    }

    fn render_title_bar() -> impl IntoElement {
        // macOS draws the traffic lights at the left of this strip; elsewhere it is free.
        let brand_inset = if cfg!(target_os = "macos") {
            px(84.)
        } else {
            px(16.)
        };
        style::title_bar().child(
            div()
                .h_flex()
                .w(style::SIDEBAR_WIDTH)
                .pl(brand_inset)
                .gap_2()
                .child(style::logo(px(18.)))
                .child(div().text_sm().font_semibold().child("Dari")),
        )
    }
}

impl Render for Home {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let page = match self.page {
            Page::Device => self.host.clone().into_any_element(),
            Page::Connect => self.connect.clone().into_any_element(),
            Page::Settings => self.render_settings(cx).into_any_element(),
        };
        // Over a picture, pages start lower, so the picture shows across the top as the
        // window's hero and the page sits where the veil has made the surface calm.
        let page_top = match &self.backdrop {
            Some(_) => (viewport.height * 0.24).max(style::TITLE_BAR_HEIGHT + px(16.)),
            None => style::TITLE_BAR_HEIGHT + px(16.),
        };
        let scenery = self.backdrop.as_ref().map(|scenery| {
            let layer = |picture: Arc<RenderImage>| {
                img(picture)
                    .absolute()
                    .inset_0()
                    .size_full()
                    .object_fit(ObjectFit::Cover)
            };
            div()
                .absolute()
                .inset_0()
                .opacity(style::picture_opacity(cx))
                .child(layer(scenery.picture.clone()))
                .child(layer(scenery.glow.clone()))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(style::veil(scenery.average, cx)),
                )
        });
        div()
            .relative()
            .size_full()
            .text_color(cx.theme().foreground)
            .children(scenery)
            .child(
                div()
                    .h_flex()
                    .size_full()
                    .child(self.render_sidebar(viewport, cx))
                    .child(
                        div()
                            .id("home-body")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .when(self.backdrop.is_none(), |body| {
                                body.bg(style::content_surface(cx))
                            })
                            .overflow_y_scrollbar()
                            .child(
                                div()
                                    .v_flex()
                                    .w_full()
                                    .max_w(px(620.))
                                    .mx_auto()
                                    .px_10()
                                    .pt(page_top)
                                    .pb_10()
                                    .child(page),
                            ),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .child(Self::render_title_bar()),
            )
    }
}

/// The background picture's layers on the GPU (see [`backdrop`]).
struct Scenery {
    picture: Arc<RenderImage>,
    frost: Arc<RenderImage>,
    glow: Arc<RenderImage>,
    /// The picture's average color.
    average: Hsla,
}

impl Scenery {
    fn new(prepared: backdrop::Backdrop) -> Self {
        let [red, green, blue] = prepared.average;
        Self {
            picture: Arc::new(prepared.picture),
            frost: Arc::new(prepared.frost),
            glow: Arc::new(prepared.glow),
            average: rgb(u32::from_be_bytes([0, red, green, blue])).into(),
        }
    }
}

enum Hosting {
    Off,
    Running(HostHandle),
    Failed(String),
}

/// Shares this device: address, one-time password, the connected viewer, and permissions.
pub(crate) struct HostPanel {
    hosting: Hosting,
    password: Option<AccessPassword>,
    reveal_password: bool,
    viewer: Option<PeerInfo>,
    session_status: Option<HostStatus>,
    addresses: Vec<IpAddr>,
    permissions: LocalPermissions,
    /// A viewer waiting for the host user's decision.
    approval: Option<(PeerInfo, ApprovalRequest)>,
    relay: Option<RelayStatus>,
    relay_input: Entity<InputState>,
    advertisement: Option<Advertisement>,
    _relay_subscription: Subscription,
    host_events: Option<Task<()>>,
    _refresh: Task<()>,
}

impl HostPanel {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let relay_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(text().relay_placeholder)
                .default_value(AppState::settings(cx).relay_address.clone())
        });
        let relay_subscription =
            cx.subscribe_in(&relay_input, window, |this, input, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let value = input.read(cx).value().trim().to_owned();
                    this.set_relay(value, cx);
                }
            });
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let Ok(()) = this.update(cx, HostPanel::refresh) else {
                    break;
                };
            }
        });
        let mut panel = Self {
            hosting: Hosting::Off,
            password: None,
            reveal_password: true,
            viewer: None,
            session_status: None,
            addresses: local_addresses(),
            permissions: LocalPermissions::check(),
            approval: None,
            relay: None,
            relay_input,
            _relay_subscription: relay_subscription,
            advertisement: None,
            host_events: None,
            _refresh: refresh,
        };
        if AppState::settings(cx).hosting_enabled {
            panel.start(cx);
        }
        panel
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let addresses = local_addresses();
        let permissions = LocalPermissions::check();
        if addresses != self.addresses || permissions != self.permissions {
            self.addresses = addresses;
            self.permissions = permissions;
            cx.notify();
        }
    }

    fn port(&self, cx: &App) -> u16 {
        match &self.hosting {
            Hosting::Running(handle) => handle.local_address().port(),
            Hosting::Off | Hosting::Failed(_) => AppState::settings(cx).port,
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let identity = match AppState::identity(cx) {
            Ok(identity) => identity,
            Err(error) => {
                self.hosting = Hosting::Failed(error);
                return;
            }
        };
        let config = HostConfig {
            bind_address: (std::net::Ipv6Addr::UNSPECIFIED, AppState::settings(cx).port).into(),
            host_name: device_name(),
            stream: StreamSettings::default(),
            require_approval: AppState::settings(cx).require_approval,
            clipboard: AppState::settings(cx).clipboard_sync,
            relay: Some(AppState::settings(cx).relay_address.clone())
                .filter(|relay| !relay.is_empty()),
        };
        let started = TokioRuntime::enter(cx, || {
            start_host(config, identity, Arc::new(SystemPlatform))
        });
        match started {
            Ok((handle, mut events)) => {
                let port = handle.local_address().port();
                self.hosting = Hosting::Running(handle);
                self.update_advertisement(port, cx);
                self.host_events = Some(cx.spawn(async move |this, cx| {
                    while let Some(event) = events.recv().await {
                        let Ok(()) = this.update(cx, |this, cx| this.on_host_event(event, cx))
                        else {
                            break;
                        };
                    }
                }));
            }
            Err(error) => {
                tracing::warn!(%error, "cannot start hosting");
                self.hosting = Hosting::Failed(error.to_string());
            }
        }
    }

    fn stop(&mut self) {
        self.hosting = Hosting::Off;
        self.host_events = None;
        self.advertisement = None;
        self.approval = None;
        self.relay = None;
        self.password = None;
        self.viewer = None;
        self.session_status = None;
    }

    fn set_hosting(&mut self, enabled: bool, cx: &mut Context<Self>) {
        AppState::update_settings(cx, |settings| settings.hosting_enabled = enabled);
        if enabled {
            self.start(cx);
        } else {
            self.stop();
        }
        cx.notify();
    }

    fn on_host_event(&mut self, event: HostEvent, cx: &mut Context<Self>) {
        match event {
            HostEvent::PasswordChanged(password) => self.password = password,
            HostEvent::SessionStarted(peer) => {
                self.viewer = Some(peer);
                self.session_status = None;
                // Authenticating consumed the one-time password; a new one is issued when the
                // session ends, so showing the old one would only mislead.
                self.password = None;
            }
            HostEvent::ApprovalRequested { peer, request } => self.approval = Some((peer, request)),
            HostEvent::SessionStatus(status) => {
                self.approval = None;
                self.session_status = Some(status);
            }
            HostEvent::Relay(status) => self.relay = Some(status),
            HostEvent::SessionEnded { .. } => {
                self.approval = None;
                self.viewer = None;
                self.session_status = None;
            }
        }
        cx.notify();
    }

    /// Announces this device on the local network while hosting, if the user allows it.
    fn update_advertisement(&mut self, port: u16, cx: &App) {
        self.advertisement = None;
        if !AppState::settings(cx).lan_discovery {
            return;
        }
        let Ok(identity) = AppState::identity(cx) else {
            return;
        };
        match Advertisement::start(&device_name(), port, &identity.fingerprint()) {
            Ok(advertisement) => self.advertisement = Some(advertisement),
            Err(error) => tracing::warn!(%error, "cannot announce this device on the network"),
        }
    }

    /// Saves the relay address and re-registers by restarting hosting.
    fn set_relay(&mut self, relay: String, cx: &mut Context<Self>) {
        if AppState::settings(cx).relay_address == relay {
            return;
        }
        AppState::update_settings(cx, |settings| settings.relay_address = relay);
        if matches!(self.hosting, Hosting::Running(_)) {
            self.stop();
            self.start(cx);
        }
        cx.notify();
    }

    fn viewer_name(&self) -> String {
        self.viewer
            .as_ref()
            .map(|viewer| viewer.name.clone())
            .unwrap_or_default()
    }

    fn answer(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) {
        if let Some((_, request)) = self.approval.take() {
            request.respond(decision);
        }
        cx.notify();
    }

    fn set_policy(
        &mut self,
        change: impl FnOnce(&mut crate::settings::Settings),
        cx: &mut Context<Self>,
    ) {
        AppState::update_settings(cx, change);
        let settings = AppState::settings(cx).clone();
        if let Hosting::Running(handle) = &self.hosting {
            handle.set_policy(settings.require_approval, settings.clipboard_sync);
            let port = handle.local_address().port();
            self.update_advertisement(port, cx);
        }
        cx.notify();
    }

    fn render_approval(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (peer, _) = self.approval.as_ref()?;
        let primary = cx.theme().primary;
        Some(
            div()
                .v_flex()
                .gap_4()
                .p_4()
                .rounded(cx.theme().radius_lg)
                .bg(primary.opacity(0.07))
                .border_1()
                .border_color(primary.opacity(0.45))
                .child(
                    div()
                        .h_flex()
                        .items_start()
                        .gap_3()
                        .child(style::icon_badge(AssetIcon::ShieldCheck, primary, px(36.)))
                        .child(
                            div()
                                .v_flex()
                                .flex_1()
                                .gap_0p5()
                                .min_w_0()
                                .child(div().text_sm().font_semibold().child(text().approval_title))
                                .child(div().text_sm().child(text().approval_prompt(&peer.name)))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(text().approval_hint),
                                ),
                        ),
                )
                .child(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("approval-decline")
                                .small()
                                .ghost()
                                .label(text().decline)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.answer(ApprovalDecision::Deny, cx);
                                })),
                        )
                        .child(
                            Button::new("approval-view")
                                .small()
                                .icon(IconName::Eye)
                                .label(text().allow_view_only)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.answer(ApprovalDecision::ViewOnly, cx);
                                })),
                        )
                        .child(
                            Button::new("approval-control")
                                .small()
                                .primary()
                                .icon(AssetIcon::MousePointer2)
                                .label(text().allow_control)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.answer(ApprovalDecision::AllowControl, cx);
                                })),
                        ),
                ),
        )
    }

    /// The one-time password, or nothing while a session has used it up.
    fn render_password(&self, cx: &mut Context<Self>) -> Option<Div> {
        let password = self.password.as_ref()?.display_text().as_str().to_owned();
        let shown: SharedString = if self.reveal_password {
            password.clone().into()
        } else {
            "•••••-•••••".into()
        };
        Some(
            div()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .h_flex()
                        .justify_between()
                        .child(style::eyebrow(text().password, cx))
                        .child(
                            div()
                                .h_flex()
                                .gap_0p5()
                                .child(Clipboard::new("password-copy").value(password))
                                .child(
                                    Button::new("password-reveal")
                                        .ghost()
                                        .xsmall()
                                        .icon(if self.reveal_password {
                                            IconName::EyeOff
                                        } else {
                                            IconName::Eye
                                        })
                                        .tooltip(if self.reveal_password {
                                            text().hide_password
                                        } else {
                                            text().show_password
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.reveal_password = !this.reveal_password;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("password-regenerate")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::RefreshCw)
                                        .tooltip(text().new_password)
                                        .on_click(cx.listener(|this, _, _, _| {
                                            if let Hosting::Running(handle) = &this.hosting {
                                                handle.regenerate_password();
                                            }
                                        })),
                                ),
                        ),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_3xl()
                        .font_semibold()
                        .line_height(px(40.))
                        .child(shown),
                ),
        )
    }

    /// Everything a viewer needs to reach this device: password, address, and relay ID.
    fn render_credentials(&self, cx: &mut Context<Self>) -> Div {
        let port = self.port(cx);
        let mono = cx.theme().mono_font_family.clone();
        let shown: Vec<(IpAddr, String)> = self
            .addresses
            .iter()
            .map(|address| (*address, shown_address(*address, port)))
            .collect();
        let detail = |label: &'static str, value: AnyElement| {
            div()
                .h_flex()
                .justify_between()
                .gap_4()
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(value)
        };
        let copyable = |id: SharedString, value: String, small: bool| {
            div()
                .h_flex()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(mono.clone())
                        .map(|text| {
                            if small {
                                text.text_xs()
                            } else {
                                text.text_sm()
                            }
                        })
                        .child(value.clone()),
                )
                .child(Clipboard::new(id).value(value))
        };

        let mut details = div()
            .v_flex()
            .gap_2p5()
            .pt_4()
            .border_t_1()
            .border_color(cx.theme().border);
        match shown.split_first() {
            None => {
                details = details.child(detail(
                    text().addresses,
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(text().no_addresses)
                        .into_any_element(),
                ));
            }
            Some(((address, first), rest)) => {
                details = details.child(detail(
                    text().addresses,
                    copyable(format!("address-{address}").into(), first.clone(), false)
                        .into_any_element(),
                ));
                if !rest.is_empty() {
                    details = details.child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(style::eyebrow(text().other_addresses, cx))
                            .children(rest.iter().map(|(address, shown)| {
                                copyable(format!("address-{address}").into(), shown.clone(), true)
                                    .text_color(cx.theme().muted_foreground)
                            })),
                    );
                }
            }
        }
        if let Some(RelayStatus::Registered(id)) = &self.relay {
            details = details.child(detail(
                text().my_id,
                copyable("relay-id-copy".into(), id.to_string(), false).into_any_element(),
            ));
        }

        div()
            .v_flex()
            .gap_4()
            .children(self.render_password(cx))
            .child(details)
    }

    fn render_permissions(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.permissions.all_granted() {
            return None;
        }
        let mut message = div().v_flex().gap_2().text_sm();
        if !self.permissions.screen {
            message = message.child(text().screen_permission_missing);
        }
        if !self.permissions.input {
            message = message.child(text().input_permission_missing);
        }
        let current = self.permissions;
        message = message.child(
            div()
                .h_flex()
                .gap_2()
                .child(
                    Button::new("permission-request")
                        .small()
                        .label(text().request_permission)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            permissions::request_missing(current);
                            this.refresh(cx);
                        })),
                )
                .child(
                    Button::new("permission-settings")
                        .small()
                        .ghost()
                        .icon(IconName::ExternalLink)
                        .label(text().open_settings)
                        .on_click(move |_, _, cx| {
                            cx.open_url(permissions::settings_url(current));
                        }),
                ),
        );
        Some(style::callout(
            IconName::TriangleAlert,
            cx.theme().warning,
            message,
            cx,
        ))
    }

    fn render_session(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.approval.is_some() {
            // The approval card speaks for the waiting viewer until the host user decides.
            return None;
        }
        let Some(viewer) = &self.viewer else {
            return Some(
                div()
                    .h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(style::status_dot(cx.theme().success))
                    .child(text().waiting_for_viewer),
            );
        };
        let success = cx.theme().success;
        let mut about = div().v_flex().flex_1().min_w_0().gap_0p5().child(
            div()
                .text_sm()
                .font_semibold()
                .child(text().viewer_connected(&viewer.name)),
        );
        if let Some(status) = self.session_status {
            let note = |content: &'static str| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(content)
            };
            if status.screen != Availability::Available {
                about = about.child(note(text().screen_permission_missing));
            }
            if status.input != Availability::Available {
                about = about.child(note(text().input_permission_missing));
            }
        }
        Some(
            div()
                .h_flex()
                .gap_3()
                .p_3()
                .rounded(cx.theme().radius_lg)
                .bg(success.opacity(0.08))
                .border_1()
                .border_color(success.opacity(0.35))
                .child(style::icon_badge(
                    AssetIcon::MonitorSmartphone,
                    success,
                    px(36.),
                ))
                .child(about)
                .child(
                    Button::new("session-end")
                        .danger()
                        .small()
                        .icon(AssetIcon::Unplug)
                        .label(text().end_session)
                        .on_click(cx.listener(|this, _, _, _| {
                            if let Hosting::Running(handle) = &this.hosting {
                                handle.end_session();
                            }
                        })),
                ),
        )
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> Div {
        let settings = AppState::settings(cx).clone();
        let relay_state = match &self.relay {
            Some(RelayStatus::Registered(_)) => Some(cx.theme().success),
            Some(RelayStatus::Connecting) => Some(cx.theme().warning),
            Some(RelayStatus::Unavailable(_)) => Some(cx.theme().danger),
            None => None,
        };
        let mut relay = div().v_flex().gap_2().child(style::setting_row(
            AssetIcon::Waypoints,
            text().relay_server,
            div().children(relay_state.map(style::status_dot)),
            cx,
        ));
        relay = relay.child(style::field(
            Input::new(&self.relay_input).id("relay-address").small(),
            cx,
        ));
        match &self.relay {
            Some(RelayStatus::Connecting) => {
                relay = relay.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(text().relay_connecting),
                );
            }
            Some(RelayStatus::Unavailable(error)) => {
                relay = relay.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(text().relay_unavailable(error)),
                );
            }
            Some(RelayStatus::Registered(_)) | None => {}
        }
        div()
            .v_flex()
            .gap_2()
            .child(style::eyebrow(text().sharing_settings, cx))
            .child(style::row_list(
                [
                    style::setting_row(
                        AssetIcon::ShieldCheck,
                        text().require_approval,
                        Switch::new("policy-approval")
                            .accessibility_label(text().require_approval)
                            .checked(settings.require_approval)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.set_policy(|settings| settings.require_approval = checked, cx);
                            })),
                        cx,
                    ),
                    style::setting_row(
                        AssetIcon::Clipboard,
                        text().clipboard_sync,
                        Switch::new("policy-clipboard")
                            .accessibility_label(text().clipboard_sync)
                            .checked(settings.clipboard_sync)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.set_policy(|settings| settings.clipboard_sync = checked, cx);
                            })),
                        cx,
                    ),
                    style::setting_row(
                        AssetIcon::Radar,
                        text().lan_discovery,
                        Switch::new("policy-discovery")
                            .accessibility_label(text().lan_discovery)
                            .checked(settings.lan_discovery)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.set_policy(|settings| settings.lan_discovery = checked, cx);
                            })),
                        cx,
                    ),
                    relay,
                ],
                cx,
            ))
    }
}

/// How an address of this device is written for a viewer to type.
fn shown_address(address: IpAddr, port: u16) -> String {
    match address {
        IpAddr::V4(v4) if port == crate::config::DEFAULT_PORT => v4.to_string(),
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

impl Render for HostPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = !matches!(self.hosting, Hosting::Off);
        let header = style::page_header(
            text().this_device,
            device_name(),
            div()
                .h_flex()
                .flex_none()
                .gap_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text().allow_remote_access)
                .child(
                    Switch::new("hosting")
                        .accessibility_label(text().allow_remote_access)
                        .checked(enabled)
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_hosting(*checked, cx);
                        })),
                ),
            cx,
        );
        let mut page = div()
            .v_flex()
            .gap_7()
            .child(header)
            .children(self.render_permissions(cx));
        page = match &self.hosting {
            Hosting::Off => page.child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(div().text_sm().font_medium().child(text().not_accepting))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(text().not_accepting_hint),
                    ),
            ),
            Hosting::Failed(error) => page.child(style::callout(
                IconName::CircleX,
                cx.theme().danger,
                div()
                    .v_flex()
                    .gap_0p5()
                    .text_sm()
                    .child(div().font_semibold().child(text().hosting_failed))
                    .child(error.clone()),
                cx,
            )),
            Hosting::Running(_) => page
                .children(self.render_approval(cx))
                .child(self.render_credentials(cx))
                .children(self.render_session(cx)),
        };
        page.child(self.render_settings(cx))
    }
}

/// Connects to another device.
pub(crate) struct ConnectPanel {
    address: Entity<InputState>,
    password: Entity<InputState>,
    connecting: bool,
    error: Option<String>,
    /// Hosts announced on the local network, newest last.
    nearby: Vec<NearbyDevice>,
    _discovery: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ConnectPanel {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let recent = AppState::settings(cx)
            .recent_addresses
            .first()
            .cloned()
            .unwrap_or_default();
        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(text().address_placeholder)
                .default_value(recent)
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(text().password_placeholder)
                .masked(true)
        });
        let submit_on_enter = |this: &mut Self,
                               _: &Entity<InputState>,
                               event: &InputEvent,
                               window: &mut Window,
                               cx: &mut Context<Self>| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.connect(window, cx);
            }
        };
        let subscriptions = vec![
            cx.subscribe_in(&address, window, submit_on_enter),
            cx.subscribe_in(&password, window, submit_on_enter),
        ];
        let discovery = Self::browse(cx);
        Self {
            address,
            password,
            connecting: false,
            error: None,
            nearby: Vec::new(),
            _discovery: discovery,
            _subscriptions: subscriptions,
        }
    }

    /// Watches the local network for hosts, ignoring this device's own announcement.
    fn browse(cx: &mut Context<Self>) -> Option<Task<()>> {
        let own_hint = AppState::identity(cx)
            .ok()
            .map(|identity| fingerprint_hint(&identity.fingerprint()));
        let mut browser = match Browser::start() {
            Ok(browser) => browser,
            Err(error) => {
                tracing::warn!(%error, "cannot browse the local network");
                return None;
            }
        };
        // `Browser::next` does not depend on an executor, so a GPUI task can await it.
        Some(cx.spawn(async move |this, cx| {
            while let Some(event) = browser.next().await {
                let own = own_hint.clone();
                let Ok(()) = this.update(cx, |this, cx| {
                    match event {
                        DiscoveryEvent::Found(device) => {
                            if own.as_deref() == Some(device.fingerprint_hint.as_str()) {
                                return;
                            }
                            this.nearby.retain(|known| known.id != device.id);
                            this.nearby.push(device);
                        }
                        DiscoveryEvent::Lost { id } => this.nearby.retain(|known| known.id != id),
                    }
                    cx.notify();
                }) else {
                    break;
                };
            }
        }))
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting {
            return;
        }
        let address_text = self.address.read(cx).value().to_string();
        let password = match AccessPassword::parse(&self.password.read(cx).value()) {
            Ok(password) => password,
            Err(error) => {
                self.error = Some(text().connect_failed(text().password_error(&error)));
                cx.notify();
                return;
            }
        };
        self.connecting = true;
        self.error = None;
        cx.notify();

        let map_shortcut_modifier = AppState::settings(cx).map_shortcut_modifier;
        let clipboard_sync = AppState::settings(cx).clipboard_sync;
        let relay = AppState::settings(cx).relay_address.clone();
        let target = address_text.clone();
        let attempt = TokioRuntime::spawn(cx, async move {
            let target = resolve_target(&target, &relay)
                .await
                .map_err(|error| error.to_string())?;
            let config = ViewerConfig {
                target,
                client_name: device_name(),
                map_shortcut_modifier,
                clipboard: clipboard_sync.then(SystemClipboard::factory),
            };
            connect_viewer(config, &password)
                .await
                .map_err(|error| describe_connect_error(&error))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = attempt.await.unwrap_or_else(|error| Err(error.to_string()));
            let _updated = this.update_in(cx, |this, window, cx| {
                this.connecting = false;
                match result {
                    Ok((viewer, events)) => {
                        this.error = None;
                        this.password
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        AppState::update_settings(cx, |settings| {
                            settings.remember_address(&address_text);
                        });
                        if let Err(error) = open_viewer_window(viewer, events, cx).map(drop) {
                            this.error = Some(text().connect_failed(&error.to_string()));
                        }
                    }
                    Err(message) => this.error = Some(text().connect_failed(&message)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// The address to put in the connect form for a discovered device.
fn nearby_address(device: &NearbyDevice) -> String {
    let address = device
        .addresses
        .first()
        .copied()
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));
    shown_address(address, device.port)
}

fn describe_connect_error(error: &ConnectError) -> String {
    match error {
        ConnectError::Handshake(HandshakeError::Rejected(reason)) => {
            text().rejection(*reason).into()
        }
        other => other.to_string(),
    }
}

impl ConnectPanel {
    /// Puts `address` in the form and moves on to the password.
    fn pick(&mut self, address: String, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        self.address.update(cx, |input, cx| {
            input.set_value(address, window, cx);
        });
        self.password.read(cx).focus_handle(cx).focus(window, cx);
    }
}

impl Render for ConnectPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, input: Input| {
            div()
                .v_flex()
                .gap_1p5()
                .child(div().text_sm().font_medium().child(label))
                .child(style::field(input, cx))
        };
        let muted = cx.theme().muted_foreground;
        div()
            .v_flex()
            .gap_7()
            .child(style::page_header(
                text().control_remote_device,
                text().connect_subtitle,
                div(),
                cx,
            ))
            .child(
                div()
                    .v_flex()
                    .gap_4()
                    .child(field(
                        text().address,
                        Input::new(&self.address).id("connect-address").prefix(
                            div()
                                .text_color(muted)
                                .child(Icon::new(IconName::Globe).small()),
                        ),
                    ))
                    .child(field(
                        text().password,
                        Input::new(&self.password).id("connect-password").prefix(
                            div()
                                .text_color(muted)
                                .child(Icon::new(AssetIcon::KeyRound).small()),
                        ),
                    ))
                    .child(style::setting_row(
                        AssetIcon::Command,
                        text().map_shortcut_modifier,
                        Switch::new("map-shortcut-modifier")
                            .accessibility_label(text().map_shortcut_modifier)
                            .checked(AppState::settings(cx).map_shortcut_modifier)
                            .on_change(cx.listener(|_, checked: &bool, _, cx| {
                                let checked = *checked;
                                AppState::update_settings(cx, |settings| {
                                    settings.map_shortcut_modifier = checked;
                                });
                                cx.notify();
                            })),
                        cx,
                    ))
                    .child(
                        Button::new("connect")
                            .primary()
                            .w_full()
                            .label(if self.connecting {
                                text().connecting
                            } else {
                                text().connect
                            })
                            .loading(self.connecting)
                            .disabled(self.connecting)
                            .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
                    )
                    .children(self.error.clone().map(|error| {
                        style::callout(
                            IconName::CircleX,
                            cx.theme().danger,
                            div()
                                .id("connect-error-text")
                                .text_sm()
                                .child(error)
                                .test_support(),
                            cx,
                        )
                        .id("connect-error")
                        .test_support()
                    })),
            )
    }
}
