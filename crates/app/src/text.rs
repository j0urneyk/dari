//! User-facing strings in Korean and English, chosen in the settings or from the system locale.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use dari_net::PasswordError;
use dari_proto::RejectReason;
use dari_session::SessionEndReason;

use crate::settings::LanguagePreference;

pub(crate) struct Text {
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
    pub(crate) frame_rate_auto: &'static str,
    pub(crate) frame_rate_title: &'static str,
    pub(crate) my_id: &'static str,
    pub(crate) relay_server: &'static str,
    pub(crate) relay_placeholder: &'static str,
    pub(crate) relay_connecting: &'static str,
    pub(crate) relay_required: &'static str,
    pub(crate) connect_subtitle: &'static str,
    pub(crate) hosting_on: &'static str,
    pub(crate) hosting_off: &'static str,
    pub(crate) not_accepting_hint: &'static str,
    pub(crate) other_addresses: &'static str,
    pub(crate) nearby_empty: &'static str,
    pub(crate) approval_hint: &'static str,
    pub(crate) background_unreadable: &'static str,
    pub(crate) settings_title: &'static str,
    pub(crate) settings_subtitle: &'static str,
    pub(crate) appearance: &'static str,
    pub(crate) theme: &'static str,
    pub(crate) theme_system: &'static str,
    pub(crate) theme_light: &'static str,
    pub(crate) theme_dark: &'static str,
    pub(crate) translucent_window: &'static str,
    pub(crate) translucent_window_hint: &'static str,
    pub(crate) background_picture: &'static str,
    pub(crate) background_none: &'static str,
    pub(crate) background_choose: &'static str,
    pub(crate) background_remove: &'static str,
    pub(crate) background_blur: &'static str,
    pub(crate) sharing_settings: &'static str,
    pub(crate) file_transfer: &'static str,
    pub(crate) send_file: &'static str,
    pub(crate) save_file: &'static str,
    pub(crate) cancel: &'static str,
    pub(crate) show_in_folder: &'static str,
    pub(crate) clear_finished: &'static str,
    pub(crate) incoming_file_prompt: &'static str,
    pub(crate) waiting_for_answer: &'static str,
    pub(crate) sending: &'static str,
    pub(crate) receiving: &'static str,
    pub(crate) sent: &'static str,
    pub(crate) received: &'static str,
    pub(crate) transfer_declined: &'static str,
    pub(crate) transfer_cancelled: &'static str,
    pub(crate) transfer_failed: &'static str,
    pub(crate) drop_to_send: &'static str,
    pub(crate) share_audio: &'static str,
    pub(crate) sound_on: &'static str,
    pub(crate) sound_off: &'static str,
    pub(crate) toggle_sound: &'static str,
    pub(crate) sound_not_allowed: &'static str,
    pub(crate) sound_awaiting_permission: &'static str,
    pub(crate) remote_sound_awaiting_permission: &'static str,
    pub(crate) remote_sound_permission: &'static str,
    pub(crate) language: &'static str,
    pub(crate) language_system: &'static str,
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
            format!("{name}이(가) 이 기기에 접속하려고 합니다.")
        } else {
            format!("{name} wants to connect to this device.")
        }
    }

    pub(crate) fn relay_unavailable(&self, detail: &str) -> String {
        if self.korean {
            format!("릴레이에 연결할 수 없습니다 ({detail}). 다시 시도하는 중…")
        } else {
            format!("Cannot reach the relay ({detail}). Retrying…")
        }
    }

    pub(crate) fn folder_files(&self, files: usize) -> String {
        match (self.korean, files) {
            (true, _) => format!("폴더 · 파일 {files}개"),
            (false, 1) => "folder · 1 file".into(),
            (false, _) => format!("folder · {files} files"),
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
    frame_rate_auto: "자동",
    frame_rate_title: "최대 프레임 레이트",
    my_id: "내 ID",
    relay_server: "릴레이 서버",
    relay_placeholder: "다른 네트워크에서 접속하려면 릴레이 주소 입력 (선택)",
    relay_connecting: "릴레이에 연결하는 중…",
    relay_required: "ID로 접속하려면 먼저 릴레이 서버를 설정하세요",
    connect_subtitle: "상대 기기에 표시된 주소와 비밀번호로 접속합니다",
    hosting_on: "접속 가능",
    hosting_off: "원격 접속 꺼짐",
    not_accepting_hint: "켜면 이 기기의 접속 주소와 일회용 비밀번호가 표시됩니다.",
    other_addresses: "다른 주소",
    nearby_empty: "같은 네트워크에서 Dari를 켠 기기가 여기에 나타납니다",
    approval_hint: "30초 안에 응답하지 않으면 자동으로 거부됩니다.",
    background_unreadable: "이미지를 열 수 없습니다",
    settings_title: "설정",
    settings_subtitle: "Dari의 모양과 언어를 바꿉니다",
    appearance: "모양",
    theme: "테마",
    theme_system: "시스템",
    theme_light: "라이트",
    theme_dark: "다크",
    translucent_window: "창 투명 효과",
    translucent_window_hint: "창 뒤가 흐릿하게 비쳐 보입니다. 배경 사진이 있으면 사진 너머로 비칩니다.",
    background_picture: "배경 사진",
    background_none: "없음",
    background_choose: "사진 선택…",
    background_remove: "제거",
    background_blur: "배경 흐리게",
    sharing_settings: "공유 설정",
    file_transfer: "파일 전송",
    send_file: "파일 보내기…",
    save_file: "저장",
    cancel: "취소",
    show_in_folder: "폴더에서 보기",
    clear_finished: "완료 항목 지우기",
    incoming_file_prompt: "상대 기기가 보내려고 합니다. 다운로드 폴더에 저장할까요?",
    waiting_for_answer: "상대방의 응답을 기다리는 중",
    sending: "보내는 중",
    receiving: "받는 중",
    sent: "보냄",
    received: "받음",
    transfer_declined: "거부됨",
    transfer_cancelled: "취소됨",
    transfer_failed: "실패",
    drop_to_send: "놓으면 상대 기기로 파일을 보냅니다",
    share_audio: "소리 공유",
    sound_on: "소리 켜짐",
    sound_off: "소리 꺼짐",
    toggle_sound: "원격 기기의 소리 켜기/끄기",
    sound_not_allowed: "소리 권한 없음",
    sound_awaiting_permission: "소리 허용 대기 중",
    remote_sound_awaiting_permission: "원격 Mac이 사용자에게 시스템 오디오 녹음을 허용할지 묻고 있습니다. 그 Mac에서 허용하면 소리가 들립니다.",
    remote_sound_permission: "원격 Mac이 시스템 오디오 녹음을 허용하지 않았습니다. 그 Mac의 개인정보 보호 및 보안 → 화면 및 시스템 오디오 녹음에서 Dari를 허용해야 합니다.",
    language: "언어",
    language_system: "시스템",
    korean: true,
};

static ENGLISH: Text = Text {
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
    frame_rate_auto: "Auto",
    frame_rate_title: "Maximum frame rate",
    my_id: "My ID",
    relay_server: "Relay server",
    relay_placeholder: "Relay address for connecting across networks (optional)",
    relay_connecting: "Connecting to the relay…",
    relay_required: "Set a relay server to connect by ID",
    connect_subtitle: "Use the address and password shown on the other device",
    hosting_on: "Accepting connections",
    hosting_off: "Remote access off",
    not_accepting_hint: "Turn it on to show this device's address and one-time password.",
    other_addresses: "Other addresses",
    nearby_empty: "Devices running Dari on this network show up here",
    approval_hint: "Declined automatically after 30 seconds without an answer.",
    background_unreadable: "Can't open this picture",
    settings_title: "Settings",
    settings_subtitle: "Change how Dari looks and which language it uses",
    appearance: "Appearance",
    theme: "Theme",
    theme_system: "System",
    theme_light: "Light",
    theme_dark: "Dark",
    translucent_window: "Translucent window",
    translucent_window_hint: "What is behind the window shows through, blurred, through the background picture too.",
    background_picture: "Background picture",
    background_none: "None",
    background_choose: "Choose…",
    background_remove: "Remove",
    background_blur: "Blur the picture",
    sharing_settings: "Sharing",
    file_transfer: "Exchange files",
    send_file: "Send file…",
    save_file: "Save",
    cancel: "Cancel",
    show_in_folder: "Show in folder",
    clear_finished: "Clear finished",
    incoming_file_prompt: "The remote device wants to send this. Save it to Downloads?",
    waiting_for_answer: "Waiting for the other side",
    sending: "Sending",
    receiving: "Receiving",
    sent: "Sent",
    received: "Received",
    transfer_declined: "Declined",
    transfer_cancelled: "Cancelled",
    transfer_failed: "Failed",
    drop_to_send: "Drop to send to the remote device",
    share_audio: "Share sound",
    sound_on: "Sound on",
    sound_off: "Sound off",
    toggle_sound: "Play or mute the remote device's sound",
    sound_not_allowed: "No sound permission",
    sound_awaiting_permission: "Waiting for sound permission",
    remote_sound_awaiting_permission: "The remote Mac is asking its user whether Dari may record its sound. Sound starts once they allow it there.",
    remote_sound_permission: "The remote Mac has not allowed Dari to record system audio. Allow it on that Mac under Privacy & Security → Screen & System Audio Recording.",
    language: "Language",
    language_system: "System",
    korean: false,
};

/// Each language's name in that language, for the language choice.
pub(crate) const KOREAN_NAME: &str = "한국어";
pub(crate) const ENGLISH_NAME: &str = "English";

const FOLLOW_SYSTEM: u8 = 0;
const USE_KOREAN: u8 = 1;
const USE_ENGLISH: u8 = 2;

/// The language [`text`] answers in, as last set by [`set_language`].
static LANGUAGE: AtomicU8 = AtomicU8::new(FOLLOW_SYSTEM);

/// Switches every later [`text`] call to `preference`. Windows show it on their next render.
pub(crate) fn set_language(preference: LanguagePreference) {
    let value = match preference {
        LanguagePreference::System => FOLLOW_SYSTEM,
        LanguagePreference::Korean => USE_KOREAN,
        LanguagePreference::English => USE_ENGLISH,
    };
    LANGUAGE.store(value, Ordering::Relaxed);
}

fn system_is_korean() -> bool {
    static KOREAN_SYSTEM: OnceLock<bool> = OnceLock::new();
    *KOREAN_SYSTEM.get_or_init(|| {
        sys_locale::get_locale().is_some_and(|locale| locale.to_ascii_lowercase().starts_with("ko"))
    })
}

/// Strings for the language chosen in the settings, or the system's.
pub(crate) fn text() -> &'static Text {
    let korean = match LANGUAGE.load(Ordering::Relaxed) {
        USE_KOREAN => true,
        USE_ENGLISH => false,
        _ => system_is_korean(),
    };
    if korean { &KOREAN } else { &ENGLISH }
}
