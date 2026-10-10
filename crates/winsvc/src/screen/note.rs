//! What the screen thread tells the event log.
use std::fmt;
use std::time::Duration;

use dari_proto::InputDesktop;

use super::{DENIED_LIMIT, NO_IMAGE_LIMIT, Size};
use crate::dxgi_result::Hresult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Note {
    Duplicated {
        display: u32,
        desktop: InputDesktop,
        size: Size,
    },
    FirstImage {
        accumulated_frames: u32,
        last_present_time: i64,
        skipped: u32,
        after: Duration,
    },
    Ended {
        why: Ended,
        stats: Stats,
        lasted: Duration,
    },
    CannotDuplicate {
        display: u32,
        code: Hresult,
    },
    Unavailable {
        display: u32,
        why: Unavailable,
    },
    BadPointerShape {
        kind: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    DesktopChanged,
    DisplaySelected,
    Failed(Hresult),
    NoImage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub(crate) images: u32,
    pub(crate) pointer_only: u32,
    pub(crate) offered: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    NoSuchDisplay,
    Unsupported(Hresult),
    StillFailing(Option<Hresult>),
    TooLarge(Size),
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicated {
                display,
                desktop,
                size,
            } => write!(f, "duplicating display {display} on {desktop}, {size}"),
            Self::FirstImage {
                accumulated_frames,
                last_present_time,
                skipped,
                after,
            } => write!(
                f,
                "first image after {after:?}: AccumulatedFrames {accumulated_frames}, \
                 LastPresentTime {last_present_time}, {skipped} pointer-only frames skipped"
            ),
            Self::Ended { why, stats, lasted } => {
                let why = match why {
                    Ended::DesktopChanged => "the desktop changed".to_owned(),
                    Ended::DisplaySelected => "another display was selected".to_owned(),
                    Ended::Failed(code) => format!("DXGI failed with {code}"),
                    Ended::NoImage => format!("no image within {NO_IMAGE_LIMIT:?}"),
                };
                write!(
                    f,
                    "duplication ended after {lasted:?}, {why}: {} images, {} pointer-only \
                     frames, {} offered to the app",
                    stats.images, stats.pointer_only, stats.offered
                )
            }
            Self::CannotDuplicate { display, code } => {
                write!(f, "cannot duplicate display {display} yet: {code}")
            }
            Self::Unavailable { display, why } => {
                write!(f, "display {display} is unavailable: ")?;
                match why {
                    Unavailable::NoSuchDisplay => f.write_str("no output matches it"),
                    Unavailable::Unsupported(code) => {
                        write!(f, "DuplicateOutput failed with {code}")
                    }
                    Unavailable::StillFailing(Some(code)) => {
                        write!(f, "still failing after {DENIED_LIMIT:?} with {code}")
                    }
                    Unavailable::StillFailing(None) => {
                        write!(
                            f,
                            "the desktop still can't be attached after {DENIED_LIMIT:?}"
                        )
                    }
                    Unavailable::TooLarge(size) => write!(f, "{size} is too large"),
                }
            }
            Self::BadPointerShape { kind } => write!(f, "ignored a pointer shape of type {kind}"),
        }
    }
}
