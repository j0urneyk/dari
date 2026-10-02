//! Home window: share this device, or connect to another one.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable as _, IconName, Sizable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use open_desk_media::StreamSettings;
use open_desk_net::{AccessPassword, ConnectError, HandshakeError, PeerInfo};
use open_desk_proto::{Availability, HostStatus};
use open_desk_session::{
    HostConfig, HostEvent, HostHandle, SystemPlatform, ViewerConfig, connect_viewer, start_host,
};

use crate::config::{device_name, local_addresses, resolve_address};
use crate::permissions::{self, LocalPermissions};
use crate::runtime::TokioRuntime;
use crate::state::AppState;
use crate::text::text;
use crate::viewer::open_viewer_window;

/// How often permissions and network addresses are re-read while the window is open.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

pub struct Home {
    host: Entity<HostPanel>,
    connect: Entity<ConnectPanel>,
}

impl std::fmt::Debug for Home {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Home").finish_non_exhaustive()
    }
}

impl Home {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            host: cx.new(HostPanel::new),
            connect: cx.new(|cx| ConnectPanel::new(window, cx)),
        }
    }
}

impl Home {
    #[doc(hidden)]
    pub fn has_password(&self, cx: &App) -> bool {
        self.host.read(cx).password.is_some()
    }
}

impl Render for Home {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .size_full()
            .p_6()
            .gap_6()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(div().text_xl().font_semibold().child("open-desk"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(text().app_subtitle),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .items_start()
                    .gap_6()
                    .child(self.host.clone())
                    .child(self.connect.clone()),
            )
    }
}

fn card(title: &'static str, cx: &App) -> Div {
    div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .gap_4()
        .p_5()
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius_lg)
        .child(div().text_base().font_semibold().child(title))
}

fn label(content: &'static str, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(content)
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
    host_events: Option<Task<()>>,
    _refresh: Task<()>,
}

impl HostPanel {
    fn new(cx: &mut Context<Self>) -> Self {
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
        };
        let started = TokioRuntime::enter(cx, || {
            start_host(config, &identity, Arc::new(SystemPlatform))
        });
        match started {
            Ok((handle, mut events)) => {
                self.hosting = Hosting::Running(handle);
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
            }
            HostEvent::SessionStatus(status) => self.session_status = Some(status),
            HostEvent::SessionEnded { .. } => {
                self.viewer = None;
                self.session_status = None;
            }
        }
        cx.notify();
    }

    fn render_addresses(&self, cx: &App) -> Div {
        let port = self.port(cx);
        let mut list = div().v_flex().gap_1();
        if self.addresses.is_empty() {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(text().no_addresses),
            );
        }
        for address in &self.addresses {
            let shown = match address {
                IpAddr::V4(v4) if port == crate::config::DEFAULT_PORT => v4.to_string(),
                IpAddr::V4(v4) => format!("{v4}:{port}"),
                IpAddr::V6(v6) => format!("[{v6}]:{port}"),
            };
            list = list.child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(
                        div()
                            .font_family("monospace")
                            .text_sm()
                            .child(shown.clone()),
                    )
                    .child(
                        Clipboard::new(SharedString::from(format!("address-{address}")))
                            .value(shown),
                    ),
            );
        }
        list
    }

    fn render_password(&self, cx: &mut Context<Self>) -> Div {
        let shown: SharedString = match &self.password {
            Some(password) if self.reveal_password => {
                password.display_text().as_str().to_owned().into()
            }
            Some(_) => "•••••-•••••".into(),
            None => "—".into(),
        };
        let copy_value: SharedString = self
            .password
            .as_ref()
            .map(|password| password.display_text().as_str().to_owned().into())
            .unwrap_or_default();
        div()
            .h_flex()
            .gap_2()
            .child(
                div()
                    .font_family("monospace")
                    .text_2xl()
                    .font_semibold()
                    .child(shown),
            )
            .when(self.password.is_some(), |row| {
                row.child(Clipboard::new("password-copy").value(copy_value))
                    .child(
                        Button::new("password-reveal")
                            .ghost()
                            .small()
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
                            .small()
                            .icon(IconName::RefreshCw)
                            .tooltip(text().new_password)
                            .on_click(cx.listener(|this, _, _, _| {
                                if let Hosting::Running(handle) = &this.hosting {
                                    handle.regenerate_password();
                                }
                            })),
                    )
            })
    }

    fn render_permissions(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.permissions.all_granted() {
            return None;
        }
        let mut message = div().v_flex().gap_1().text_sm();
        if !self.permissions.screen {
            message = message.child(text().screen_permission_missing);
        }
        if !self.permissions.input {
            message = message.child(text().input_permission_missing);
        }
        let current = self.permissions;
        Some(
            div()
                .v_flex()
                .gap_2()
                .p_3()
                .rounded(cx.theme().radius)
                .bg(cx.theme().warning.opacity(0.12))
                .border_1()
                .border_color(cx.theme().warning.opacity(0.4))
                .child(message)
                .child(
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
                                .label(text().open_settings)
                                .on_click(move |_, _, cx| {
                                    cx.open_url(permissions::settings_url(current));
                                }),
                        ),
                ),
        )
    }

    fn render_session(&self, cx: &mut Context<Self>) -> Div {
        let Some(viewer) = &self.viewer else {
            return div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text().waiting_for_viewer);
        };
        let mut row = div().v_flex().gap_2().child(
            div()
                .text_sm()
                .font_semibold()
                .child(text().viewer_connected(&viewer.name)),
        );
        if let Some(status) = self.session_status {
            if status.screen != Availability::Available {
                row = row.child(div().text_xs().child(text().screen_permission_missing));
            }
            if status.input != Availability::Available {
                row = row.child(div().text_xs().child(text().input_permission_missing));
            }
        }
        row.child(
            Button::new("session-end")
                .danger()
                .small()
                .label(text().end_session)
                .on_click(cx.listener(|this, _, _, _| {
                    if let Hosting::Running(handle) = &this.hosting {
                        handle.end_session();
                    }
                })),
        )
    }
}

impl Render for HostPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = !matches!(self.hosting, Hosting::Off);
        let mut panel = card(text().this_device, cx).child(
            Switch::new("hosting")
                .label(text().allow_remote_access)
                .checked(enabled)
                .on_change(
                    cx.listener(|this, checked: &bool, _, cx| this.set_hosting(*checked, cx)),
                ),
        );
        panel = match &self.hosting {
            Hosting::Off => panel.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(text().not_accepting),
            ),
            Hosting::Failed(error) => panel.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(format!("{}: {error}", text().hosting_failed)),
            ),
            Hosting::Running(_) => panel
                .child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(label(text().addresses, cx))
                        .child(self.render_addresses(cx)),
                )
                .child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(label(text().password, cx))
                        .child(self.render_password(cx)),
                )
                .child(self.render_session(cx)),
        };
        panel.children(self.render_permissions(cx))
    }
}

/// Connects to another device.
pub(crate) struct ConnectPanel {
    address: Entity<InputState>,
    password: Entity<InputState>,
    connecting: bool,
    error: Option<String>,
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
        Self {
            address,
            password,
            connecting: false,
            error: None,
            _subscriptions: subscriptions,
        }
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
        let target = address_text.clone();
        let attempt = TokioRuntime::spawn(cx, async move {
            let address = resolve_address(&target)
                .await
                .map_err(|error| error.to_string())?;
            let config = ViewerConfig {
                address,
                client_name: device_name(),
                map_shortcut_modifier,
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

fn describe_connect_error(error: &ConnectError) -> String {
    match error {
        ConnectError::Handshake(HandshakeError::Rejected(reason)) => {
            text().rejection(*reason).into()
        }
        other => other.to_string(),
    }
}

impl Render for ConnectPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let recent = AppState::settings(cx).recent_addresses.clone();
        card(text().control_remote_device, cx)
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(label(text().address, cx))
                    .child(Input::new(&self.address)),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(label(text().password, cx))
                    .child(Input::new(&self.password)),
            )
            .child(
                Switch::new("map-shortcut-modifier")
                    .label(text().map_shortcut_modifier)
                    .checked(AppState::settings(cx).map_shortcut_modifier)
                    .on_change(cx.listener(|_, checked: &bool, _, cx| {
                        let checked = *checked;
                        AppState::update_settings(cx, |settings| {
                            settings.map_shortcut_modifier = checked;
                        });
                        cx.notify();
                    })),
            )
            .child(
                Button::new("connect")
                    .primary()
                    .label(if self.connecting {
                        text().connecting
                    } else {
                        text().connect
                    })
                    .loading(self.connecting)
                    .disabled(self.connecting)
                    .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
            )
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_sm().text_color(cx.theme().danger).child(error)),
            )
            .when(!recent.is_empty(), |panel| {
                panel.child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(label(text().recent, cx))
                        .children(recent.into_iter().map(|address| {
                            Button::new(SharedString::from(format!("recent-{address}")))
                                .ghost()
                                .small()
                                .label(address.clone())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let address = address.clone();
                                    this.address.update(cx, |input, cx| {
                                        input.set_value(address, window, cx);
                                    });
                                }))
                        })),
                )
            })
    }
}
