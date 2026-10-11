//! Everything the helper tells the app: desktop changes, unavailable screens, sections, and
//! frames. `AppChannel` owns the double-buffer credit, so no caller can publish a frame, report a
//! desktop change, or replace a section in an order that breaks the protocol.
//!
//! The credit: the app owns the slot of the last `Frame` it hasn't answered with `RequestFrame`.
//! The helper writes only the other slot and publishes at most one frame per credit. A section is
//! replaced only while the app owns nothing.
use std::io;

use dari_proto::{FrameLayout, FrameSlot, HelperToApp, InputDesktop};

pub(crate) trait Outbox {
    fn send(&mut self, message: HelperToApp) -> io::Result<()>;
}

pub(crate) trait SectionFactory {
    type Section: SectionMemory;
    /// A section laid out for `layout` with its header written, and the value of a handle to it,
    /// valid in the app's process, that can only map it for reading.
    fn create(&mut self, layout: FrameLayout) -> io::Result<(Self::Section, u64)>;
    /// Closes the app's handle with value `handle`, which `create` returned. Only for a handle the
    /// app can never have read, or the value may name another of the app's handles by now.
    fn close_in_app(&mut self, handle: u64) -> io::Result<()>;
}

/// The helper's writable view of one section. Dropping it closes the helper's view and handle.
pub(crate) trait SectionMemory {
    /// Sets the slot's sequence word to 0, lets `pixels` fill the slot, then sets the word to
    /// `sequence`.
    fn write(&mut self, slot: FrameSlot, sequence: u64, pixels: impl FnOnce(&mut [u8]));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offered {
    /// Written and published: the app owns it now.
    Published,
    /// Written into the slot the app doesn't own; published when the app returns its credit.
    Drafted,
    /// Not written: the size changed and the section can't be replaced yet, because the app owns
    /// a slot. Offer it again later.
    Deferred,
}

#[derive(Debug, Clone, Copy)]
struct Draft {
    slot: FrameSlot,
    display: u32,
    sequence: u64,
}

#[derive(Debug)]
enum Credit {
    Free,
    Held {
        slot: FrameSlot,
        draft: Option<Draft>,
    },
}

#[derive(Debug)]
struct Current<S> {
    layout: FrameLayout,
    section: S,
}

#[derive(Debug)]
pub(crate) struct AppChannel<O, F: SectionFactory> {
    outbox: O,
    factory: F,
    current: Option<Current<F::Section>>,
    credit: Credit,
    next_sequence: u64,
}

impl<O: Outbox, F: SectionFactory> AppChannel<O, F> {
    pub(crate) fn new(outbox: O, factory: F) -> Self {
        Self {
            outbox,
            factory,
            current: None,
            credit: Credit::Free,
            next_sequence: 1,
        }
    }

    pub(crate) fn desktop_changed(&mut self, desktop: InputDesktop) -> io::Result<()> {
        self.discard_draft();
        self.outbox.send(HelperToApp::DesktopChanged(desktop))
    }

    pub(crate) fn screen_unavailable(&mut self, display: u32) -> io::Result<()> {
        self.discard_draft();
        self.outbox.send(HelperToApp::ScreenUnavailable { display })
    }

    pub(crate) fn capture_ended(&mut self) {
        self.discard_draft();
    }

    pub(crate) fn offer(
        &mut self,
        layout: FrameLayout,
        display: u32,
        pixels: impl FnOnce(&mut [u8]),
    ) -> io::Result<Offered> {
        let slot = match &self.credit {
            Credit::Free => FrameSlot::First,
            Credit::Held { slot, .. } => slot.other(),
        };
        let sequence = self.next_sequence;
        let Some(section) = self.section_for(layout)? else {
            return Ok(Offered::Deferred);
        };
        section.write(slot, sequence, pixels);
        self.next_sequence += 1;
        let draft = Draft {
            slot,
            display,
            sequence,
        };
        match &mut self.credit {
            Credit::Free => {
                self.publish(draft)?;
                Ok(Offered::Published)
            }
            Credit::Held { draft: waiting, .. } => {
                *waiting = Some(draft);
                Ok(Offered::Drafted)
            }
        }
    }

    pub(crate) fn request_frame(&mut self) -> io::Result<()> {
        match std::mem::replace(&mut self.credit, Credit::Free) {
            Credit::Held {
                draft: Some(draft), ..
            } => self.publish(draft),
            Credit::Held { draft: None, .. } | Credit::Free => Ok(()),
        }
    }

    fn section_for(&mut self, layout: FrameLayout) -> io::Result<Option<&mut F::Section>> {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.layout == layout)
        {
            return Ok(self.current.as_mut().map(|current| &mut current.section));
        }
        if let Credit::Held { draft, .. } = &mut self.credit {
            // The draft has the old size, and the frame on offer is newer anyway.
            *draft = None;
            return Ok(None);
        }
        let (section, handle) = self.factory.create(layout)?;
        self.outbox.send(HelperToApp::FrameSection {
            handle,
            width: layout.width(),
            height: layout.height(),
        })?;
        Ok(Some(
            &mut self.current.insert(Current { layout, section }).section,
        ))
    }

    fn publish(&mut self, draft: Draft) -> io::Result<()> {
        self.credit = Credit::Held {
            slot: draft.slot,
            draft: None,
        };
        self.outbox.send(HelperToApp::Frame {
            display: draft.display,
            slot: draft.slot,
            sequence: draft.sequence,
        })
    }

    fn discard_draft(&mut self) {
        if let Credit::Held { draft, .. } = &mut self.credit {
            *draft = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::rc::Rc;

    use dari_proto::AppToHelper;

    use super::*;

    #[derive(Debug, Default)]
    struct World {
        sent: Vec<HelperToApp>,
        sections: Vec<TestSection>,
        last_section: Option<usize>,
        closed_in_app: Vec<u64>,
        owned: Option<(usize, FrameSlot)>,
        fills: HashMap<u64, u8>,
    }

    #[derive(Debug)]
    struct TestSection {
        layout: FrameLayout,
        bytes: Vec<u8>,
        open: bool,
    }

    impl TestSection {
        fn sequence(&self, slot: FrameSlot) -> u64 {
            let at = FrameLayout::sequence_offset(slot);
            u64::from_le_bytes(self.bytes[at..at + 8].try_into().unwrap())
        }

        fn pixels(&mut self, slot: FrameSlot) -> &mut [u8] {
            let at = self.layout.slot_offset(slot);
            &mut self.bytes[at..at + self.layout.slot_len()]
        }
    }

    type Shared = Rc<RefCell<World>>;

    #[derive(Debug)]
    struct Outgoing(Shared);

    impl Outbox for Outgoing {
        fn send(&mut self, message: HelperToApp) -> io::Result<()> {
            let mut world = self.0.borrow_mut();
            match &message {
                HelperToApp::FrameSection { handle, .. } => {
                    world.last_section = Some(usize::try_from(handle - 1).unwrap());
                }
                HelperToApp::Frame { slot, .. } => {
                    assert_eq!(world.owned, None, "a second credit was spent");
                    world.owned = Some((world.last_section.unwrap(), *slot));
                }
                HelperToApp::DesktopChanged(_) | HelperToApp::ScreenUnavailable { .. } => {}
            }
            world.sent.push(message);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Factory(Shared);

    #[derive(Debug)]
    struct SectionRef {
        id: usize,
        world: Shared,
    }

    impl SectionFactory for Factory {
        type Section = SectionRef;

        fn create(&mut self, layout: FrameLayout) -> io::Result<(SectionRef, u64)> {
            let mut world = self.0.borrow_mut();
            world.sections.push(TestSection {
                layout,
                bytes: vec![0; layout.total_len()],
                open: true,
            });
            let open = world.sections.iter().filter(|section| section.open).count();
            assert!(open <= 2, "{open} sections open");
            let id = world.sections.len() - 1;
            let section = SectionRef {
                id,
                world: self.0.clone(),
            };
            Ok((section, u64::try_from(id).unwrap() + 1))
        }

        fn close_in_app(&mut self, handle: u64) -> io::Result<()> {
            self.0.borrow_mut().closed_in_app.push(handle);
            Ok(())
        }
    }

    impl SectionMemory for SectionRef {
        fn write(&mut self, slot: FrameSlot, sequence: u64, pixels: impl FnOnce(&mut [u8])) {
            let mut world = self.world.borrow_mut();
            assert_ne!(
                world.owned,
                Some((self.id, slot)),
                "wrote the slot the app owns"
            );
            let section = &mut world.sections[self.id];
            let at = FrameLayout::sequence_offset(slot);
            section.bytes[at..at + 8].copy_from_slice(&0u64.to_le_bytes());
            let target = section.pixels(slot);
            pixels(target);
            let fill = target[0];
            section.bytes[at..at + 8].copy_from_slice(&sequence.to_le_bytes());
            world.fills.insert(sequence, fill);
        }
    }

    impl Drop for SectionRef {
        fn drop(&mut self) {
            self.world.borrow_mut().sections[self.id].open = false;
        }
    }

    #[derive(Debug, Default)]
    struct App {
        read: usize,
        mapped: Option<usize>,
        frame: Option<(FrameSlot, u64)>,
        replies: VecDeque<AppToHelper>,
        frames_copied: usize,
    }

    impl App {
        fn read_next(&mut self, world: &World) -> bool {
            let Some(message) = world.sent.get(self.read) else {
                return false;
            };
            self.read += 1;
            match message {
                HelperToApp::FrameSection { handle, .. } => {
                    assert_eq!(
                        self.frame, None,
                        "a section was replaced under a held frame"
                    );
                    let id = usize::try_from(handle - 1).unwrap();
                    assert!(world.sections[id].open);
                    self.mapped = Some(id);
                }
                HelperToApp::Frame { slot, sequence, .. } => {
                    assert!(self.mapped.is_some(), "a frame before any section");
                    assert_eq!(self.frame, None, "a frame while the app owned one");
                    self.frame = Some((*slot, *sequence));
                }
                HelperToApp::DesktopChanged(_) | HelperToApp::ScreenUnavailable { .. } => {}
            }
            true
        }

        fn copy(&mut self, world: &mut World) {
            let (Some(id), Some((slot, sequence))) = (self.mapped, self.frame) else {
                return;
            };
            let fill = world.fills[&sequence];
            let section = &mut world.sections[id];
            assert!(section.open, "read a closed section");
            assert_eq!(section.sequence(slot), sequence);
            assert!(section.pixels(slot).iter().all(|&byte| byte == fill));
            assert_eq!(section.sequence(slot), sequence);
            self.frames_copied += 1;
        }

        fn give_back(&mut self) {
            if self.frame.take().is_some() {
                self.replies.push_back(AppToHelper::RequestFrame);
            }
        }
    }

    type Channel = AppChannel<Outgoing, Factory>;

    fn channel() -> (Channel, Shared) {
        let world = Shared::default();
        let channel = AppChannel::new(Outgoing(world.clone()), Factory(world.clone()));
        (channel, world)
    }

    fn deliver(channel: &mut Channel, world: &Shared, app: &mut App) -> bool {
        match app.replies.pop_front() {
            Some(AppToHelper::RequestFrame) => {
                world.borrow_mut().owned = None;
                channel.request_frame().unwrap();
            }
            Some(AppToHelper::SelectDisplay(_) | AppToHelper::Input(_) | AppToHelper::Stop) => {
                unreachable!()
            }
            None => return false,
        }
        true
    }

    fn layout(width: u32, height: u32) -> FrameLayout {
        FrameLayout::new(width, height).unwrap()
    }

    fn fill(byte: u8) -> impl FnOnce(&mut [u8]) {
        move |pixels: &mut [u8]| pixels.fill(byte)
    }

    struct XorShift(u64);

    impl XorShift {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % bound
        }
    }

    #[test]
    fn the_writer_never_touches_the_slot_the_reader_holds() {
        let layouts = [layout(2, 1), layout(3, 2)];
        for seed in 1..=64u64 {
            let mut random = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let (mut channel, world) = channel();
            let mut app = App::default();
            let mut size = 0;
            for step in 0..10_000u32 {
                match random.below(9) {
                    0 | 1 => {
                        if random.below(8) == 0 {
                            size = 1 - size;
                        }
                        let byte = u8::try_from(step % 251).unwrap() + 1;
                        channel.offer(layouts[size], 7, fill(byte)).unwrap();
                    }
                    2 | 3 => {
                        app.read_next(&world.borrow());
                    }
                    4 => app.copy(&mut world.borrow_mut()),
                    5 => {
                        app.copy(&mut world.borrow_mut());
                        app.give_back();
                    }
                    6 => app.give_back(),
                    7 => {
                        deliver(&mut channel, &world, &mut app);
                    }
                    _ => match random.below(3) {
                        0 => channel.desktop_changed(InputDesktop::Winlogon).unwrap(),
                        1 => channel.screen_unavailable(7).unwrap(),
                        _ => channel.capture_ended(),
                    },
                }
            }
            assert!(
                app.frames_copied > 100,
                "seed {seed}: {}",
                app.frames_copied
            );

            let mut published = None;
            for _round in 0..8 {
                while app.read_next(&world.borrow()) {}
                app.copy(&mut world.borrow_mut());
                app.give_back();
                while deliver(&mut channel, &world, &mut app) {}
                if channel.offer(layouts[1 - size], 7, fill(9)).unwrap() == Offered::Published {
                    published = Some(());
                    break;
                }
            }
            assert!(published.is_some(), "seed {seed}");
        }
    }

    #[test]
    fn a_size_change_waits_for_the_held_frame_to_be_returned() {
        let (mut channel, world) = channel();
        let mut app = App::default();
        assert_eq!(
            channel.offer(layout(2, 1), 1, fill(1)).unwrap(),
            Offered::Published
        );
        assert_eq!(
            channel.offer(layout(2, 1), 1, fill(2)).unwrap(),
            Offered::Drafted
        );
        assert_eq!(
            channel.offer(layout(4, 4), 1, fill(3)).unwrap(),
            Offered::Deferred
        );
        assert_eq!(world.borrow().sections.len(), 1);

        while app.read_next(&world.borrow()) {}
        app.give_back();
        deliver(&mut channel, &world, &mut app);
        assert!(matches!(
            world.borrow().sent.last(),
            Some(HelperToApp::Frame { sequence: 1, .. })
        ));
        assert_eq!(
            channel.offer(layout(4, 4), 1, fill(3)).unwrap(),
            Offered::Published
        );
        assert_eq!(world.borrow().sections.len(), 2);
        assert!(
            !world.borrow().sections[0].open,
            "the replaced section closes at once"
        );
    }

    #[test]
    fn a_desktop_change_or_an_ended_capture_discards_the_unpublished_draft() {
        let discards: [fn(&mut Channel); 2] = [
            |channel| channel.desktop_changed(InputDesktop::Default).unwrap(),
            Channel::capture_ended,
        ];
        for discard in discards {
            let (mut channel, world) = channel();
            let mut app = App::default();
            channel.offer(layout(2, 1), 1, fill(1)).unwrap();
            assert_eq!(
                channel.offer(layout(2, 1), 1, fill(2)).unwrap(),
                Offered::Drafted
            );
            discard(&mut channel);

            while app.read_next(&world.borrow()) {}
            app.give_back();
            deliver(&mut channel, &world, &mut app);
            let sent = &world.borrow().sent;
            assert!(
                !sent
                    .iter()
                    .any(|message| matches!(message, HelperToApp::Frame { sequence: 2, .. })),
                "{sent:?}"
            );
        }
    }

    #[derive(Debug)]
    struct Failing(io::ErrorKind);

    impl Outbox for Failing {
        fn send(&mut self, _message: HelperToApp) -> io::Result<()> {
            Err(self.0.into())
        }
    }

    #[test]
    fn a_section_sent_into_a_closed_pipe_is_closed_in_the_app_but_one_that_timed_out_is_not() {
        for (failure, closed) in [
            (io::ErrorKind::BrokenPipe, vec![1]),
            (io::ErrorKind::TimedOut, vec![]),
        ] {
            let world = Shared::default();
            let mut channel = AppChannel::new(Failing(failure), Factory(world.clone()));
            let error = channel.offer(layout(2, 1), 1, fill(1)).unwrap_err();
            assert_eq!(error.kind(), failure);
            let world = world.borrow();
            assert_eq!(world.closed_in_app, closed, "{failure:?}");
            assert!(!world.sections[0].open, "{failure:?}");
        }
    }

    #[test]
    fn a_returned_credit_publishes_the_newest_draft_into_the_free_slot() {
        let (mut channel, world) = channel();
        let mut app = App::default();
        channel.offer(layout(2, 1), 5, fill(1)).unwrap();
        channel.offer(layout(2, 1), 5, fill(2)).unwrap();
        channel.offer(layout(2, 1), 5, fill(3)).unwrap();
        while app.read_next(&world.borrow()) {}
        app.copy(&mut world.borrow_mut());
        app.give_back();
        deliver(&mut channel, &world, &mut app);

        assert_eq!(
            world.borrow().sent[1..],
            [
                HelperToApp::Frame {
                    display: 5,
                    slot: FrameSlot::First,
                    sequence: 1
                },
                HelperToApp::Frame {
                    display: 5,
                    slot: FrameSlot::Second,
                    sequence: 3
                },
            ]
        );
        app.read_next(&world.borrow());
        app.copy(&mut world.borrow_mut());
        assert_eq!(app.frames_copied, 2);
    }
}
