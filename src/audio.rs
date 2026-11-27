//! Microphone capture. A control thread owns the WASAPI stream; the device
//! callback downmixes to mono and pushes into a lock-free SPSC ring that the
//! engine thread drains. The stream opens lazily on the first recording,
//! pauses between dictations and closes after `mic_keep_open_secs`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

pub const RATE: u32 = 16_000;
const RING_SECONDS: usize = 30;

static CAPTURING: AtomicBool = AtomicBool::new(false);

enum Cmd {
    Start { device: String, keep_open: Duration, reply: Sender<Result<StartInfo, String>> },
    Stop,
    Close,
}

#[derive(Debug, Clone, Copy)]
pub struct StartInfo {
    /// Time spent opening the device (zero when it was already open).
    pub open: Duration,
    pub rate: u32,
}

#[derive(Clone)]
pub struct Control {
    tx: Sender<Cmd>,
}

/// The consumer side, owned by the engine thread.
pub struct Capture {
    ring: rtrb::Consumer<f32>,
    rate: Arc<AtomicU32>,
}

impl Capture {
    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Acquire)
    }

    /// Moves everything captured so far into `out` (device rate, mono).
    pub fn drain(&mut self, out: &mut Vec<f32>) {
        let n = self.ring.slots();
        if n == 0 {
            return;
        }
        if let Ok(chunk) = self.ring.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out.extend_from_slice(a);
            out.extend_from_slice(b);
            chunk.commit_all();
        }
    }

    pub fn discard(&mut self) {
        let n = self.ring.slots();
        if let Ok(chunk) = self.ring.read_chunk(n) {
            chunk.commit_all();
        }
    }
}

impl Control {
    /// Starts capturing; opens the device first if needed.
    pub fn start(&self, device: &str, keep_open: Duration) -> Result<StartInfo, String> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Start { device: device.to_string(), keep_open, reply })
            .map_err(|_| "audio thread gone".to_string())?;
        rx.recv().map_err(|_| "audio thread gone".to_string())?
    }

    pub fn stop(&self) {
        CAPTURING.store(false, Ordering::Release);
        let _ = self.tx.send(Cmd::Stop);
    }

    /// Closes the device (microphone changed in the config).
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

pub fn spawn() -> (Control, Capture) {
    let ring_len = 48_000 * RING_SECONDS;
    let (producer, consumer) = rtrb::RingBuffer::new(ring_len);
    let producer = Arc::new(Mutex::new(producer));
    let rate = Arc::new(AtomicU32::new(48_000));
    let (tx, rx) = mpsc::channel();
    let thread_rate = rate.clone();
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || control_loop(rx, producer, thread_rate))
        .expect("spawn audio thread");
    (Control { tx }, Capture { ring: consumer, rate })
}

struct Open {
    stream: cpal::Stream,
    device: String,
}

fn control_loop(rx: Receiver<Cmd>, producer: Arc<Mutex<rtrb::Producer<f32>>>, rate: Arc<AtomicU32>) {
    let mut open: Option<Open> = None;
    let mut close_at: Option<Instant> = None;
    let mut keep_open = Duration::ZERO;
    loop {
        let cmd = match close_at {
            Some(t) => match rx.recv_timeout(t.saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        match cmd {
            None | Some(Cmd::Close) => {
                if open.take().is_some() {
                    log::info!("microphone closed");
                }
                close_at = None;
            }
            Some(Cmd::Stop) => {
                if let Some(o) = &open {
                    let _ = o.stream.pause();
                }
                close_at = Some(Instant::now() + keep_open);
            }
            Some(Cmd::Start { device, keep_open: k, reply }) => {
                keep_open = k;
                close_at = None;
                if open.as_ref().is_some_and(|o| o.device != device) {
                    open = None;
                }
                let t0 = Instant::now();
                let mut opened = false;
                if open.is_none() {
                    match open_stream(&device, producer.clone(), rate.clone()) {
                        Ok(stream) => {
                            open = Some(Open { stream, device });
                            opened = true;
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                    }
                }
                let o = open.as_ref().unwrap();
                CAPTURING.store(true, Ordering::Release);
                let result = match o.stream.play() {
                    Ok(()) => Ok(StartInfo {
                        open: if opened { t0.elapsed() } else { Duration::ZERO },
                        rate: rate.load(Ordering::Acquire),
                    }),
                    Err(e) => {
                        CAPTURING.store(false, Ordering::Release);
                        open = None;
                        Err(format!("microphone start: {e}"))
                    }
                };
                if opened {
                    log::info!("microphone opened in {:?}", t0.elapsed());
                }
                let _ = reply.send(result);
            }
        }
    }
}

/// Lists input device names (for the log when the configured one is missing).
pub fn device_names() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.map(|d| d.to_string()).collect())
        .unwrap_or_default()
}

fn find_device(name: &str) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    if !name.is_empty() {
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if d.to_string() == name {
                    return Ok(d);
                }
            }
        }
        log::warn!("microphone {name:?} not found; using the default. Available: {:?}", device_names());
    }
    host.default_input_device().ok_or_else(|| "no microphone".to_string())
}

fn open_stream(
    name: &str,
    producer: Arc<Mutex<rtrb::Producer<f32>>>,
    rate: Arc<AtomicU32>,
) -> Result<cpal::Stream, String> {
    let device = find_device(name)?;
    let supported = device.default_input_config().map_err(|e| format!("microphone config: {e}"))?;
    let channels = supported.channels() as usize;
    rate.store(supported.sample_rate(), Ordering::Release);
    log::info!(
        "microphone {:?}: {} Hz, {} ch, {:?}",
        device.to_string(),
        supported.sample_rate(),
        channels,
        supported.sample_format()
    );
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, config, channels, producer),
        cpal::SampleFormat::I16 => build::<i16>(&device, config, channels, producer),
        cpal::SampleFormat::I32 => build::<i32>(&device, config, channels, producer),
        cpal::SampleFormat::U16 => build::<u16>(&device, config, channels, producer),
        other => return Err(format!("unsupported sample format {other}")),
    }?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    producer: Arc<Mutex<rtrb::Producer<f32>>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut mono = Vec::with_capacity(4096);
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                if !CAPTURING.load(Ordering::Acquire) {
                    return;
                }
                mono.clear();
                let scale = 1.0 / channels as f32;
                mono.extend(data.chunks_exact(channels).map(|f| f.iter().map(|s| f32::from_sample(*s)).sum::<f32>() * scale));
                if let Ok(mut p) = producer.try_lock() {
                    let n = mono.len().min(p.slots());
                    if let Ok(mut chunk) = p.write_chunk_uninit(n) {
                        let (a, b) = chunk.as_mut_slices();
                        for (dst, src) in a.iter_mut().chain(b.iter_mut()).zip(&mono) {
                            dst.write(*src);
                        }
                        unsafe { chunk.commit_all() };
                    }
                }
            },
            |e| log::warn!("microphone: {e}"),
            None,
        )
        .map_err(|e| format!("microphone open: {e}"))
}

/// Streaming low-pass + fractional resampler to 16 kHz.
pub struct Resampler {
    step: f64,
    taps: Vec<f32>,
    buf: Vec<f32>,
    pos: f64,
}

impl Resampler {
    pub fn new(in_rate: u32) -> Self {
        let step = in_rate as f64 / RATE as f64;
        if in_rate == RATE {
            return Resampler { step, taps: Vec::new(), buf: Vec::new(), pos: 0.0 };
        }
        // Windowed sinc, cutoff just under 8 kHz relative to the input rate.
        let n = ((step * 16.0).ceil() as usize).clamp(16, 128) | 1;
        let fc = 0.45 * RATE as f64 / in_rate as f64;
        let m = (n - 1) as f64;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let x = i as f64 - m / 2.0;
                let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / m).cos();
                (sinc * w) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        let half = n / 2;
        Resampler { step, taps, buf: vec![0.0; half], pos: half as f64 }
    }

    fn filtered(&self, center: usize) -> f32 {
        let half = self.taps.len() / 2;
        let start = center - half;
        self.taps.iter().zip(&self.buf[start..start + self.taps.len()]).map(|(t, s)| t * s).sum()
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.taps.is_empty() {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let half = self.taps.len() / 2;
        loop {
            let i = self.pos as usize;
            if i + 1 + half >= self.buf.len() {
                break;
            }
            let frac = (self.pos - i as f64) as f32;
            let a = self.filtered(i);
            let b = self.filtered(i + 1);
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        let consumed = (self.pos as usize).saturating_sub(half);
        if consumed > 0 {
            self.buf.drain(..consumed);
            self.pos -= consumed as f64;
        }
    }

    /// Emits what the filter still holds.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if !self.taps.is_empty() {
            let pad = vec![0.0; self.taps.len()];
            self.process(&pad, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize).map(|i| (i as f32 * hz * 2.0 * std::f32::consts::PI / rate as f32).sin()).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn length_matches_ratio() {
        for rate in [44_100, 48_000, 96_000] {
            let input = tone(rate, 440.0, 1.0);
            let mut r = Resampler::new(rate);
            let mut out = Vec::new();
            for chunk in input.chunks(441) {
                r.process(chunk, &mut out);
            }
            r.flush(&mut out);
            assert!((out.len() as i64 - 16_000).abs() < 40, "{rate}: {}", out.len());
        }
    }

    #[test]
    fn passes_speech_band_and_rejects_alias() {
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.process(&tone(48_000, 1000.0, 0.5), &mut out);
        assert!((rms(&out[400..]) - 0.707).abs() < 0.05);
        // 12 kHz would alias to 4 kHz without the low-pass.
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.process(&tone(48_000, 12_000.0, 0.5), &mut out);
        assert!(rms(&out[400..]) < 0.02, "{}", rms(&out[400..]));
    }

    #[test]
    fn passthrough_at_16k() {
        let mut r = Resampler::new(16_000);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }
}
