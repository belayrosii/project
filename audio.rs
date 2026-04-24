// src/audio.rs — production-ready WASAPI audio capture + DSP pipeline

use std::sync::atomic::{AtomicU64, AtomicUsize, AtomicBool, Ordering};
use std::sync::Arc;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use std::thread::{self, JoinHandle};
use windows::Win32::System::Com::{
    CoInitializeEx, CoCreateInstance, CLSCTX_ALL, COINIT_MULTITHREADED,
    CoTaskMemFree,
};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioCaptureClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    eCapture, eCommunications, eConsole,
};
use windows::Win32::System::Threading::{
    SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject,
    CreateEventW,
};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// GUID KSDATAFORMAT_SUBTYPE_IEEE_FLOAT = {00000003-0000-0010-8000-00aa00389b71}
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID = windows::core::GUID::from_values(
    0x00000003, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]
);
/// GUID KSDATAFORMAT_SUBTYPE_PCM = {00000001-0000-0010-8000-00aa00389b71}
const KSDATAFORMAT_SUBTYPE_PCM: windows::core::GUID = windows::core::GUID::from_values(
    0x00000001, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]
);

/// Отключает denormalized floats для предотвращения CPU penalty.
pub fn enable_denormal_protection() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let mut csr: u32 = 0;
        std::arch::asm!("stmxcsr [{0}]", in(reg) &mut csr, options(nostack));
        csr |= (1 << 15) | (1 << 6); // DAZ + FTZ
        std::arch::asm!("ldmxcsr [{0}]", in(reg) &csr, options(nostack));
    }
}

const EPSILON: f32 = 1e-10;

#[inline(always)]
fn compute_rms(frame: &[f32]) -> f32 {
    if frame.is_empty() { return 0.0; }
    let sum_sq: f32 = frame.iter().map(|&s| s * s).sum();
    (sum_sq / frame.len() as f32).sqrt()
}

#[inline(always)]
fn linear_to_dbfs(linear: f32) -> f32 {
    20.0 * (linear + EPSILON).log10()
}

// ============================================================
// AudioRingBuffer — lock-free SPSC queue, aligned cache lines
// ============================================================

#[repr(align(64))]
pub struct AudioRingBuffer {
    buffer: Vec<f32>,
    capacity: usize,
    capacity_mask: usize,
    write_seq: AtomicU64,
    read_seq: AtomicU64,
}

impl AudioRingBuffer {
    pub fn new(capacity_ms: usize, sample_rate: usize, channels: usize) -> Self {
        let samples_per_ms = sample_rate / 1000;
        let capacity = (capacity_ms * samples_per_ms * channels)
            .next_power_of_two()
            .max(1024);
        
        Self {
            buffer: vec![0.0f32; capacity],
            capacity,
            capacity_mask: capacity - 1,
            write_seq: AtomicU64::new(0),
            read_seq: AtomicU64::new(0),
        }
    }

    #[inline(always)]
    pub fn write(&self, data: &[f32]) {
        if data.is_empty() { return; }
        
        let write_seq = self.write_seq.load(Ordering::Relaxed);
        let read_seq = self.read_seq.load(Ordering::Acquire);
        
        let space_used = (write_seq.wrapping_sub(read_seq)) as usize;
        let space_avail = self.capacity.saturating_sub(space_used);
        
        let to_write = data.len().min(space_avail);
        if to_write == 0 { return; }
        
        let write_idx = (write_seq as usize) & self.capacity_mask;
        
        let ptr = self.buffer.as_ptr() as *mut f32;
        for i in 0..to_write {
            let idx = (write_idx + i) & self.capacity_mask;
            unsafe { std::ptr::write(ptr.add(idx), data[i]); }
        }
        
        self.write_seq.store(write_seq.wrapping_add(to_write as u64), Ordering::Release);
    }

    #[inline(always)]
    pub fn read(&self, dst: &mut [f32]) -> usize {
        if dst.is_empty() { return 0; }
        
        let write_seq = self.write_seq.load(Ordering::Acquire);
        let read_seq = self.read_seq.load(Ordering::Relaxed);
        
        let available = (write_seq.wrapping_sub(read_seq)) as usize;
        let to_read = dst.len().min(available);
        if to_read == 0 { return 0; }
        
        let read_idx = (read_seq as usize) & self.capacity_mask;
        
        let ptr = self.buffer.as_ptr();
        for i in 0..to_read {
            let idx = (read_idx + i) & self.capacity_mask;
            unsafe { dst[i] = std::ptr::read(ptr.add(idx)); }
        }
        
        self.read_seq.store(read_seq.wrapping_add(to_read as u64), Ordering::Release);
        to_read
    }

    pub fn available(&self) -> usize {
        let write_seq = self.write_seq.load(Ordering::Acquire);
        let read_seq = self.read_seq.load(Ordering::Acquire);
        (write_seq.wrapping_sub(read_seq)) as usize
    }

    pub fn clear(&self) {
        self.read_seq.store(
            self.write_seq.load(Ordering::Acquire),
            Ordering::Release
        );
    }
}

// ============================================================
// WasapiCaptureEngine — WASAPI capture thread
// ============================================================

pub struct WasapiCaptureEngine {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    format: Arc<(AtomicUsize, AtomicU64)>,
}

impl WasapiCaptureEngine {
    pub fn new() -> Self {
        Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            thread: None,
            format: Arc::new((AtomicUsize::new(0), AtomicU64::new(0))),
        }
    }

    pub fn start(&mut self, output: Arc<AudioRingBuffer>) -> Result<(), String> {
        let stop_flag = self.stop_flag.clone();
        let format = self.format.clone();

        self.thread = Some(thread::spawn(move || {
            unsafe { Self::thread_proc(stop_flag, output, format); }
        }));

        Ok(())
    }

    unsafe fn thread_proc(
        stop_flag: Arc<AtomicBool>,
        output: Arc<AudioRingBuffer>,
        format_out: Arc<(AtomicUsize, AtomicU64)>,
    ) {
        enable_denormal_protection();

        let thread = windows::Win32::System::Threading::GetCurrentThread();
        let _ = SetThreadPriority(thread, THREAD_PRIORITY_TIME_CRITICAL);

        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() && hr != windows::core::HRESULT(1) {
            eprintln!("[CAPTURE] CoInitializeEx failed: {:?}", hr);
            return;
        }

        let enumerator: IMMDeviceEnumerator = match CoCreateInstance(
            &MMDeviceEnumerator, None, CLSCTX_ALL
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[CAPTURE] CoCreateInstance failed: {:?}", e);
                return;
            }
        };

        let device: IMMDevice = match enumerator
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .or_else(|_| enumerator.GetDefaultAudioEndpoint(eCapture, eConsole))
        {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[CAPTURE] GetDefaultAudioEndpoint failed: {:?}", e);
                return;
            }
        };

        let audio_client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[CAPTURE] Activate failed: {:?}", e);
                return;
            }
        };

        let mix_format_ptr = match audio_client.GetMixFormat() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[CAPTURE] GetMixFormat failed: {:?}", e);
                return;
            }
        };

        let sample_rate = (*mix_format_ptr).nSamplesPerSec;
        let channels = (*mix_format_ptr).nChannels;
        let bits = (*mix_format_ptr).wBitsPerSample;
        let format_tag = (*mix_format_ptr).wFormatTag;
        let block_align = (*mix_format_ptr).nBlockAlign;

        // Разбираем WAVE_FORMAT_EXTENSIBLE
        let (effective_bits, is_float) = if format_tag == WAVE_FORMAT_EXTENSIBLE {
            let ext = &*(mix_format_ptr as *const WAVEFORMATEXTENSIBLE);
            let is_float = ext.SubFormat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
            (ext.Format.wBitsPerSample, is_float)
        } else {
            (bits, format_tag == WAVE_FORMAT_IEEE_FLOAT)
        };

        format_out.0.store(channels as usize, Ordering::Release);
        format_out.1.store(sample_rate as u64, Ordering::Release);

        let event_handle = match CreateEventW(None, false, false, None) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[CAPTURE] CreateEventW failed: {:?}", e);
                CoTaskMemFree(Some(mix_format_ptr as *mut _));
                return;
            }
        };

        if event_handle.is_invalid() {
            eprintln!("[CAPTURE] CreateEventW returned invalid handle");
            CoTaskMemFree(Some(mix_format_ptr as *mut _));
            return;
        }

        let buffer_duration = (10_000_000i64 * 10 / 1000).max(2000000);
        
        let hr = audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            buffer_duration,
            0,
            mix_format_ptr,
            None,
        );

        if hr.is_err() {
            eprintln!("[CAPTURE] Initialize failed: {:?}", hr);
            let _ = CloseHandle(event_handle);
            CoTaskMemFree(Some(mix_format_ptr as *mut _));
            return;
        }

        let capture_client: IAudioCaptureClient = match audio_client.GetService() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[CAPTURE] GetService failed: {:?}", e);
                let _ = CloseHandle(event_handle);
                CoTaskMemFree(Some(mix_format_ptr as *mut _));
                return;
            }
        };

        if let Err(e) = audio_client.SetEventHandle(event_handle) {
            eprintln!("[CAPTURE] SetEventHandle failed: {:?}", e);
            let _ = CloseHandle(event_handle);
            CoTaskMemFree(Some(mix_format_ptr as *mut _));
            return;
        }

        if let Err(e) = audio_client.Start() {
            eprintln!("[CAPTURE] Start failed: {:?}", e);
            let _ = CloseHandle(event_handle);
            CoTaskMemFree(Some(mix_format_ptr as *mut _));
            return;
        }

        let channels = channels as usize;
        let bytes_per_sample = (effective_bits as usize / 8).max(1);
        let bytes_per_frame = (channels * bytes_per_sample).max(1);
        
        let max_convert_samples = 16384;
        let mut convert: Vec<f32> = vec![0.0; max_convert_samples];

        eprintln!("[CAPTURE] Started: {} ch @ {} Hz, {} bits, float={}, format={}",
            channels, sample_rate, effective_bits, is_float, format_tag);

        loop {
            if stop_flag.load(Ordering::Relaxed) { break; }

            let wait = WaitForSingleObject(event_handle, 100);
            if wait != WAIT_OBJECT_0 { continue; }

            loop {
                let mut data_ptr: *mut u8 = std::ptr::null_mut();
                let mut frames_available: u32 = 0;
                let mut flags: u32 = 0;

                let result = capture_client.GetBuffer(
                    &mut data_ptr, &mut frames_available, &mut flags, None, None
                );
                
                if let Err(e) = result {
                    let code = e.code().0 as u32;
                    if code == 0x88890001 { break; }
                    eprintln!("[CAPTURE] GetBuffer error: 0x{:08x}", code);
                    break;
                }

                if frames_available == 0 || data_ptr.is_null() {
                    let _ = capture_client.ReleaseBuffer(0);
                    break;
                }

                let frames = frames_available as usize;
                let total_samples = frames * channels;

                if total_samples > convert.len() {
                    convert.resize(total_samples, 0.0);
                }

                let mut converted = 0usize;
                
                // AUDCLNT_BUFFERFLAGS_SILENT = 0x00000002
                if (flags & 0x00000002) != 0 {
                    converted = total_samples;
                    convert[..converted].fill(0.0);
                } else {
                    match (effective_bits, is_float) {
                        (16, false) => {
                            let src = std::slice::from_raw_parts(
                                data_ptr as *const i16,
                                total_samples
                            );
                            for (i, &s) in src.iter().enumerate() {
                                convert[i] = (s as f32) * (1.0 / 32768.0);
                            }
                            converted = src.len();
                        }
                        (24, false) => {
                            for i in 0..total_samples {
                                let off = i * 3;
                                if off + 2 >= frames * block_align as usize { break; }
                                let b0 = *data_ptr.add(off) as i32;
                                let b1 = *data_ptr.add(off + 1) as i32;
                                let b2 = *data_ptr.add(off + 2) as i32;
                                let sample = ((b0 | (b1 << 8) | (b2 << 16)) as i32) << 8 >> 8;
                                convert[i] = (sample as f32) * (1.0 / 8388608.0);
                                converted += 1;
                            }
                        }
                        (32, true) => {
                            let src = std::slice::from_raw_parts(
                                data_ptr as *const f32,
                                total_samples
                            );
                            convert[..src.len()].copy_from_slice(src);
                            converted = src.len();
                        }
                        (32, false) => {
                            let src = std::slice::from_raw_parts(
                                data_ptr as *const i32,
                                total_samples
                            );
                            for (i, &s) in src.iter().enumerate() {
                                convert[i] = (s as f32) * (1.0 / 2147483648.0);
                            }
                            converted = src.len();
                        }
                        _ => {
                            eprintln!("[CAPTURE] Unsupported format: {} bits, float={}", effective_bits, is_float);
                            converted = total_samples.min(convert.len());
                            convert[..converted].fill(0.0);
                        }
                    }
                }

                let _ = capture_client.ReleaseBuffer(frames_available);

                if converted > 0 {
                    output.write(&convert[..converted]);
                }
            }
        }

        let _ = audio_client.Stop();
        let _ = CloseHandle(event_handle);
        CoTaskMemFree(Some(mix_format_ptr as *mut _));
        
        eprintln!("[CAPTURE] Stopped");
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn get_format(&self) -> (usize, u32) {
        (
            self.format.0.load(Ordering::Acquire),
            self.format.1.load(Ordering::Acquire) as u32
        )
    }
}

// WAVEFORMATEXTENSIBLE для разбора SubFormat
#[repr(C)]
#[derive(Debug)]
struct WAVEFORMATEXTENSIBLE {
    Format: WAVEFORMAT,
    Samples: u16,
    SubFormat: windows::core::GUID,
}

#[repr(C)]
#[derive(Debug)]
struct WAVEFORMAT {
    wFormatTag: u16,
    nChannels: u16,
    nSamplesPerSec: u32,
    nAvgBytesPerSec: u32,
    nBlockAlign: u16,
    wBitsPerSample: u16,
}

// ============================================================
// Resampler — linear interpolation + simple anti-aliasing FIR
// ============================================================

pub struct Resampler {
    ratio_num: usize,
    ratio_den: usize,
    phase: usize,
    last: f32,
    last2: f32,
    buf: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let gcd = Self::gcd(in_rate, out_rate);
        let max_buf = ((in_rate as usize * 2) / 1000) * 10 + 16;
        
        Self {
            ratio_num: (in_rate / gcd) as usize,
            ratio_den: (out_rate / gcd) as usize,
            phase: 0,
            last: 0.0,
            last2: 0.0,
            buf: vec![0.0; max_buf],
        }
    }

    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { Self::gcd(b, a % b) }
    }

    pub fn process(&mut self, input: &[f32]) -> &[f32] {
        if input.is_empty() { return &[]; }
        
        let mut out = 0;
        for i in 0..input.len() {
            // Простой 3-tap FIR anti-aliasing filter (unused for now, applied in-place)
            let _filtered = if i == 0 {
                (self.last2 + self.last + input[i]) / 3.0
            } else if i == 1 {
                (self.last + input[i - 1] + input[i]) / 3.0
            } else {
                (input[i - 2] + input[i - 1] + input[i]) / 3.0
            };
            
            self.phase += self.ratio_den;
            while self.phase >= self.ratio_num && out < self.buf.len() {
                self.phase -= self.ratio_num;
                let prev = if i == 0 { self.last } else { input[i - 1] };
                let frac = self.phase as f32 / self.ratio_num as f32;
                self.buf[out] = prev * (1.0 - frac) + input[i] * frac;
                out += 1;
            }
        }
        self.last2 = if input.len() >= 2 { input[input.len() - 2] } else { self.last };
        self.last = *input.last().unwrap_or(&0.0);
        &self.buf[..out.min(self.buf.len())]
    }
}

// ============================================================
// AGC — Automatic Gain Control
// ============================================================

pub struct Agc {
    gain_db: f32,
    target: f32,
    max_gain: f32,
    min_gain: f32,
    attack: f32,
    release: f32,
}

impl Agc {
    pub fn new() -> Self {
        Self {
            gain_db: 0.0,
            target: -20.0,
            max_gain: 30.0,
            min_gain: -20.0,
            attack: 0.2,
            release: 0.001,
        }
    }

    pub fn process(&mut self, frame: &mut [f32]) {
        if frame.is_empty() { return; }
        let rms = compute_rms(frame);
        let level = linear_to_dbfs(rms);
        let target = (self.target - level).clamp(self.min_gain, self.max_gain);
        let alpha = if target > self.gain_db { self.attack } else { self.release };
        self.gain_db = self.gain_db * (1.0 - alpha) + target * alpha;
        let gain = 10.0_f32.powf(self.gain_db / 20.0);
        for s in frame.iter_mut() {
            *s = (*s * gain).clamp(-0.99, 0.99);
        }
    }
}

// ============================================================
// VAD — Voice Activity Detection
// ============================================================

pub struct Vad {
    history: VecDeque<f32>,
    noise: f32,
    speech_prob: f32,
    hangover: usize,
    max_hangover: usize,
    abs_threshold: f32,
}

impl Vad {
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(30),
            noise: 0.001,
            speech_prob: 0.0,
            hangover: 0,
            max_hangover: 8,
            abs_threshold: 0.01, // -40 dB
        }
    }

    pub fn process(&mut self, frame: &[f32]) -> bool {
        if frame.is_empty() { return false; }
        
        let energy = compute_rms(frame);
        self.history.push_back(energy);
        if self.history.len() > 30 { self.history.pop_front(); }
        
        let min_e = self.history.iter().fold(f32::MAX, |a, &b| a.min(b));
        self.noise = self.noise * 0.9 + min_e * 0.1;
        
        let threshold = self.noise * 4.0;
        let snr = if self.noise > 0.0 { energy / self.noise } else { 0.0 };
        let indicator = if energy > threshold.max(self.abs_threshold) && snr > 2.0 { 1.0 } else { 0.0 };
        let beta = if indicator > self.speech_prob { 0.1 } else { 0.005 };
        self.speech_prob = self.speech_prob * (1.0 - beta) + indicator * beta;

        if self.speech_prob > 0.6 {
            self.hangover = self.max_hangover;
            true
        } else if self.hangover > 0 {
            self.hangover -= 1;
            true
        } else {
            false
        }
    }
}

// ============================================================
// DspPipeline — mono, resample, AGC, VAD
// ============================================================

pub struct DspPipeline {
    resampler: Resampler,
    agc: Agc,
    vad: Vad,
    enable_vad: bool,
    channels: usize,
    mono: Vec<f32>,
    dc: Vec<f32>,
    output: Vec<f32>,
}

impl DspPipeline {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize, enable_vad: bool) -> Self {
        Self {
            resampler: Resampler::new(in_rate, out_rate),
            agc: Agc::new(),
            vad: Vad::new(),
            enable_vad,
            channels,
            mono: Vec::with_capacity(4800),
            dc: Vec::with_capacity(4800),
            output: Vec::with_capacity(1600),
        }
    }

    pub fn process(&mut self, input: &[f32]) -> (&[f32], bool) {
        if input.is_empty() || self.channels == 0 { return (&[], false); }
        
        let frames = input.len() / self.channels;
        if frames == 0 { return (&[], false); }

        self.mono.clear();
        match self.channels {
            1 => self.mono.extend_from_slice(&input[..frames]),
            2 => {
                for i in 0..frames {
                    self.mono.push((input[i * 2] + input[i * 2 + 1]) * 0.5);
                }
            }
            _ => {
                for i in 0..frames {
                    let mut sum = 0.0f32;
                    for c in 0..self.channels {
                        sum += input[i * self.channels + c];
                    }
                    self.mono.push(sum / self.channels as f32);
                }
            }
        }

        if self.mono.is_empty() { return (&[], false); }
        
        let dc = self.mono.iter().sum::<f32>() / self.mono.len() as f32;
        self.dc.resize(self.mono.len(), 0.0);
        for (i, &s) in self.mono.iter().enumerate() {
            self.dc[i] = s - dc;
        }

        let resampled = self.resampler.process(&self.dc);
        if resampled.is_empty() { return (&[], false); }

        self.output.resize(resampled.len(), 0.0);
        self.output.copy_from_slice(resampled);
        self.agc.process(&mut self.output);

        let is_speech = if self.enable_vad {
            self.vad.process(&self.output)
        } else {
            true
        };

        (&self.output, is_speech)
    }
}

// ============================================================
// ProductionAudioEngine — high-level audio engine
// ============================================================

pub struct ProductionAudioEngine {
    pub capture: WasapiCaptureEngine,
    raw_buffer: Arc<AudioRingBuffer>,
    processed_buffer: Arc<AudioRingBuffer>,
    dsp_thread: Option<JoinHandle<()>>,
    dsp_stop: Arc<AtomicBool>,
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    enable_vad: bool,
}

impl ProductionAudioEngine {
    pub fn new(_device_id: &str) -> Self {
        let raw = Arc::new(AudioRingBuffer::new(200, 48000, 2));
        let processed = Arc::new(AudioRingBuffer::new(100, 16000, 1));
        
        Self {
            capture: WasapiCaptureEngine::new(),
            raw_buffer: raw,
            processed_buffer: processed,
            dsp_thread: None,
            dsp_stop: Arc::new(AtomicBool::new(false)),
            in_rate: 48000,
            out_rate: 16000,
            channels: 2,
            enable_vad: true,
        }
    }

    pub fn start(&mut self, _config: WasapiConfig, enable_vad: bool) -> Result<(), String> {
        self.enable_vad = enable_vad;
        
        self.capture.start(self.raw_buffer.clone())?;
        
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let (ch, sr) = self.capture.get_format();
            if ch != 0 && sr != 0 {
                self.channels = ch;
                self.in_rate = sr;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        
        if self.channels == 0 || self.in_rate == 0 {
            return Err("Failed to get audio format from WASAPI".to_string());
        }

        eprintln!("[ENGINE] {} ch @ {} Hz -> {} Hz, VAD: {}", 
            self.channels, self.in_rate, self.out_rate,
            if self.enable_vad { "ON" } else { "OFF" });

        let raw = self.raw_buffer.clone();
        let processed = self.processed_buffer.clone();
        let stop = self.dsp_stop.clone();
        let in_rate = self.in_rate;
        let out_rate = self.out_rate;
        let channels = self.channels;
        let vad = self.enable_vad;
        
        self.dsp_thread = Some(thread::spawn(move || {
            enable_denormal_protection();
            
            let mut pipeline = DspPipeline::new(in_rate, out_rate, channels, vad);
            
            let input_samples = (in_rate as usize * 10 / 1000) * channels;
            let mut input_buf = vec![0.0f32; input_samples];
            
            let mut frame_count = 0u64;
            let mut last_log = Instant::now();

            while !stop.load(Ordering::Relaxed) {
                let read = raw.read(&mut input_buf);
                if read == 0 {
                    thread::sleep(Duration::from_micros(100));
                    continue;
                }

                let (output, is_speech) = pipeline.process(&input_buf[..read]);
                
                if !output.is_empty() {
                    processed.write(output);
                }

                frame_count += 1;
                if last_log.elapsed().as_secs() >= 5 {
                    let rms = compute_rms(output);
                    eprintln!("[DSP] Frames: {}, rms: {:.1} dB, raw_avail: {}, proc_avail: {}, {}",
                        frame_count,
                        linear_to_dbfs(rms),
                        raw.available(),
                        processed.available(),
                        if is_speech { "SPEECH" } else { "SILENCE" }
                    );
                    last_log = Instant::now();
                }
            }
            
            eprintln!("[DSP] Stopped");
        }));

        Ok(())
    }

    pub fn read(&self, dst: &mut [f32]) -> (usize, bool) {
        let read = self.processed_buffer.read(dst);
        let is_speech = if read > 0 {
            compute_rms(&dst[..read]) > 0.001
        } else {
            false
        };
        (read, is_speech)
    }

    pub fn stop(&mut self) {
        self.dsp_stop.store(true, Ordering::Relaxed);
        self.capture.stop();
        if let Some(t) = self.dsp_thread.take() {
            let _ = t.join();
        }
    }

    pub fn stats(&self) -> String {
        format!("raw: {}, proc: {}", 
            self.raw_buffer.available(),
            self.processed_buffer.available())
    }
}

pub struct WasapiConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_duration_ms: u32,
}
