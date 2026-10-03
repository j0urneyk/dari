//! A window showing one remote screen and forwarding keyboard and pointer input to it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use dari_media::DecodedFrame;
use dari_proto::{
    Availability, DisplayDescription, HostStatus, InputEvent, KeyCode, MouseButton as RemoteButton,
    PointerPosition, QualityPreset,
};
use dari_session::{SessionEndReason, ViewerEvent, ViewerHandle};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use image::{Frame, RgbaImage};
use tokio::sync::mpsc;

use crate::keymap::{key_code, modifier_changes};
use crate::style;
use crate::text::text;
use crate::video_layout::{ScrollAccumulator, letterbox, pointer_position};

const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1280.),
    height: px(800.),
};
const MIN_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(640.),
    height: px(400.),
};
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Key context of the remote screen while it has focus.
const REMOTE_SCREEN_CONTEXT: &str = "RemoteScreen";

/// Registers the viewer's key bindings. Call once during app initialization.
///
/// gpui-kit's `Root` binds Tab, Shift-Tab, and the platform copy shortcut for focus navigation
/// and copying. Inside the remote screen those keys belong to the remote machine, so they are
/// unbound there and reach the screen's key listeners instead.
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
        KeyBinding::new("shift-tab", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
        KeyBinding::new("cmd-c", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
        KeyBinding::new("ctrl-c", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
    ]);
}

/// Opens a viewer window for an established session.
pub fn open_viewer_window(
    viewer: ViewerHandle,
    events: mpsc::UnboundedReceiver<ViewerEvent>,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<ViewerView>)> {
    let title = format!("{} — dari", viewer.peer().name);
    let options = style::window_options(title, WINDOW_SIZE, Some(MIN_WINDOW_SIZE), cx);
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| ViewerView::new(viewer, events, window, cx))
    })
}

pub struct ViewerView {
    /// `None` once the session is over.
    session: Option<ViewerHandle>,
    peer_name: SharedString,
    image: Option<Arc<RenderImage>>,
    frame_size: Size<u32>,
    frames_shown: u64,
    /// Where the picture was last painted, for mapping the pointer.
    picture: Rc<Cell<Option<Bounds<Pixels>>>>,
    status: Option<HostStatus>,
    ended: Option<SessionEndReason>,
    awaiting_approval: bool,
    displays: Vec<DisplayDescription>,
    active_display: Option<u32>,
    quality: QualityPreset,
    focus: FocusHandle,
    modifiers: Modifiers,
    held_keys: Vec<KeyCode>,
    held_buttons: Vec<RemoteButton>,
    scroll: ScrollAccumulator,
    fps: f64,
    rtt: Duration,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ViewerView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerView")
            .field("peer", &self.peer_name)
            .finish_non_exhaustive()
    }
}

impl ViewerView {
    fn new(
        session: ViewerHandle,
        mut events: mpsc::UnboundedReceiver<ViewerEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);

        let mut frames = session.frames();
        let frame_task = cx.spawn_in(window, async move |this, cx| {
            while frames.changed().await.is_ok() {
                let frame = frames.borrow_and_update().clone();
                let Some(frame) = frame else { continue };
                let Ok(()) =
                    this.update_in(cx, |this, window, cx| this.show_frame(&frame, window, cx))
                else {
                    break;
                };
            }
        });
        let event_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                let Ok(()) = this.update(cx, |this, cx| this.on_event(event, cx)) else {
                    break;
                };
            }
        });
        let stats_task = cx.spawn(async move |this, cx| {
            let mut last = (0u64, Instant::now());
            loop {
                cx.background_executor().timer(STATS_INTERVAL).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    let Some(session) = &this.session else { return };
                    let frames = session.stats().frames_decoded.load(Ordering::Relaxed);
                    let elapsed = last.1.elapsed().as_secs_f64().max(0.001);
                    #[expect(clippy::cast_precision_loss, reason = "display only")]
                    let fps = (frames - last.0) as f64 / elapsed;
                    this.fps = fps;
                    this.rtt = session.rtt();
                    last = (frames, Instant::now());
                    cx.notify();
                }) else {
                    break;
                };
            }
        });
        // Keys held while the window loses focus would stay down on the host.
        let activation = cx.observe_window_activation(window, |this, window, _| {
            if !window.is_window_active() {
                this.release_all();
            }
        });

        Self {
            peer_name: session.peer().name.clone().into(),
            session: Some(session),
            image: None,
            frame_size: size(0, 0),
            frames_shown: 0,
            picture: Rc::new(Cell::new(None)),
            status: None,
            ended: None,
            awaiting_approval: false,
            displays: Vec::new(),
            active_display: None,
            quality: QualityPreset::Balanced,
            focus,
            modifiers: Modifiers::default(),
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            scroll: ScrollAccumulator::default(),
            fps: 0.,
            rtt: Duration::ZERO,
            _tasks: vec![frame_task, event_task, stats_task],
            _subscriptions: vec![activation, style::follow_appearance(window, cx)],
        }
    }

    fn show_frame(&mut self, frame: &DecodedFrame, window: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer) = RgbaImage::from_raw(frame.width, frame.height, frame.bgra.clone())
        else {
            return;
        };
        // `RenderImage` expects BGRA, which is what the decoder produces.
        let image = Arc::new(RenderImage::new(vec![Frame::new(buffer)]));
        if let Some(previous) = self.image.replace(image) {
            // Each frame is a new GPU texture; release the old one or the atlas grows forever.
            let _dropped = window.drop_image(previous);
        }
        self.frame_size = size(frame.width, frame.height);
        self.frames_shown += 1;
        cx.notify();
    }

    #[doc(hidden)]
    pub fn has_frame(&self) -> bool {
        self.image.is_some()
    }

    #[doc(hidden)]
    pub fn frames_shown(&self) -> u64 {
        self.frames_shown
    }

    #[doc(hidden)]
    pub fn has_ended(&self) -> bool {
        self.ended.is_some()
    }

    fn on_event(&mut self, event: ViewerEvent, cx: &mut Context<Self>) {
        match event {
            ViewerEvent::AwaitingApproval => self.awaiting_approval = true,
            ViewerEvent::HostStatus(status) => {
                self.awaiting_approval = false;
                self.status = Some(status);
            }
            ViewerEvent::Displays { displays, active } => {
                self.displays = displays;
                self.active_display = Some(active);
            }
            ViewerEvent::Ended(reason) => {
                self.ended = Some(reason);
                self.session = None;
            }
        }
        cx.notify();
    }

    fn send(&mut self, event: InputEvent) {
        if let Some(session) = &self.session {
            session.send_input(event);
        }
    }

    fn release_all(&mut self) {
        for key in std::mem::take(&mut self.held_keys) {
            self.send(InputEvent::Key {
                key,
                pressed: false,
            });
        }
        for event in modifier_changes(std::mem::take(&mut self.modifiers), Modifiers::default()) {
            self.send(event);
        }
        for button in std::mem::take(&mut self.held_buttons) {
            self.send(InputEvent::PointerButton {
                button,
                pressed: false,
            });
        }
    }

    fn position(&self, position: Point<Pixels>) -> Option<PointerPosition> {
        pointer_position(self.picture.get()?, position)
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent) {
        if let Some(position) = self.position(event.position) {
            self.send(InputEvent::PointerMove(position));
        }
    }

    fn on_mouse_button(&mut self, button: MouseButton, position: Point<Pixels>, pressed: bool) {
        let button = remote_button(button);
        if pressed {
            // Only presses on the picture count; releases always go through so nothing sticks.
            let Some(position) = self.position(position) else {
                return;
            };
            self.send(InputEvent::PointerMove(position));
            if !self.held_buttons.contains(&button) {
                self.held_buttons.push(button);
            }
        } else if let Some(index) = self.held_buttons.iter().position(|held| *held == button) {
            self.held_buttons.remove(index);
        } else {
            return;
        }
        self.send(InputEvent::PointerButton { button, pressed });
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &Window) {
        if self.position(event.position).is_none() {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height());
        if let Some((dx, dy)) = self.scroll.add(delta) {
            self.send(InputEvent::Scroll { dx, dy });
        }
    }

    fn on_modifiers(&mut self, modifiers: Modifiers) {
        for event in modifier_changes(self.modifiers, modifiers) {
            self.send(event);
        }
        self.modifiers = modifiers;
    }

    fn on_key(&mut self, keystroke: &Keystroke, pressed: bool) {
        // Keep the modifier state exact even if a modifier event was missed.
        self.on_modifiers(keystroke.modifiers);
        let Some(key) = key_code(keystroke) else {
            return;
        };
        if pressed {
            if !self.held_keys.contains(&key) {
                self.held_keys.push(key);
            }
        } else {
            self.held_keys.retain(|held| *held != key);
        }
        self.send(InputEvent::Key { key, pressed });
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.release_all();
        if let Some(session) = &self.session {
            session.disconnect();
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let live = self.session.is_some();
        let state = if !live {
            cx.theme().muted_foreground
        } else if self.awaiting_approval || self.status.is_none() {
            cx.theme().warning
        } else {
            cx.theme().success
        };
        // The toolbar is the window's title bar, so the macOS traffic lights sit at its left.
        let inset = if cfg!(target_os = "macos") {
            px(84.)
        } else {
            px(16.)
        };
        let controls = div()
            .h_flex()
            .w_full()
            .min_w_0()
            .pl(inset)
            .pr_3()
            .gap_3()
            .child(style::status_dot(state))
            .child(
                div()
                    .min_w_0()
                    .text_sm()
                    .font_semibold()
                    .truncate()
                    .child(self.peer_name.clone()),
            )
            .when(live, |toolbar| {
                toolbar.child(
                    div()
                        .h_flex()
                        .flex_none()
                        .gap_1p5()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(cx.theme().muted)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(Icon::new(AssetIcon::Gauge).xsmall())
                        .child(format!("{:.0} fps · {} ms", self.fps, self.rtt.as_millis())),
                )
            })
            .child(div().flex_1())
            .when(live, |toolbar| {
                toolbar.child(self.render_stream_controls(cx))
            })
            .when(live, |toolbar| {
                toolbar.child(style::no_drag(
                    div().child(
                        Button::new("disconnect")
                            .small()
                            .outline()
                            .icon(AssetIcon::Unplug)
                            .label(text().disconnect)
                            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
                    ),
                ))
            });
        div()
            .flex_none()
            .bg(style::content_surface(false, cx))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(style::title_bar().child(controls))
    }

    /// Display and quality switchers, once the host has said what it can stream.
    fn render_stream_controls(&self, cx: &mut Context<Self>) -> Div {
        div()
            .h_flex()
            .gap_3()
            .when(self.displays.len() > 1, |controls| {
                controls.child(style::segmented(
                    self.displays.iter().enumerate().map(|(index, display)| {
                        let id = display.id;
                        let detail: SharedString =
                            format!("{} · {}×{}", display.name, display.width, display.height)
                                .into();
                        style::segment(
                            format!("display-{id}"),
                            format!("{} {}", text().display, index + 1),
                            self.active_display == Some(id),
                            cx,
                        )
                        .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(session) = &this.session {
                                session.select_display(id);
                            }
                            cx.notify();
                        }))
                    }),
                    cx,
                ))
            })
            .when(self.status.is_some(), |controls| {
                controls.child(style::segmented(
                    [
                        (QualityPreset::Speed, text().quality_speed),
                        (QualityPreset::Balanced, text().quality_balanced),
                        (QualityPreset::Quality, text().quality_quality),
                    ]
                    .into_iter()
                    .map(|(preset, label)| {
                        style::segment(
                            format!("quality-{preset:?}"),
                            label,
                            self.quality == preset,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.quality = preset;
                            if let Some(session) = &this.session {
                                session.set_quality(preset);
                            }
                            cx.notify();
                        }))
                    }),
                    cx,
                ))
            })
    }

    /// What the remote screen says about the session: a card that stops it, or a notice pill.
    fn render_overlay(&self, cx: &App) -> Option<Div> {
        Some(match self.notice()? {
            Notice::Ended(reason) => centered().bg(gpui_kit::black().opacity(0.55)).child(
                overlay_card(AssetIcon::Unplug, cx.theme().muted_foreground, cx)
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .child(text().session_ended),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(reason),
                    )
                    .child(
                        Button::new("close-viewer")
                            .primary()
                            .w_full()
                            .label(text().close)
                            .on_click(|_, window, _| window.remove_window()),
                    ),
            ),
            Notice::AwaitingApproval => centered().child(
                overlay_card(AssetIcon::ShieldCheck, cx.theme().primary, cx)
                    .child(div().text_sm().child(text().waiting_for_approval))
                    .child(Spinner::new().color(cx.theme().primary)),
            ),
            Notice::Info(message) => div()
                .absolute()
                .top_3()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .max_w(px(640.))
                        .px_3()
                        .py_1p5()
                        .rounded_full()
                        .bg(gpui_kit::black().opacity(0.72))
                        .text_xs()
                        .text_color(gpui_kit::white())
                        .child(Icon::new(IconName::Info).xsmall())
                        .child(div().min_w_0().child(message)),
                ),
        })
    }

    /// A spinner until the first frame arrives.
    fn render_loading(&self) -> Option<Div> {
        if self.image.is_some() || self.ended.is_some() || self.awaiting_approval {
            return None;
        }
        let dimmed = gpui_kit::white().opacity(0.7);
        Some(
            centered().child(
                div()
                    .v_flex()
                    .items_center()
                    .gap_3()
                    .text_sm()
                    .text_color(dimmed)
                    .child(Spinner::new().color(dimmed))
                    .child(text().waiting_for_screen),
            ),
        )
    }

    fn notice(&self) -> Option<Notice> {
        if let Some(reason) = &self.ended {
            return Some(Notice::Ended(text().session_end_reason(reason)));
        }
        if self.awaiting_approval {
            return Some(Notice::AwaitingApproval);
        }
        let status = self.status?;
        match status.screen {
            Availability::PermissionDenied => {
                return Some(Notice::Info(text().remote_screen_permission));
            }
            Availability::Unavailable | Availability::NotAllowed => {
                return Some(Notice::Info(text().remote_screen_unavailable));
            }
            Availability::Available => {}
        }
        match status.input {
            Availability::Available => None,
            Availability::NotAllowed => Some(Notice::Info(text().view_only_session)),
            Availability::PermissionDenied | Availability::Unavailable => {
                Some(Notice::Info(text().remote_input_unavailable))
            }
        }
    }
}

/// What the viewer tells the user over the remote screen.
enum Notice {
    /// The session is over, and why.
    Ended(String),
    /// The host user has not answered the connection request yet.
    AwaitingApproval,
    /// The session runs with a limitation worth knowing.
    Info(&'static str),
}

/// A layer over the whole remote screen that centers its child.
fn centered() -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
}

/// A centered card over the remote screen, for states that stop the session.
fn overlay_card(icon: impl Into<Icon>, color: Hsla, cx: &App) -> Div {
    div()
        .v_flex()
        .items_center()
        .gap_3()
        .w(px(360.))
        .p_6()
        .rounded(cx.theme().radius_lg)
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_lg()
        .text_center()
        .child(style::icon_badge(icon, color, px(44.)))
}

fn remote_button(button: MouseButton) -> RemoteButton {
    match button {
        MouseButton::Left => RemoteButton::Left,
        MouseButton::Right => RemoteButton::Right,
        MouseButton::Middle => RemoteButton::Middle,
        MouseButton::Navigate(NavigationDirection::Back) => RemoteButton::Back,
        MouseButton::Navigate(NavigationDirection::Forward) => RemoteButton::Forward,
    }
}

impl Render for ViewerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(style::REM);
        let image = self.image.clone();
        let frame_size = self.frame_size;
        let picture = self.picture.clone();
        let screen = canvas(
            move |bounds, _, _| {
                let rect = letterbox(bounds, frame_size);
                picture.set(Some(rect));
                rect
            },
            move |_, rect, window, _| {
                if let Some(image) = image {
                    let _painted =
                        window.paint_image(rect, rect, Corners::default(), image, 0, false);
                }
            },
        )
        .size_full();

        let surface = div()
            .id("remote-screen")
            .key_context(REMOTE_SCREEN_CONTEXT)
            .relative()
            .flex_1()
            .min_h_0()
            .bg(gpui_kit::black())
            .track_focus(&self.focus)
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, _| this.on_mouse_move(event)),
            )
            .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.focus.focus(window, cx);
                this.on_mouse_button(event.button, event.position, true);
            }))
            .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, _, _| {
                this.on_mouse_button(event.button, event.position, false);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, _| {
                this.on_scroll(event, window);
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, _| {
                this.on_modifiers(event.modifiers);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.on_key(&event.keystroke, true);
                cx.stop_propagation();
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                this.on_key(&event.keystroke, false);
                cx.stop_propagation();
            }))
            .child(screen)
            .children(self.render_loading())
            .children(self.render_overlay(cx));

        div()
            .v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .child(surface)
    }
}
