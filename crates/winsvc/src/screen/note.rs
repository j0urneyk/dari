//! What the screen thread tells the event log.
use std::fmt;
use std::time::Duration;

use dari_proto::InputDesktop;

use super::{GIVE_UP_LIMIT, Size};
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
    /// Sums up the attempts of a streak whose notes the log left out.
    Retried {
        failures: u32,
        over: Duration,
        last: Failure,
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
    Lost(Lost),
}

/// Why a duplication ended by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lost {
    Failed(Hresult),
    NoImage,
}

/// How an attempt to show the display failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The thread couldn't attach to the input desktop, or attached to another one.
    Unattached,
    /// Attaching or `DuplicateOutput` failed.
    CannotDuplicate(Hresult),
    Lost(Lost),
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
    StillFailing(Failure),
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
                    Ended::Lost(lost) => lost.to_string(),
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
            Self::Retried {
                failures,
                over,
                last,
            } => write!(
                f,
                "{failures} attempts in {over:?} showed no image, the last because {last}"
            ),
            Self::Unavailable { display, why } => {
                write!(f, "display {display} is unavailable: ")?;
                match why {
                    Unavailable::NoSuchDisplay => f.write_str("no output matches it"),
                    Unavailable::Unsupported(code) => {
                        write!(f, "DuplicateOutput failed with {code}")
                    }
                    Unavailable::StillFailing(last) => write!(
                        f,
                        "no image for {GIVE_UP_LIMIT:?}, the last attempt failed because {last}"
                    ),
                    Unavailable::TooLarge(size) => write!(f, "{size} is too large"),
                }
            }
            Self::BadPointerShape { kind } => write!(f, "ignored a pointer shape of type {kind}"),
        }
    }
}

impl fmt::Display for Lost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(code) => write!(f, "DXGI failed with {code}"),
            Self::NoImage => f.write_str("no image in time"),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unattached => f.write_str("the desktop couldn't be attached"),
            Self::CannotDuplicate(code) => write!(f, "duplicating failed with {code}"),
            Self::Lost(lost) => write!(f, "the duplication ended: {lost}"),
        }
    }
}
