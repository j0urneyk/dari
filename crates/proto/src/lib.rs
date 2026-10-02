//! Wire protocol shared by open-desk hosts and viewers.
//!
//! This crate defines every message exchanged over a session, the protocol version rules,
//! and a length-bounded framing codec. It performs no I/O of its own.

mod codec;
mod control;
mod handshake;
mod input;
mod validate;
mod version;
mod video;

pub use codec::{
    CONTROL_FRAME_LIMIT, CodecError, HANDSHAKE_FRAME_LIMIT, MessageCodec, VIDEO_FRAME_LIMIT,
};
pub use control::{
    Availability, ControlMessage, DisplayDescription, HostStatus, MAX_CLIPBOARD_BYTES,
    MAX_DISPLAYS, QualityPreset,
};
pub use handshake::{
    AuthOutcome, ClientHello, HandshakeMessage, KEY_CONFIRMATION_LEN, Os, RejectReason, ServerHello,
};
pub use input::{
    InputEvent, KeyCode, MAX_FUNCTION_KEY, MAX_INPUT_TEXT_CHARS, MAX_SCROLL_LINES, MouseButton,
    NamedKey, PointerPosition,
};
pub use validate::{MAX_DEVICE_NAME_CHARS, Validate, ValidationError, sanitize_display_text};
pub use version::{PROTOCOL_VERSION, ProtocolVersion};
pub use video::VideoPacket;
