use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU64, Ordering, fence};

use dari_proto::{FRAME_SECTION_MAGIC, FRAME_SECTION_VERSION, FrameLayout, FrameSlot};
use windows::Win32::Foundation::{
    DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::PCWSTR;

use super::{owned, raw};
use crate::channel::{SectionFactory, SectionMemory};

/// Creates frame sections and duplicates a read-only handle to each into the app's process.
#[derive(Debug)]
pub(crate) struct AppSections<'a, P> {
    app: &'a P,
}

impl<'a, P: AsRawHandle> AppSections<'a, P> {
    pub(crate) fn new(app: &'a P) -> Self {
        Self { app }
    }
}

impl<P: AsRawHandle> SectionFactory for AppSections<'_, P> {
    type Section = Section;

    fn create(&mut self, layout: FrameLayout) -> io::Result<(Section, u64)> {
        let section = Section::create(layout)?;
        let handle = section.duplicate_read_only(self.app)?;
        Ok((section, handle))
    }
}

/// An unnamed section laid out per `FrameLayout`, with the helper's writable view of it.
#[derive(Debug)]
pub(crate) struct Section {
    mapping: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    layout: FrameLayout,
}

impl Section {
    fn create(layout: FrameLayout) -> io::Result<Self> {
        let len = layout.total_len() as u64;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the size's low and high halves"
        )]
        let (low, high) = (len as u32, (len >> 32) as u32);
        // SAFETY: no name and no security attributes; a pagefile-backed mapping of `len` bytes.
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                high,
                low,
                PCWSTR::null(),
            )?
        };
        // SAFETY: `CreateFileMappingW` just returned this handle to us.
        let mapping = unsafe { owned(mapping) };
        // SAFETY: maps the whole section this process just created.
        let view =
            unsafe { MapViewOfFile(raw(&mapping), FILE_MAP_WRITE, 0, 0, layout.total_len()) };
        if view.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        let section = Self {
            mapping,
            view,
            layout,
        };
        for (offset, value) in [
            (FrameLayout::MAGIC_OFFSET, FRAME_SECTION_MAGIC),
            (FrameLayout::VERSION_OFFSET, FRAME_SECTION_VERSION),
            (FrameLayout::WIDTH_OFFSET, layout.width()),
            (FrameLayout::HEIGHT_OFFSET, layout.height()),
        ] {
            // SAFETY: every header field lies in the view's first page, which nothing else
            // writes, and no other process can see the section yet.
            unsafe {
                section
                    .base()
                    .add(offset)
                    .cast::<[u8; 4]>()
                    .write_unaligned(value.to_le_bytes());
            }
        }
        Ok(section)
    }

    /// A handle to the section, valid in `process`, that can map it only for reading.
    fn duplicate_read_only(&self, process: &impl AsRawHandle) -> io::Result<u64> {
        let mut duplicate = HANDLE::default();
        // The handle belongs to `process` from here on. If the app's link ends before it reads
        // the `FrameSection` naming it, nobody closes it and the section leaks in the app until
        // the app exits. That takes a size change at the instant the session ends, and draining
        // the pipe for it isn't worth the machinery.
        // SAFETY: duplicates a handle this process owns into `process`, which the service opened
        // with PROCESS_DUP_HANDLE; the new handle is only read as a value.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                raw(&self.mapping),
                raw(process),
                &raw mut duplicate,
                FILE_MAP_READ.0,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )?;
        }
        Ok(duplicate.0 as usize as u64)
    }

    fn base(&self) -> *mut u8 {
        self.view.Value.cast()
    }

    #[expect(
        clippy::cast_ptr_alignment,
        reason = "sequence words are 8-byte aligned in a page-aligned view"
    )]
    fn sequence(&self, slot: FrameSlot) -> &AtomicU64 {
        // SAFETY: the word is 8-byte aligned inside the page-aligned view, which lives as long
        // as `self`, and the helper touches it only through atomics.
        unsafe {
            AtomicU64::from_ptr(
                self.base()
                    .add(FrameLayout::sequence_offset(slot))
                    .cast::<u64>(),
            )
        }
    }
}

impl SectionMemory for Section {
    fn write(&mut self, slot: FrameSlot, sequence: u64, pixels: impl FnOnce(&mut [u8])) {
        let word = self.sequence(slot);
        word.store(0, Ordering::Relaxed);
        // Orders the 0 before every pixel write, so an app that saw the old sequence after its
        // copy also saw none of the new pixels.
        fence(Ordering::Release);
        // SAFETY: the slot lies inside the view, which lives as long as `self`. `AppChannel` only
        // writes a slot the app doesn't own, and the app only ever reads, so nothing else
        // accesses these bytes while the slice lives.
        let slot_bytes = unsafe {
            std::slice::from_raw_parts_mut(
                self.base().add(self.layout.slot_offset(slot)),
                self.layout.slot_len(),
            )
        };
        pixels(slot_bytes);
        self.sequence(slot).store(sequence, Ordering::Release);
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view came from `MapViewOfFile` and nothing uses it after this.
        let _unmapped = unsafe { UnmapViewOfFile(self.view) };
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

    use super::*;
    use crate::win32::open_client_process;

    #[test]
    fn the_app_side_handle_cannot_map_the_section_for_writing() {
        let layout = FrameLayout::new(3, 2).unwrap();
        let mut section = Section::create(layout).unwrap();
        section.write(FrameSlot::Second, 7, |pixels| pixels.fill(0xAB));
        let this = open_client_process(std::process::id()).unwrap();
        let handle = section.duplicate_read_only(&this).unwrap();
        // SAFETY: the duplicate was made in this process for this test, which closes it once.
        let handle = unsafe { owned(HANDLE(usize::try_from(handle).unwrap() as *mut _)) };

        // SAFETY: maps a section handle this test owns; a failed map returns null.
        let writable = unsafe { MapViewOfFile(raw(&handle), FILE_MAP_WRITE, 0, 0, 0) };
        assert!(writable.Value.is_null());
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(ERROR_ACCESS_DENIED.0.cast_signed())
        );

        // SAFETY: as above; the view is unmapped before the test ends.
        let readable = unsafe { MapViewOfFile(raw(&handle), FILE_MAP_READ, 0, 0, 0) };
        assert!(!readable.Value.is_null());
        // SAFETY: the view covers the whole section, laid out per `layout`.
        let bytes =
            unsafe { std::slice::from_raw_parts(readable.Value.cast::<u8>(), layout.total_len()) };
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(word(FrameLayout::MAGIC_OFFSET), FRAME_SECTION_MAGIC);
        assert_eq!(word(FrameLayout::VERSION_OFFSET), FRAME_SECTION_VERSION);
        assert_eq!(word(FrameLayout::WIDTH_OFFSET), 3);
        assert_eq!(word(FrameLayout::HEIGHT_OFFSET), 2);
        let sequence = FrameLayout::sequence_offset(FrameSlot::Second);
        assert_eq!(
            u64::from_le_bytes(bytes[sequence..sequence + 8].try_into().unwrap()),
            7
        );
        let second = layout.slot_offset(FrameSlot::Second);
        assert!(
            bytes[second..second + layout.slot_len()]
                .iter()
                .all(|&byte| byte == 0xAB)
        );
        let first = layout.slot_offset(FrameSlot::First);
        assert!(
            bytes[first..first + layout.slot_len()]
                .iter()
                .all(|&byte| byte == 0)
        );
        // SAFETY: the view came from `MapViewOfFile` above and `bytes` isn't used after this.
        unsafe { UnmapViewOfFile(readable).unwrap() };
    }
}
