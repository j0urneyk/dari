//! User-facing strings in Korean and English, chosen from the system locale.

use std::sync::OnceLock;

use dari_net::PasswordError;
use dari_proto::RejectReason;
use dari_session::SessionEndReason;

pub(crate) struct Text {
    pub(crate) app_subtitle: &'static str,
    pub(crate) this_device: &'static str,
    pub(crate) allow_remote_access: &'static str,
    pub(crate) addresses: &'static str,
    pub(crate) no_addresses: &'static str,
    pub(crate) password: &'static str,
    pub(crate) new_password: &'static str,
    pub(crate) show_password: &'static str,
    pub(crate) hide_password: &'static str,
    pub(crate) not_accepting: &'static str,
    pub(crate) hosting_failed: &'static str,
    pub(crate) waiting_for_viewer: &'static str,
    pub(crate) end_session: &'static str,
    pub(crate) screen_permission_missing: &'static str,
    pub(crate) input_permission_missing: &'static str,
    pub(crate) request_permission: &'static str,
    pub(crate) open_settings: &'static str,
    pub(crate) control_remote_device: &'static str,
    pub(crate) address: &'static str,
    pub(crate) address_placeholder: &'static str,
    pub(crate) password_placeholder: &'static str,
    pub(crate) connect: &'static str,
    pub(crate) connecting: &'static str,
    pub(crate) recent: &'static str,
    pub(crate) map_shortcut_modifier: &'static str,
    pub(crate) disconnect: &'static str,
    pub(crate) close: &'static str,
    pub(crate) waiting_for_screen: &'static str,
    pub(crate) remote_screen_unavailable: &'static str,
    pub(crate) remote_screen_permission: &'static str,
    pub(crate) remote_input_unavailable: &'static str,
    pub(crate) session_ended: &'static str,
    pub(crate) approval_title: &'static str,
    pub(crate) allow_control: &'static str,
    pub(crate) allow_view_only: &'static str,
    pub(crate) decline: &'static str,
    pub(crate) require_approval: &'static str,
    pub(crate) clipboard_sync: &'static str,
    pub(crate) lan_discovery: &'static str,
    pub(crate) nearby_devices: &'static str,
    pub(crate) waiting_for_approval: &'static str,
    pub(crate) view_only_session: &'static str,
    pub(crate) display: &'static str,
    pub(crate) quality_speed: &'static str,
    pub(crate) quality_balanced: &'static str,
    pub(crate) quality_quality: &'static str,
    pub(crate) my_id: &'static str,
    pub(crate) relay_server: &'static str,
    pub(crate) relay_placeholder: &'static str,
    pub(crate) relay_connecting: &'static str,
    pub(crate) relay_required: &'static str,
    korean: bool,
}

impl Text {
    pub(crate) fn viewer_connected(&self, name: &str) -> String {
        if self.korean {
            format!("{name}이(가) 이 기기에 접속해 있습니다")
        } else {
            format!("{name} is connected to this device")
        }
    }

    pub(crate) fn approval_prompt(&self, name: &str) -> String {
        if self.korean {
            format!(
                "{name}이(가) 이 기기에 접속하려고 합니다. 30초 안에 응답하지 않으면 거부됩니다."
            )
        } else {
            format!("{name} wants to connect to this device. It is declined after 30 seconds.")
        }
    }

    pub(crate) fn relay_unavailable(&self, detail: &str) -> String {
        if self.korean {
            format!("릴레이에 연결할 수 없습니다 ({detail}). 다시 시도하는 중…")
        } else {
            format!("Cannot reach the relay ({detail}). Retrying…")
        }
    }

    pub(crate) fn connect_failed(&self, detail: &str) -> String {
        if self.korean {
            format!("접속하지 못했습니다: {detail}")
        } else {
            format!("Could not connect: {detail}")
        }
    }

    pub(crate) fn rejection(&self, reason: RejectReason) -> &'static str {
        match (reason, self.korean) {
            (RejectReason::AuthenticationFailed, true) => "비밀번호가 맞지 않습니다",
            (RejectReason::AuthenticationFailed, false) => "The password is not correct",
            (RejectReason::Busy, true) => "상대 기기가 이미 다른 세션에 연결되어 있습니다",
            (RejectReason::Busy, false) => "The remote device is already in a session",
            (RejectReason::TooManyAttempts, true) => {
                "실패한 시도가 너무 많습니다. 잠시 후 다시 시도하세요"
            }
            (RejectReason::TooManyAttempts, false) => "Too many failed attempts. Try again later",
            (RejectReason::NotAccepting, true) => "상대 기기가 원격 접속을 허용하지 않고 있습니다",
            (RejectReason::NotAccepting, false) => "The remote device is not accepting connections",
            (RejectReason::IncompatibleVersion, true) => {
                "상대 기기의 Dari 버전이 호환되지 않습니다"
            }
            (RejectReason::IncompatibleVersion, false) => {
                "The remote device runs an incompatible Dari version"
            }
        }
    }

    pub(crate) fn password_error(&self, error: &PasswordError) -> &'static str {
        match (error, self.korean) {
            (PasswordError::WrongLength, true) => "비밀번호는 10자입니다 (예: K7MXQ-3PTWA)",
            (PasswordError::WrongLength, false) => {
                "The password has 10 characters (e.g. K7MXQ-3PTWA)"
            }
            (PasswordError::InvalidCharacter, true) => {
                "비밀번호에 쓰이지 않는 문자가 있습니다. 상대 기기에 표시된 비밀번호를 확인하세요"
            }
            (PasswordError::InvalidCharacter, false) => {
                "The password contains a character never used in passwords. Check the remote device"
            }
            (PasswordError::Random, true) => "비밀번호를 처리하지 못했습니다",
            (PasswordError::Random, false) => "The password could not be processed",
        }
    }

    pub(crate) fn session_end_reason(&self, reason: &SessionEndReason) -> String {
        match (reason, self.korean) {
            (SessionEndReason::ViewerLeft, true) => "연결을 끊었습니다".into(),
            (SessionEndReason::ViewerLeft, false) => "You disconnected".into(),
            (SessionEndReason::HostEnded, true) => "상대 기기가 세션을 종료했습니다".into(),
            (SessionEndReason::HostEnded, false) => "The remote device ended the session".into(),
            (SessionEndReason::Declined, true) => "상대방이 접속을 거부했습니다".into(),
            (SessionEndReason::Declined, false) => "The remote side declined the connection".into(),
            (SessionEndReason::ConnectionLost(detail), true) => {
                format!("연결이 끊어졌습니다 ({detail})")
            }
            (SessionEndReason::ConnectionLost(detail), false) => {
                format!("Connection lost ({detail})")
            }
            (SessionEndReason::ProtocolError(detail), true) => format!("통신 오류 ({detail})"),
            (SessionEndReason::ProtocolError(detail), false) => {
                format!("Protocol error ({detail})")
            }
        }
    }
}

static KOREAN: Text = Text {
    app_subtitle: "macOS와 Windows를 위한 원격 데스크톱",
    this_device: "이 기기",
    allow_remote_access: "원격 접속 허용",
    addresses: "접속 주소",
    no_addresses: "네트워크에 연결되어 있지 않습니다",
    password: "일회용 비밀번호",
    new_password: "새 비밀번호",
    show_password: "비밀번호 보기",
    hide_password: "비밀번호 숨기기",
    not_accepting: "원격 접속이 꺼져 있습니다",
    hosting_failed: "원격 접속을 시작하지 못했습니다",
    waiting_for_viewer: "접속을 기다리는 중",
    end_session: "연결 끊기",
    screen_permission_missing: "화면 기록 권한이 없어 상대방이 이 화면을 볼 수 없습니다.",
    input_permission_missing: "손쉬운 사용 권한이 없어 상대방이 이 기기를 제어할 수 없습니다.",
    request_permission: "권한 요청",
    open_settings: "시스템 설정 열기",
    control_remote_device: "원격 기기 제어",
    address: "주소",
    address_placeholder: "IP 주소, IP:포트 또는 9자리 ID",
    password_placeholder: "상대 기기에 표시된 비밀번호",
    connect: "접속",
    connecting: "접속 중…",
    recent: "최근 접속",
    map_shortcut_modifier: "⌘와 Ctrl 단축키 변환",
    disconnect: "연결 끊기",
    close: "닫기",
    waiting_for_screen: "화면을 기다리는 중…",
    remote_screen_unavailable: "원격 기기의 화면을 가져올 수 없습니다.",
    remote_screen_permission: "원격 기기에 화면 기록 권한이 없습니다. 상대 기기에서 권한을 허용해야 합니다.",
    remote_input_unavailable: "원격 기기를 제어할 수 없습니다(손쉬운 사용 권한 필요). 화면 보기만 가능합니다.",
    session_ended: "세션이 종료되었습니다",
    approval_title: "접속 요청",
    allow_control: "제어 허용",
    allow_view_only: "보기만 허용",
    decline: "거부",
    require_approval: "접속할 때마다 승인 요청",
    clipboard_sync: "클립보드 공유",
    lan_discovery: "같은 네트워크에 이 기기 표시",
    nearby_devices: "근처 기기",
    waiting_for_approval: "상대방이 접속을 허용하기를 기다리는 중…",
    view_only_session: "보기 전용 세션입니다. 상대방이 제어를 허용하지 않았습니다.",
    display: "화면",
    quality_speed: "속도",
    quality_balanced: "균형",
    quality_quality: "화질",
    my_id: "내 ID",
    relay_server: "릴레이 서버",
    relay_placeholder: "다른 네트워크에서 접속하려면 릴레이 주소 입력 (선택)",
    relay_connecting: "릴레이에 연결하는 중…",
    relay_required: "ID로 접속하려면 먼저 릴레이 서버를 설정하세요",
    korean: true,
};

static ENGLISH: Text = Text {
    app_subtitle: "Remote desktop for macOS and Windows",
    this_device: "This device",
    allow_remote_access: "Allow remote access",
    addresses: "Addresses",
    no_addresses: "Not connected to a network",
    password: "One-time password",
    new_password: "New password",
    show_password: "Show password",
    hide_password: "Hide password",
    not_accepting: "Remote access is off",
    hosting_failed: "Could not start remote access",
    waiting_for_viewer: "Waiting for a connection",
    end_session: "Disconnect",
    screen_permission_missing: "Screen Recording permission is missing, so the other side cannot see this screen.",
    input_permission_missing: "Accessibility permission is missing, so the other side cannot control this device.",
    request_permission: "Request permission",
    open_settings: "Open System Settings",
    control_remote_device: "Control a remote device",
    address: "Address",
    address_placeholder: "IP address, IP:port, or 9-digit ID",
    password_placeholder: "Password shown on the remote device",
    connect: "Connect",
    connecting: "Connecting…",
    recent: "Recent",
    map_shortcut_modifier: "Translate ⌘ and Ctrl shortcuts",
    disconnect: "Disconnect",
    close: "Close",
    waiting_for_screen: "Waiting for the screen…",
    remote_screen_unavailable: "The remote screen is unavailable.",
    remote_screen_permission: "The remote device has not granted Screen Recording permission.",
    remote_input_unavailable: "The remote device cannot be controlled (Accessibility permission needed). View only.",
    session_ended: "Session ended",
    approval_title: "Connection request",
    allow_control: "Allow control",
    allow_view_only: "View only",
    decline: "Decline",
    require_approval: "Ask before each connection",
    clipboard_sync: "Share clipboard",
    lan_discovery: "Show this device on the local network",
    nearby_devices: "Nearby devices",
    waiting_for_approval: "Waiting for the remote side to allow the connection…",
    view_only_session: "View-only session: the remote side did not allow control.",
    display: "Display",
    quality_speed: "Speed",
    quality_balanced: "Balanced",
    quality_quality: "Quality",
    my_id: "My ID",
    relay_server: "Relay server",
    relay_placeholder: "Relay address for connecting across networks (optional)",
    relay_connecting: "Connecting to the relay…",
    relay_required: "Set a relay server to connect by ID",
    korean: false,
};

/// Strings for the system language.
pub(crate) fn text() -> &'static Text {
    static CHOICE: OnceLock<bool> = OnceLock::new();
    let korean = *CHOICE.get_or_init(|| {
        sys_locale::get_locale().is_some_and(|locale| locale.to_ascii_lowercase().starts_with("ko"))
    });
    if korean { &KOREAN } else { &ENGLISH }
}
