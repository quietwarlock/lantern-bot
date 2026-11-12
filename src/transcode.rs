//! Web player audio (1.11): anything Lantern can play, re-encoded as one Ogg
//! Opus stream a browser's `<audio>` element plays.
//!
//! The source is opened exactly like a Discord song ([`crate::media::Playable::input`]
//! and Songbird's Symphonia registries), so YouTube, SoundCloud, library files and
//! every radio format 1.10.1 handles (HLS, AAC, MP3, Ogg, .pls/.m3u) play the same
//! way. The decoded audio is mixed to stereo, resampled to 48 kHz, encoded with
//! libopus (already linked for Discord) and wrapped in Ogg pages here.

use rubato::{FftFixedInOut, Resampler};
use songbird::input::Parsed;
use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Signal};
use symphonia::core::errors::Error as SymphError;
use symphonia::core::formats::{SeekMode, SeekTo};
use symphonia::core::units::Time;

pub const RATE: u32 = 48_000;
/// 20 ms of audio per Opus packet.
const FRAME: usize = 960;
/// Packets per Ogg page: 100 ms, so a live station starts quickly.
const PAGE_PACKETS: usize = 5;
const BITRATE: i32 = 160_000;
pub const CONTENT_TYPE: &str = "audio/ogg; codecs=opus";

// ------------------------------------------------------------------ Ogg pages

fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04c1_1db7 } else { r << 1 };
            }
            *slot = r;
        }
        table
    })
}

/// The Ogg checksum (CRC-32, polynomial 0x04c11db7, not reflected, starting at 0).
pub fn ogg_crc(bytes: &[u8]) -> u32 {
    let table = crc_table();
    bytes.iter().fold(0u32, |crc, b| (crc << 8) ^ table[(((crc >> 24) as u8) ^ b) as usize])
}

/// Writes one logical Ogg stream.
pub struct OggWriter {
    serial: u32,
    sequence: u32,
}

pub const BOS: u8 = 0x02;
pub const EOS: u8 = 0x04;

impl OggWriter {
    pub fn new(serial: u32) -> Self {
        Self { serial, sequence: 0 }
    }

    /// One page holding whole packets (each under 255 * 255 bytes in total).
    pub fn page(&mut self, flags: u8, granule: u64, packets: &[Vec<u8>]) -> Vec<u8> {
        let mut lacing = Vec::new();
        for packet in packets {
            let mut left = packet.len();
            while left >= 255 {
                lacing.push(255u8);
                left -= 255;
            }
            lacing.push(left as u8);
        }
        debug_assert!(lacing.len() <= 255);
        let mut page = Vec::with_capacity(27 + lacing.len() + packets.iter().map(Vec::len).sum::<usize>());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        page.extend_from_slice(&[0, 0, 0, 0]);
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        for packet in packets {
            page.extend_from_slice(packet);
        }
        let crc = ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        self.sequence += 1;
        page
    }
}

// --------------------------------------------------------------- Opus encoder

/// 48 kHz stereo samples in, Ogg Opus bytes out.
pub struct OggOpus {
    encoder: opus2::Encoder,
    ogg: OggWriter,
    pre_skip: u64,
    pending: Vec<f32>,
    packets: Vec<Vec<u8>>,
    /// Opus frames encoded so far, times 960.
    encoded: u64,
    /// Real samples (per channel) fed in, for the final page's end trim.
    fed: u64,
    out: Vec<u8>,
    finishing: bool,
}

impl OggOpus {
    pub fn new() -> Result<Self, String> {
        let mut encoder = opus2::Encoder::new(RATE, opus2::Channels::Stereo, opus2::Application::Audio).map_err(|e| e.to_string())?;
        let _ = encoder.set_bitrate(opus2::Bitrate::Bits(BITRATE));
        let pre_skip = encoder.get_lookahead().unwrap_or(312).max(0) as u64;
        let mut ogg = OggWriter::new(fastrand::u32(..));
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(2); // channels
        head.extend_from_slice(&(pre_skip as u16).to_le_bytes());
        head.extend_from_slice(&RATE.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // mapping family: mono/stereo
        let vendor = format!("Lantern {}", crate::VERSION);
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor.as_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        let mut out = ogg.page(BOS, 0, &[head]);
        out.extend(ogg.page(0, 0, &[tags]));
        Ok(Self { encoder, ogg, pre_skip, pending: Vec::new(), packets: Vec::new(), encoded: 0, fed: 0, out, finishing: false })
    }

    /// Interleaved stereo at 48 kHz.
    pub fn push(&mut self, stereo: &[f32]) -> Result<(), String> {
        self.fed += (stereo.len() / 2) as u64;
        self.pending.extend_from_slice(stereo);
        let mut start = 0;
        while self.pending.len() - start >= FRAME * 2 {
            let frame: Vec<f32> = self.pending[start..start + FRAME * 2].to_vec();
            self.encode(&frame)?;
            start += FRAME * 2;
        }
        self.pending.drain(..start);
        Ok(())
    }

    fn encode(&mut self, frame: &[f32]) -> Result<(), String> {
        let mut buf = vec![0u8; 4000];
        let n = self.encoder.encode_float(frame, &mut buf).map_err(|e| format!("Opus: {e}"))?;
        buf.truncate(n);
        self.packets.push(buf);
        self.encoded += FRAME as u64;
        if self.packets.len() >= PAGE_PACKETS && !self.finishing {
            let packets = std::mem::take(&mut self.packets);
            let page = self.ogg.page(0, self.encoded, &packets);
            self.out.extend(page);
        }
        Ok(())
    }

    /// Bytes ready to send.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Pads and ends the stream; the last page's position trims the padding.
    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        let real_end = self.pre_skip + self.fed;
        self.finishing = true;
        while !self.pending.is_empty() || self.encoded < real_end || self.packets.is_empty() {
            let mut frame = std::mem::take(&mut self.pending);
            frame.resize(FRAME * 2, 0.0);
            self.encode(&frame)?;
        }
        let packets = std::mem::take(&mut self.packets);
        let page = self.ogg.page(EOS, real_end, &packets);
        self.out.extend(page);
        Ok(self.out)
    }
}

// ------------------------------------------------------- decode and resample

/// Turns decoded buffers of any rate and layout into 48 kHz interleaved stereo.
#[derive(Default)]
pub struct ToStereo48 {
    resampler: Option<(u32, FftFixedInOut<f32>)>,
    waiting: [Vec<f32>; 2],
    pub gain: f32,
}

impl ToStereo48 {
    pub fn new(gain: f32) -> Self {
        Self { gain, ..Self::default() }
    }

    /// Planar left/right at `rate`; returns interleaved stereo at 48 kHz.
    pub fn push(&mut self, rate: u32, left: &[f32], right: &[f32]) -> Vec<f32> {
        let gain = if self.gain > 0.0 { self.gain } else { 1.0 };
        if rate == RATE || rate == 0 {
            if !self.waiting[0].is_empty() {
                // Rate changed back to 48 kHz: drop the resampler's leftovers.
                self.resampler = None;
                self.waiting = [Vec::new(), Vec::new()];
            }
            return interleave(left, right, gain);
        }
        if self.resampler.as_ref().is_none_or(|(r, _)| *r != rate) {
            self.waiting = [Vec::new(), Vec::new()];
            self.resampler = FftFixedInOut::<f32>::new(rate as usize, RATE as usize, 1024, 2).ok().map(|r| (rate, r));
        }
        let Some((_, resampler)) = self.resampler.as_mut() else { return Vec::new() };
        self.waiting[0].extend_from_slice(left);
        self.waiting[1].extend_from_slice(right);
        let mut out = Vec::new();
        loop {
            let need = resampler.input_frames_next();
            if self.waiting[0].len() < need {
                break;
            }
            let chunk = [self.waiting[0][..need].to_vec(), self.waiting[1][..need].to_vec()];
            if let Ok(done) = resampler.process(&chunk, None) {
                out.extend(interleave(&done[0], &done[1], gain));
            }
            self.waiting[0].drain(..need);
            self.waiting[1].drain(..need);
        }
        out
    }
}

fn interleave(left: &[f32], right: &[f32], gain: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(left.len() * 2);
    for (l, r) in left.iter().zip(right) {
        out.push((l * gain).clamp(-1.0, 1.0));
        out.push((r * gain).clamp(-1.0, 1.0));
    }
    out
}

/// Left and right of a decoded buffer (mono is doubled; extra channels are left out).
pub fn stereo_of(decoded: AudioBufferRef<'_>) -> (u32, Vec<f32>, Vec<f32>) {
    let spec = *decoded.spec();
    let mut buffer: AudioBuffer<f32> = AudioBuffer::new(decoded.capacity() as u64, spec);
    decoded.convert(&mut buffer);
    let channels = spec.channels.count();
    let left = if channels > 0 { buffer.chan(0).to_vec() } else { Vec::new() };
    let right = if channels > 1 { buffer.chan(1).to_vec() } else { left.clone() };
    (spec.rate, left, right)
}

/// Why a transcode ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The source ran out.
    Finished,
    /// The listener went away.
    Gone,
}

/// Decodes `parsed` from `start` seconds and sends Ogg Opus bytes to `send`
/// until the source ends or `send` returns false. Blocking: run it on its own thread.
pub fn run(mut parsed: Parsed, start: f64, gain: f32, mut send: impl FnMut(Vec<u8>) -> bool) -> Result<Ended, String> {
    let mut encoder = OggOpus::new()?;
    if !send(encoder.take()) {
        return Ok(Ended::Gone);
    }
    if start > 0.5 {
        let to = SeekTo::Time { time: Time::from(start), track_id: Some(parsed.track_id) };
        match parsed.format.seek(SeekMode::Coarse, to) {
            Ok(_) => parsed.decoder.reset(),
            Err(err) => return Err(format!("couldn't skip to {start:.0} s: {err}")),
        }
    }
    let mut pcm = ToStereo48::new(gain);
    let mut decoded_any = false;
    let mut bad_packets = 0u32;
    loop {
        let packet = match parsed.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphError::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphError::ResetRequired) => {
                parsed.decoder.reset();
                continue;
            }
            // The stream broke off after playing: end it like a finished song.
            Err(_) if decoded_any => break,
            Err(err) => return Err(format!("couldn't read the audio: {err}")),
        };
        if packet.track_id() != parsed.track_id {
            continue;
        }
        match parsed.decoder.decode(&packet) {
            Ok(decoded) => {
                bad_packets = 0;
                decoded_any = true;
                let (rate, left, right) = stereo_of(decoded);
                let stereo = pcm.push(rate, &left, &right);
                encoder.push(&stereo)?;
            }
            Err(SymphError::DecodeError(_)) => {
                bad_packets += 1;
                if bad_packets > 200 {
                    return Err("the audio couldn't be decoded".into());
                }
                continue;
            }
            Err(SymphError::ResetRequired) => {
                parsed.decoder.reset();
                continue;
            }
            Err(err) => return Err(format!("couldn't decode the audio: {err}")),
        }
        let bytes = encoder.take();
        if !bytes.is_empty() && !send(bytes) {
            return Ok(Ended::Gone);
        }
    }
    if !decoded_any {
        return Err("the source sent no audio Lantern could play".into());
    }
    let tail = encoder.finish()?;
    send(tail);
    Ok(Ended::Finished)
}

/// A 440 Hz tone as a WAV file, for tests.
#[cfg(test)]
pub(crate) fn test_wav(rate: u32, channels: u16, seconds: f32) -> Vec<u8> {
    let frames = (rate as f32 * seconds) as usize;
    let mut data = Vec::new();
    for i in 0..frames {
        let s = ((i as f32 / rate as f32) * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        for _ in 0..channels {
            data.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    #[test]
    fn the_ogg_checksum_matches_the_spec() {
        // CRC-32/MPEG-2 style without the final xor, as Ogg uses it.
        assert_eq!(ogg_crc(b""), 0);
        assert_eq!(ogg_crc(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn long_packets_are_laced_in_255_byte_steps() {
        let mut ogg = OggWriter::new(7);
        let page = ogg.page(BOS, 0, &[vec![1u8; 255], vec![2u8; 10]]);
        assert_eq!(&page[..4], b"OggS");
        assert_eq!(page[5], BOS);
        assert_eq!(page[26], 3); // 255, 0, 10
        assert_eq!(&page[27..30], &[255, 0, 10]);
        assert_eq!(page.len(), 27 + 3 + 265);
        let second = ogg.page(0, 960, &[vec![3u8; 1]]);
        assert_eq!(u32::from_le_bytes(second[18..22].try_into().unwrap()), 1, "page sequence counts up");
        let mut zeroed = second.clone();
        zeroed[22..26].copy_from_slice(&[0; 4]);
        assert_eq!(u32::from_le_bytes(second[22..26].try_into().unwrap()), ogg_crc(&zeroed));
    }

    use super::test_wav as wav;

    fn parse(bytes: Vec<u8>, ext: &str) -> Parsed {
        let stream = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(ext);
        let probed = get_probe().format(&hint, stream, &Default::default(), &Default::default()).expect("probe");
        let format = probed.format;
        let track = format.default_track().expect("track").clone();
        let decoder = get_codec_registry().make(&track.codec_params, &Default::default()).expect("decoder");
        Parsed { format, decoder, track_id: track.id, meta: probed.metadata, supports_backseek: true }
    }

    /// Decodes Ogg Opus bytes back to PCM: (samples per channel, peak level).
    fn decode_back(ogg: Vec<u8>) -> (u64, f32) {
        let mut parsed = parse(ogg, "ogg");
        let mut frames = 0u64;
        let mut peak = 0f32;
        while let Ok(packet) = parsed.format.next_packet() {
            let decoded = parsed.decoder.decode(&packet).expect("decodes");
            let (rate, left, right) = stereo_of(decoded);
            assert_eq!(rate, RATE);
            frames += left.len() as u64;
            peak = left.iter().chain(&right).fold(peak, |p, s| p.max(s.abs()));
        }
        (frames, peak)
    }

    fn transcode(bytes: Vec<u8>, ext: &str, start: f64) -> Vec<u8> {
        let mut out = Vec::new();
        let ended = run(parse(bytes, ext), start, 1.0, |b| {
            out.extend(b);
            true
        })
        .expect("transcodes");
        assert_eq!(ended, Ended::Finished);
        out
    }

    #[test]
    fn a_44k_mono_file_becomes_48k_stereo_opus_of_the_same_length() {
        let out = transcode(wav(44_100, 1, 3.0), "wav", 0.0);
        assert_eq!(&out[..4], b"OggS");
        let (frames, peak) = decode_back(out);
        let seconds = frames as f64 / RATE as f64;
        assert!((seconds - 3.0).abs() < 0.15, "{seconds} s");
        assert!(peak > 0.3 && peak < 0.8, "peak {peak}");
    }

    #[test]
    fn a_48k_stereo_file_passes_straight_through() {
        let out = transcode(wav(48_000, 2, 2.0), "wav", 0.0);
        let (frames, _) = decode_back(out);
        assert!((frames as f64 / RATE as f64 - 2.0).abs() < 0.1);
    }

    #[test]
    fn a_start_time_skips_ahead() {
        let out = transcode(wav(48_000, 2, 4.0), "wav", 3.0);
        let (frames, _) = decode_back(out);
        let seconds = frames as f64 / RATE as f64;
        assert!((seconds - 1.0).abs() < 0.15, "{seconds} s");
    }

    #[test]
    fn a_listener_leaving_stops_the_work() {
        let mut calls = 0;
        let ended = run(parse(wav(48_000, 2, 5.0), "wav"), 0.0, 1.0, |_| {
            calls += 1;
            calls < 3
        })
        .unwrap();
        assert_eq!(ended, Ended::Gone);
        assert_eq!(calls, 3);
    }

    #[test]
    fn garbage_is_an_error_not_silence() {
        let stream = MediaSourceStream::new(Box::new(std::io::Cursor::new(vec![0u8; 4096])), Default::default());
        assert!(get_probe().format(&Hint::new(), stream, &Default::default(), &Default::default()).is_err());
    }
}
