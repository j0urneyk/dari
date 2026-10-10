use std::time::Duration;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::desktop::{AttachedDesktop, InputDesktopSource};
use crate::dxgi_result::Hresult;
use crate::pointer::{PointerPosition, RawPointerShape};
use crate::screen::{
    AcquiredFrame, DesktopWorld, Duplicate, Duplication, Size, rgba_from_bgra_rows,
};
use crate::tracker::{DesktopSource, Observation};

#[derive(Debug, Default)]
pub(crate) struct DxgiWorld {
    attached: Option<AttachedDesktop>,
}

impl DesktopSource for DxgiWorld {
    fn poll(&mut self) -> Observation {
        InputDesktopSource.poll()
    }
}

impl DesktopWorld for DxgiWorld {
    type Duplication = DxgiDuplication;

    fn attach(&mut self) -> Result<Observation, Hresult> {
        let (desktop, name) = AttachedDesktop::attach().map_err(hresult)?;
        // The thread moved to `desktop`, so the previous one can close now.
        self.attached = Some(desktop);
        Ok(name)
    }

    fn duplicate(&mut self, display: u32) -> Result<DxgiDuplication, Duplicate> {
        let failed = |error| Duplicate::Failed(hresult(error));
        let (adapter, output) = find_output(display)
            .map_err(failed)?
            .ok_or(Duplicate::NoSuchDisplay)?;
        let mut device = None;
        let mut context = None;
        // SAFETY: the out pointers outlive the call.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )
        }
        .map_err(failed)?;
        let (Some(device), Some(context)) = (device, context) else {
            return Err(Duplicate::Failed(Hresult::DEVICE_REMOVED));
        };
        let output = output.cast::<IDXGIOutput1>().map_err(failed)?;
        // SAFETY: `device` was created on the adapter that owns `output`.
        let duplication = unsafe { output.DuplicateOutput(&device) }.map_err(failed)?;
        // SAFETY: `GetDesc` only fills the returned struct.
        let desc = unsafe { duplication.GetDesc() };
        if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
            && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
        {
            return Err(Duplicate::Failed(Hresult::UNSUPPORTED));
        }
        Ok(DxgiDuplication {
            device,
            context,
            duplication,
            staging: None,
            size: Size {
                width: desc.ModeDesc.Width,
                height: desc.ModeDesc.Height,
            },
            held: None,
        })
    }
}

/// The adapter and output whose `HMONITOR`'s low 32 bits are `display`, as xcap and the app's
/// `find_monitor` name displays.
fn find_output(display: u32) -> windows::core::Result<Option<(IDXGIAdapter1, IDXGIOutput)>> {
    // SAFETY: `CreateDXGIFactory1` takes no pointers.
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>()? };
    for adapter_index in 0.. {
        // SAFETY: an index past the last adapter fails with DXGI_ERROR_NOT_FOUND.
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => return Ok(None),
            Err(error) => return Err(error),
        };
        for output_index in 0.. {
            // SAFETY: an index past the last output fails with DXGI_ERROR_NOT_FOUND.
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error),
            };
            // SAFETY: `GetDesc` only fills the returned struct.
            let desc = unsafe { output.GetDesc()? };
            #[expect(
                clippy::cast_possible_truncation,
                reason = "display IDs are the handle's low 32 bits"
            )]
            if desc.Monitor.0 as usize as u32 == display {
                return Ok(Some((adapter, output)));
            }
        }
    }
    Ok(None)
}

#[derive(Debug)]
pub(crate) struct DxgiDuplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    size: Size,
    held: Option<Held>,
}

#[derive(Debug)]
struct Held {
    image: Option<IDXGIResource>,
    shape_len: u32,
}

impl DxgiDuplication {
    fn staging(&mut self, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D, Hresult> {
        if let Some(staging) = &self.staging {
            return Ok(staging.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
            MiscFlags: 0,
            ..*desc
        };
        let mut staging = None;
        // SAFETY: `desc` and the out pointer outlive the call.
        unsafe {
            self.device
                .CreateTexture2D(&raw const desc, None, Some(&raw mut staging))
        }
        .map_err(hresult)?;
        let staging = staging.ok_or(Hresult::DEVICE_REMOVED)?;
        self.staging = Some(staging.clone());
        Ok(staging)
    }
}

impl Duplication for DxgiDuplication {
    fn size(&self) -> Size {
        self.size
    }

    fn acquire(&mut self, timeout: Duration) -> Result<AcquiredFrame, Hresult> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut image = None;
        // SAFETY: the out pointers outlive the call.
        unsafe {
            self.duplication.AcquireNextFrame(
                u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX),
                &raw mut info,
                &raw mut image,
            )
        }
        .map_err(hresult)?;
        self.held = Some(Held {
            image,
            shape_len: info.PointerShapeBufferSize,
        });
        let position = info.PointerPosition;
        Ok(AcquiredFrame {
            accumulated_frames: info.AccumulatedFrames,
            last_present_time: info.LastPresentTime,
            pointer: (info.LastMouseUpdateTime != 0).then_some(PointerPosition {
                visible: position.Visible.as_bool(),
                x: position.Position.x,
                y: position.Position.y,
            }),
            shape_changed: info.PointerShapeBufferSize != 0,
        })
    }

    fn copy_image(&mut self, into: &mut [u8]) -> Result<(), Hresult> {
        let image = self
            .held
            .as_ref()
            .and_then(|held| held.image.as_ref())
            .ok_or(Hresult::INVALID_CALL)?
            .cast::<ID3D11Texture2D>()
            .map_err(hresult)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` outlives the call that fills it.
        unsafe { image.GetDesc(&raw mut desc) };
        if desc.Width != self.size.width || desc.Height != self.size.height {
            // A mode change DXGI hasn't reported yet: duplicate again.
            return Err(Hresult::ACCESS_LOST);
        }
        let staging = self.staging(&desc)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both textures live on this device with the same size and format; `mapped`
        // outlives the call that fills it.
        unsafe {
            self.context.CopyResource(&staging, &image);
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
        }
        .map_err(hresult)?;
        let width = self.size.width as usize;
        let pitch = mapped.RowPitch as usize;
        let len = pitch * (self.size.height as usize).saturating_sub(1) + width * 4;
        // SAFETY: while mapped, `pData` points at `height` rows of `RowPitch` bytes, each holding
        // `width` BGRA pixels; the slice isn't used after the unmap below.
        let rows = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
        rgba_from_bgra_rows(rows, pitch, width, into);
        // SAFETY: `staging` was mapped above.
        unsafe { self.context.Unmap(&staging, 0) };
        Ok(())
    }

    fn pointer_shape(&mut self) -> Result<RawPointerShape, Hresult> {
        let len = self.held.as_ref().ok_or(Hresult::INVALID_CALL)?.shape_len;
        let mut buffer = vec![0u8; len as usize];
        let mut required = 0;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: `buffer` holds `len` bytes and it and the out pointers outlive the call.
        unsafe {
            self.duplication.GetFramePointerShape(
                len,
                buffer.as_mut_ptr().cast(),
                &raw mut required,
                &raw mut info,
            )
        }
        .map_err(hresult)?;
        Ok(RawPointerShape {
            kind: info.Type,
            width: info.Width,
            height: info.Height,
            pitch: info.Pitch,
            buffer,
        })
    }

    fn release(&mut self) -> Result<(), Hresult> {
        self.held = None;
        // SAFETY: called once per acquired frame; the frame's image was released just above.
        unsafe { self.duplication.ReleaseFrame() }.map_err(hresult)
    }
}

#[expect(clippy::needless_pass_by_value, reason = "a `map_err` callback")]
fn hresult(error: windows::core::Error) -> Hresult {
    Hresult(error.code().0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Instant;

    use dari_proto::FrameLayout;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};

    use super::*;
    use crate::screen::{ScreenEvent, ScreenMachine};

    #[derive(Debug, Default)]
    struct DefaultAsSecure(DxgiWorld);

    fn renamed(observation: Observation) -> Observation {
        match observation {
            Observation::Named(name) if name.eq_ignore_ascii_case("Default") => {
                Observation::Named("Default-as-secure".into())
            }
            other => other,
        }
    }

    impl DesktopSource for DefaultAsSecure {
        fn poll(&mut self) -> Observation {
            renamed(self.0.poll())
        }
    }

    impl DesktopWorld for DefaultAsSecure {
        type Duplication = DxgiDuplication;

        fn attach(&mut self) -> Result<Observation, Hresult> {
            self.0.attach().map(renamed)
        }

        fn duplicate(&mut self, display: u32) -> Result<DxgiDuplication, Duplicate> {
            self.0.duplicate(display)
        }
    }

    #[test]
    #[ignore = "needs an interactive desktop: run it in the signed-in session"]
    fn the_machine_captures_the_primary_display() {
        // SAFETY: `MonitorFromPoint` takes no pointers.
        let primary = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "display IDs are the low 32 bits"
        )]
        let display = primary.0 as usize as u32;
        let (_, output) = find_output(display).unwrap().unwrap();
        // SAFETY: `GetDesc` only fills the returned struct.
        let bounds = unsafe { output.GetDesc() }.unwrap().DesktopCoordinates;
        let expected = FrameLayout::new(
            u32::try_from(bounds.right - bounds.left).unwrap(),
            u32::try_from(bounds.bottom - bounds.top).unwrap(),
        )
        .unwrap();

        let started = Instant::now();
        let mut machine = ScreenMachine::new(DefaultAsSecure::default(), started);
        machine.select_display(display, started);
        let mut pixels = None;
        while started.elapsed() < Duration::from_secs(10) {
            for event in machine.step(Instant::now()) {
                println!("{:?} {event:?}", started.elapsed());
                if let ScreenEvent::Note(note) = event {
                    println!("  {note}");
                }
            }
            if let Some(frame) = machine.dirty_frame() {
                assert_eq!(frame.layout(), expected);
                let mut slot = vec![0; frame.layout().slot_len()];
                frame.write_into(&mut slot);
                pixels = Some(slot);
                break;
            }
        }
        let pixels = pixels.expect("no frame within 10 s");
        let colors: HashSet<&[u8; 4]> = pixels.as_chunks::<4>().0.iter().collect();
        println!(
            "{:?}: {}x{}, {} colors",
            started.elapsed(),
            expected.width(),
            expected.height(),
            colors.len()
        );
        assert!(colors.len() > 16, "{} colors", colors.len());
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 0xFF)
        );
    }
}
