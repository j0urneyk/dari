//! Samples Apple silicon power without sudo, from the same `IOReport` "Energy Model" counters
//! `powermetrics` reads, and prints watts per channel for each interval.
//!
//! ```sh
//! cargo run --release -p dari-media --example power_sample -- [interval-ms] [count] [CHANNEL...]
//! ```
//!
//! Each line is a Unix timestamp (the end of the interval) followed by tab-separated
//! `CHANNEL=watts` pairs, so the lines inside a benchmark's window can be averaged. A `count` of
//! 0 samples until interrupted; otherwise a final `mean` line gives each channel's average over
//! the whole run. With channel names (`AVE` is the hardware video encoder, and `GPU Energy`,
//! `DRAM`, and `CPU Energy` are the other useful ones) only those are printed, zeros included;
//! without, every channel that used energy in the interval is. macOS only.

#![allow(clippy::print_stdout, reason = "sampler output")]

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::fmt::Write as _;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    let mut args = std::env::args().skip(1);
    let interval_ms: u64 = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(1000);
    let count: u64 = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(10);
    let filter: Vec<String> = args.collect();

    let mut sampler = ioreport::EnergySampler::new()?;
    let mut previous = Instant::now();
    // Joules per channel over the whole run, for the `mean` line.
    let mut totals: Vec<(String, f64)> = Vec::new();
    let mut elapsed = 0.0;
    let mut sample = 0;
    while count == 0 || sample < count {
        std::thread::sleep(Duration::from_millis(interval_ms));
        let joules = sampler.sample()?;
        let now = Instant::now();
        let seconds = now.duration_since(previous).as_secs_f64();
        previous = now;
        elapsed += seconds;
        sample += 1;

        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        let mut line = format!("{timestamp:.3}");
        let selected: Vec<(String, f64)> = if filter.is_empty() {
            joules
                .into_iter()
                .filter(|(_, energy)| *energy > 0.0)
                .collect()
        } else {
            // In the order asked for, and a typo fails instead of printing nothing.
            filter
                .iter()
                .map(|wanted| {
                    joules
                        .iter()
                        .find(|(name, _)| name == wanted)
                        .cloned()
                        .ok_or_else(|| format!("no energy channel named {wanted:?}"))
                })
                .collect::<Result<_, _>>()?
        };
        for (name, energy) in selected {
            write!(line, "\t{name}={:.4}", energy / seconds)?;
            match totals
                .iter_mut()
                .find(|(total_name, _)| *total_name == name)
            {
                Some((_, total)) => *total += energy,
                None => totals.push((name, energy)),
            }
        }
        println!("{line}");
    }

    let mut line = String::from("mean");
    for (name, energy) in totals {
        write!(line, "\t{name}={:.4}", energy / elapsed)?;
    }
    println!("{line}");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("power_sample reads Apple silicon's IOReport counters and only runs on macOS".into())
}

/// The private libIOReport.dylib calls `powermetrics` is built on. They aren't in any SDK
/// header, so the signatures follow the ones reverse-engineered tools use.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "calls into the private libIOReport.dylib")]
mod ioreport {
    use std::ffi::{c_int, c_void};
    use std::ptr::{self, NonNull};

    use block2::StackBlock;
    use objc2_core_foundation::{CFDictionary, CFMutableDictionary, CFRetained, CFString, CFType};

    /// Opaque; released with `CFRelease` like any CoreFoundation object.
    #[repr(C)]
    struct IOReportSubscription {
        _private: [u8; 0],
    }

    #[link(name = "IOReport", kind = "dylib")]
    unsafe extern "C" {
        fn IOReportCopyChannelsInGroup(
            group: &CFString,
            subgroup: *const CFString,
            a: u64,
            b: u64,
            c: u64,
        ) -> *mut CFMutableDictionary;
        fn IOReportCreateSubscription(
            allocator: *const c_void,
            channels: &CFMutableDictionary,
            subscribed: *mut *mut CFMutableDictionary,
            channel_id: u64,
            options: *const CFType,
        ) -> *mut IOReportSubscription;
        fn IOReportCreateSamples(
            subscription: *mut IOReportSubscription,
            subscribed: &CFMutableDictionary,
            options: *const CFType,
        ) -> *mut CFDictionary;
        fn IOReportCreateSamplesDelta(
            previous: &CFDictionary,
            current: &CFDictionary,
            options: *const CFType,
        ) -> *mut CFDictionary;
        fn IOReportChannelGetChannelName(channel: &CFDictionary) -> *const CFString;
        fn IOReportChannelGetUnitLabel(channel: &CFDictionary) -> *const CFString;
        fn IOReportSimpleGetIntegerValue(channel: &CFDictionary, index: i32) -> i64;
        fn IOReportIterate(
            samples: &CFDictionary,
            block: &block2::Block<dyn Fn(*const CFDictionary) -> c_int + '_>,
        );
    }

    #[derive(Debug)]
    pub(super) struct EnergySampler {
        subscription: NonNull<IOReportSubscription>,
        subscribed: CFRetained<CFMutableDictionary>,
        previous: CFRetained<CFDictionary>,
    }

    impl EnergySampler {
        pub(super) fn new() -> Result<Self, &'static str> {
            let group = CFString::from_static_str("Energy Model");
            // SAFETY: `group` is a valid string and a null subgroup means every subgroup. The
            // result follows the Copy rule, so it is owned (+1).
            let channels =
                NonNull::new(unsafe { IOReportCopyChannelsInGroup(&group, ptr::null(), 0, 0, 0) })
                    .ok_or("IOReport has no Energy Model group")?;
            // SAFETY: Owned (+1), see above.
            let channels = unsafe { CFRetained::from_raw(channels) };

            let mut subscribed = ptr::null_mut();
            // SAFETY: `channels` is valid and `subscribed` is a valid out pointer, which the
            // call fills with a dictionary it creates (+1).
            let subscription = NonNull::new(unsafe {
                IOReportCreateSubscription(
                    ptr::null(),
                    &channels,
                    &raw mut subscribed,
                    0,
                    ptr::null(),
                )
            })
            .ok_or("IOReport refused the subscription")?;
            let subscribed = NonNull::new(subscribed).ok_or("IOReport subscribed no channels")?;
            // SAFETY: Created by the call above (+1).
            let subscribed = unsafe { CFRetained::from_raw(subscribed) };
            let previous = create_samples(subscription, &subscribed)?;
            Ok(Self {
                subscription,
                subscribed,
                previous,
            })
        }

        /// Returns each channel's energy in joules since the previous call (or since `new`).
        pub(super) fn sample(&mut self) -> Result<Vec<(String, f64)>, &'static str> {
            let current = create_samples(self.subscription, &self.subscribed)?;
            // SAFETY: Both are sample dictionaries from this subscription; the result follows
            // the Create rule, so it is owned (+1).
            let delta = NonNull::new(unsafe {
                IOReportCreateSamplesDelta(&self.previous, &current, ptr::null())
            })
            .ok_or("IOReport could not compute a sample delta")?;
            // SAFETY: Owned (+1), see above.
            let delta = unsafe { CFRetained::from_raw(delta) };
            self.previous = current;

            let joules = std::cell::RefCell::new(Vec::new());
            let block = StackBlock::new(|channel: *const CFDictionary| -> c_int {
                // SAFETY: IOReportIterate passes each channel dictionary of `delta`, valid for
                // the duration of the callback.
                let Some(channel) = (unsafe { channel.as_ref() }) else {
                    return 0;
                };
                // SAFETY: `channel` is a channel dictionary; the getters return borrowed (+0)
                // strings owned by it, or null.
                let (name, unit) = unsafe {
                    (
                        IOReportChannelGetChannelName(channel).as_ref(),
                        IOReportChannelGetUnitLabel(channel).as_ref(),
                    )
                };
                let scale = match unit.map(ToString::to_string).as_deref() {
                    Some("mJ") => 1e-3,
                    Some("uJ") => 1e-6,
                    Some("nJ") => 1e-9,
                    _ => return 0,
                };
                let Some(name) = name else { return 0 };
                // SAFETY: Energy channels are simple channels with a single integer value.
                let value = unsafe { IOReportSimpleGetIntegerValue(channel, 0) };
                #[allow(clippy::cast_precision_loss, reason = "energy counts fit in f64")]
                joules
                    .borrow_mut()
                    .push((name.to_string(), value as f64 * scale));
                0
            });
            // SAFETY: `delta` is a valid sample dictionary and the stack block outlives the call.
            unsafe { IOReportIterate(&delta, &block) };
            Ok(joules.into_inner())
        }
    }

    impl Drop for EnergySampler {
        fn drop(&mut self) {
            // SAFETY: The subscription is a CoreFoundation object we own (+1) and don't use
            // after this.
            unsafe {
                CFRetained::from_raw(self.subscription.cast::<CFType>());
            }
        }
    }

    fn create_samples(
        subscription: NonNull<IOReportSubscription>,
        subscribed: &CFMutableDictionary,
    ) -> Result<CFRetained<CFDictionary>, &'static str> {
        // SAFETY: The subscription and its channel dictionary are valid; the result follows
        // the Create rule, so it is owned (+1).
        let samples = NonNull::new(unsafe {
            IOReportCreateSamples(subscription.as_ptr(), subscribed, ptr::null())
        })
        .ok_or("IOReport returned no samples")?;
        // SAFETY: Owned (+1), see above.
        Ok(unsafe { CFRetained::from_raw(samples) })
    }
}
