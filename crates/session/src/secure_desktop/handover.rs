use std::io;
use std::sync::Arc;

use dari_media::RgbaFrame;
use dari_proto::{AppToHelper, FrameLayout, FrameSlot, HelperToApp};
use tracing::warn;

use super::LinkDriver;

pub(crate) trait SectionMapper {
    type Section: MappedSection;

    /// Maps the section `handle` names, laid out as `layout`. The handle is closed either way.
    fn map(&mut self, handle: u64, layout: FrameLayout) -> io::Result<Self::Section>;
}

/// A read-only view of one frame section, unmapped on drop.
pub(crate) trait MappedSection: Send + Sync + 'static {
    /// Copies `slot` if its sequence word reads `sequence` before and after the copy. `None`
    /// means the helper wrote a slot the app owned.
    fn copy(&self, slot: FrameSlot, sequence: u64) -> Option<RgbaFrame>;
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HandoverError {
    #[error("the helper sent a frame before a frame section")]
    FrameBeforeSection,
    #[error("the helper's frame section is unusable: {0}")]
    BadSection(String),
    #[error("copying a frame failed: {0}")]
    Copy(#[from] tokio::task::JoinError),
}

pub(crate) struct Handover<M: SectionMapper> {
    mapper: M,
    section: Option<Arc<M::Section>>,
}

impl<M: SectionMapper> Handover<M> {
    pub(crate) fn new(mapper: M) -> Self {
        Self {
            mapper,
            section: None,
        }
    }

    pub(crate) async fn handle(
        &mut self,
        message: HelperToApp,
        driver: &LinkDriver,
    ) -> Result<Option<AppToHelper>, HandoverError> {
        match message {
            HelperToApp::DesktopChanged(desktop) => {
                driver.desktop_changed(desktop);
                Ok(None)
            }
            HelperToApp::ScreenUnavailable { display } => {
                driver.screen_unavailable(display);
                Ok(None)
            }
            HelperToApp::FrameSection {
                handle,
                width,
                height,
            } => {
                let layout = FrameLayout::new(width, height)
                    .map_err(|error| HandoverError::BadSection(error.to_string()))?;
                let section = self
                    .mapper
                    .map(handle, layout)
                    .map_err(|error| HandoverError::BadSection(error.to_string()))?;
                // The old view is unmapped here, before the helper hears it may close it.
                let replaced = self.section.replace(Arc::new(section)).is_some();
                Ok(replaced.then_some(AppToHelper::SectionReleased))
            }
            HelperToApp::Frame {
                display,
                slot,
                sequence,
            } => {
                let section = self
                    .section
                    .clone()
                    .ok_or(HandoverError::FrameBeforeSection)?;
                if driver.wants(display) {
                    // A 4K frame is 33 MB; copying it would hold up an async worker.
                    let copied =
                        tokio::task::spawn_blocking(move || section.copy(slot, sequence)).await?;
                    if let Some(image) = copied {
                        driver.frame(display, image);
                    } else {
                        warn!(?slot, sequence, "the helper wrote a frame the app owned");
                    }
                }
                // Kept, dropped, or torn: the slot goes back to the helper.
                Ok(Some(AppToHelper::RequestFrame))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests may panic")]

    use std::sync::Mutex;
    use std::time::Duration;

    use dari_proto::InputDesktop;

    use super::*;
    use crate::secure_desktop::{SecureDesktopLink, SecureDesktopView, Wait};

    const DISPLAY: u32 = 7;

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn push(&self, entry: String) {
            self.0.lock().unwrap().push(entry);
        }
        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[derive(Default)]
    struct FakeMapper {
        log: Log,
        refuse: bool,
        torn: bool,
    }

    struct FakeSection {
        handle: u64,
        log: Log,
        torn: bool,
    }

    impl SectionMapper for FakeMapper {
        type Section = FakeSection;

        fn map(&mut self, handle: u64, layout: FrameLayout) -> io::Result<FakeSection> {
            if self.refuse {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.log.push(format!(
                "map {handle} {}x{}",
                layout.width(),
                layout.height()
            ));
            Ok(FakeSection {
                handle,
                log: self.log.clone(),
                torn: self.torn,
            })
        }
    }

    impl MappedSection for FakeSection {
        fn copy(&self, slot: FrameSlot, sequence: u64) -> Option<RgbaFrame> {
            self.log
                .push(format!("copy {} {slot:?} {sequence}", self.handle));
            let red = u8::try_from(sequence).unwrap();
            (!self.torn).then(|| RgbaFrame::new(1, 1, vec![red, 0, 0, 255]).unwrap())
        }
    }

    impl Drop for FakeSection {
        fn drop(&mut self) {
            self.log.push(format!("unmap {}", self.handle));
        }
    }

    struct Setup {
        handover: Handover<FakeMapper>,
        log: Log,
        driver: LinkDriver,
        view: SecureDesktopView,
        _link: SecureDesktopLink,
    }

    fn setup(mapper: FakeMapper) -> Setup {
        let (link, driver) = SecureDesktopLink::pair();
        link.select_display(DISPLAY);
        driver.desktop_changed(InputDesktop::Winlogon);
        Setup {
            log: mapper.log.clone(),
            handover: Handover::new(mapper),
            driver,
            view: link.view(),
            _link: link,
        }
    }

    fn section(handle: u64) -> HelperToApp {
        HelperToApp::FrameSection {
            handle,
            width: 1,
            height: 1,
        }
    }

    fn frame(display: u32, sequence: u64) -> HelperToApp {
        HelperToApp::Frame {
            display,
            slot: FrameSlot::Second,
            sequence,
        }
    }

    fn held(view: &SecureDesktopView) -> Option<u8> {
        let (epoch, _) = view.route();
        match view.wait_frame(epoch, None, Duration::ZERO) {
            Wait::Frame { image, .. } => Some(image.pixels()[0]),
            Wait::Nothing | Wait::Unavailable | Wait::Moved => None,
        }
    }

    #[tokio::test]
    async fn a_wanted_frame_is_copied_and_held_before_it_is_acknowledged() {
        let Setup {
            mut handover,
            log,
            driver,
            view,
            ..
        } = setup(FakeMapper::default());
        assert_eq!(handover.handle(section(5), &driver).await.unwrap(), None);
        assert_eq!(
            handover.handle(frame(DISPLAY, 9), &driver).await.unwrap(),
            Some(AppToHelper::RequestFrame)
        );
        assert_eq!(log.entries(), ["map 5 1x1", "copy 5 Second 9"]);
        assert_eq!(held(&view), Some(9));
    }

    #[tokio::test]
    async fn a_frame_the_link_would_drop_is_acknowledged_without_a_copy() {
        let Setup {
            mut handover,
            log,
            driver,
            view,
            ..
        } = setup(FakeMapper::default());
        handover.handle(section(5), &driver).await.unwrap();
        assert_eq!(
            handover
                .handle(frame(DISPLAY + 1, 9), &driver)
                .await
                .unwrap(),
            Some(AppToHelper::RequestFrame)
        );
        driver.desktop_changed(InputDesktop::Default);
        assert_eq!(
            handover.handle(frame(DISPLAY, 10), &driver).await.unwrap(),
            Some(AppToHelper::RequestFrame)
        );
        assert_eq!(log.entries(), ["map 5 1x1"]);
        assert_eq!(held(&view), None);
    }

    #[tokio::test]
    async fn a_torn_copy_is_dropped_and_still_acknowledged() {
        let Setup {
            mut handover,
            driver,
            view,
            ..
        } = setup(FakeMapper {
            torn: true,
            ..FakeMapper::default()
        });
        handover.handle(section(5), &driver).await.unwrap();
        assert_eq!(
            handover.handle(frame(DISPLAY, 9), &driver).await.unwrap(),
            Some(AppToHelper::RequestFrame)
        );
        assert_eq!(held(&view), None);
    }

    #[tokio::test]
    async fn each_replaced_section_is_unmapped_then_released_once() {
        let Setup {
            mut handover,
            log,
            driver,
            ..
        } = setup(FakeMapper::default());
        let mut replies = Vec::new();
        for handle in [5, 6, 7] {
            replies.push(handover.handle(section(handle), &driver).await.unwrap());
        }
        handover.handle(frame(DISPLAY, 9), &driver).await.unwrap();
        assert_eq!(
            replies,
            [
                None,
                Some(AppToHelper::SectionReleased),
                Some(AppToHelper::SectionReleased)
            ]
        );
        assert_eq!(
            log.entries(),
            [
                "map 5 1x1",
                "map 6 1x1",
                "unmap 5",
                "map 7 1x1",
                "unmap 6",
                "copy 7 Second 9"
            ]
        );
    }

    #[tokio::test]
    async fn a_frame_before_any_section_ends_the_link() {
        let Setup {
            mut handover,
            driver,
            ..
        } = setup(FakeMapper::default());
        assert!(matches!(
            handover.handle(frame(DISPLAY, 9), &driver).await,
            Err(HandoverError::FrameBeforeSection)
        ));
    }

    #[tokio::test]
    async fn a_section_that_cannot_be_mapped_ends_the_link() {
        let Setup {
            mut handover,
            driver,
            ..
        } = setup(FakeMapper {
            refuse: true,
            ..FakeMapper::default()
        });
        assert!(matches!(
            handover.handle(section(5), &driver).await,
            Err(HandoverError::BadSection(_))
        ));
    }

    #[tokio::test]
    async fn desktop_changes_and_an_unavailable_screen_reach_the_link() {
        let Setup {
            mut handover,
            driver,
            view,
            ..
        } = setup(FakeMapper::default());
        handover.handle(section(5), &driver).await.unwrap();
        assert_eq!(
            handover
                .handle(HelperToApp::ScreenUnavailable { display: DISPLAY }, &driver)
                .await
                .unwrap(),
            None
        );
        let (epoch, _) = view.route();
        assert!(matches!(
            view.wait_frame(epoch, None, Duration::ZERO),
            Wait::Unavailable
        ));
        handover
            .handle(HelperToApp::DesktopChanged(InputDesktop::Default), &driver)
            .await
            .unwrap();
        assert_eq!(
            view.route(),
            (
                epoch + 1,
                crate::secure_desktop::Route::Default { helper_live: true }
            )
        );
    }
}
