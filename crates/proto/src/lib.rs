//! Wire protocol shared by Dari hosts and viewers.
//!
//! This crate defines every message exchanged over a session, the protocol version rules,
//! and a length-bounded framing codec. It performs no I/O of its own.

mod audio;
mod codec;
mod control;
mod handshake;
mod input;
mod relay;
mod stream;
mod transfer;
mod validate;
mod version;
mod video;

pub use audio::{AudioPacket, MAX_AUDIO_PACKET_BYTES};
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
pub use relay::{
    Allocation, DEFAULT_RELAY_PORT, DeviceId, RELAY_ACK_MAGIC, RELAY_ALPN, RELAY_BIND_MAGIC,
    RELAY_TOKEN_LEN, RelayError, RelayRequest, RelayResponse,
};
pub use stream::StreamKind;
pub use transfer::{
    FileOffer, MAX_FILE_NAME_BYTES, TransferEnd, TransferId, sanitize_file_name, split_extension,
};
pub use validate::{MAX_DEVICE_NAME_CHARS, Validate, ValidationError, sanitize_display_text};
pub use version::{PROTOCOL_VERSION, ProtocolVersion};
pub use video::VideoPacket;
