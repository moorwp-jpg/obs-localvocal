use std::{collections::VecDeque, sync::Mutex, time::Duration};
use video_bytestream_tools::webvtt::WebvttWrite;

/// How many video frames before it is due a chunk may be written
/// (see `WebvttMuxer::try_mux_into_bytestream`).
const SEND_AHEAD_FRAMES: u32 = 2;

/// The fewest video frames per chunk that the send frequency may leave.
const MIN_FRAMES_PER_CHUNK: u32 = 2;

/// The highest send frequency that leaves `MIN_FRAMES_PER_CHUNK` frames per chunk.
fn max_send_frequency_hz(video_frame_time: Duration) -> u8 {
    let chunk_time = video_frame_time.as_nanos() * u128::from(MIN_FRAMES_PER_CHUNK);
    if chunk_time == 0 {
        return u8::MAX;
    }
    u8::try_from(1_000_000_000 / chunk_time)
        .unwrap_or(u8::MAX)
        .max(1)
}

pub struct WebvttMuxerBuilder {
    latency_to_video: Duration,
    send_frequency_hz: u8,
    video_frame_time: Duration,
    tracks: Vec<WebvttMuxerTrack>,
}

struct WebvttMuxerTrack {
    cues: VecDeque<WebvttCue>,
    default: bool,
    autoselect: bool,
    forced: bool,
    name: String,
    language: String,
    assoc_language: Option<String>,
    characteristics: Option<String>,
}

pub struct WebvttMuxer {
    latency_to_video: Duration,
    send_frequency_hz: u8,
    video_frame_time: Duration,
    inner: Mutex<WebvttMuxerInner>,
}

struct WebvttMuxerInner {
    tracks: Vec<WebvttMuxerTrack>,
    webvtt_buffer: String,
    next_chunk_number: u64,
    first_video_timestamp: Option<Duration>,
}

// TODO: this should probably be moved into video-bytestream-tools instead
pub struct WebvttString(String);

struct WebvttCue {
    start_time: Duration,
    duration: Duration,
    text: WebvttString,
}

pub struct NulError {
    pub string: String,
    pub nul_position: usize,
}

impl WebvttString {
    /// Create a `WebvttString`.
    /// This verifies that there are no interior NUL bytes, since
    /// the WebVTT-in-SEI wire format uses NUL terminated strings.
    ///
    /// # Errors
    ///
    /// This function will return an error if there are any NUL bytes in the string.
    pub fn from_string(string: String) -> Result<Self, NulError> {
        if let Some(nul_position) = string.find('\0') {
            Err(NulError {
                string,
                nul_position,
            })
        } else {
            Ok(WebvttString(string))
        }
    }
}

pub struct TooManySubtitleTracksError {
    pub name: WebvttString,
    pub language: WebvttString,
    pub assoc_language: Option<WebvttString>,
    pub characteristics: Option<WebvttString>,
}

impl WebvttMuxerBuilder {
    pub fn new(
        latency_to_video: Duration,
        send_frequency_hz: u8,
        video_frame_time: Duration,
    ) -> Self {
        // A chunk is due `latency_to_video` after it starts, and may be written
        // up to `SEND_AHEAD_FRAMES` frames before it is due. With less latency
        // than that, a chunk could land in a frame before it starts, and its
        // video offset would be negative (a panic that aborts the host).
        let min_latency_to_video = video_frame_time * SEND_AHEAD_FRAMES;
        // At most one chunk is written per video frame, so if chunks are due
        // more often than frames, the muxer falls further behind every frame.
        // Leaving `MIN_FRAMES_PER_CHUNK` frames per chunk also lets it catch
        // up after dropped frames.
        let max_send_frequency_hz = max_send_frequency_hz(video_frame_time);
        Self {
            latency_to_video: latency_to_video.max(min_latency_to_video),
            send_frequency_hz: send_frequency_hz.clamp(1, max_send_frequency_hz),
            video_frame_time,
            tracks: vec![],
        }
    }

    // FIXME: split these arguments somehow?
    #[allow(clippy::too_many_arguments)]
    pub fn add_track(
        &mut self,
        default: bool,
        autoselect: bool,
        forced: bool,
        name: WebvttString,
        language: WebvttString,
        assoc_language: Option<WebvttString>,
        characteristics: Option<WebvttString>,
    ) -> Result<&mut Self, TooManySubtitleTracksError> {
        if self.tracks.len() == 0xff {
            return Err(TooManySubtitleTracksError {
                name,
                language,
                assoc_language,
                characteristics,
            });
        }
        self.tracks.push(WebvttMuxerTrack {
            cues: VecDeque::new(),
            default,
            autoselect,
            forced,
            name: name.0,
            language: language.0,
            assoc_language: assoc_language.map(|a| a.0),
            characteristics: characteristics.map(|c| c.0),
        });
        Ok(self)
    }

    pub fn create_muxer(self) -> WebvttMuxer {
        WebvttMuxer {
            latency_to_video: self.latency_to_video,
            send_frequency_hz: self.send_frequency_hz,
            video_frame_time: self.video_frame_time,
            inner: Mutex::new(WebvttMuxerInner {
                tracks: self.tracks,
                webvtt_buffer: String::new(),
                next_chunk_number: 0,
                first_video_timestamp: None,
            }),
        }
    }
}

pub struct InvalidWebvttTrack(pub u8);

impl WebvttMuxer {
    pub fn add_cue(
        &self,
        track: u8,
        start_time: Duration,
        duration: Duration,
        text: WebvttString,
    ) -> Result<(), InvalidWebvttTrack> {
        let mut inner = self.inner.lock().unwrap();
        let tracks = &mut inner.tracks;
        let track = tracks
            .get_mut(usize::from(track))
            .ok_or(InvalidWebvttTrack(track))?;
        let cues = &mut track.cues;
        let index = cues
            .iter()
            .position(|c| c.start_time > start_time)
            .unwrap_or(cues.len());
        cues.insert(
            index,
            WebvttCue {
                start_time,
                duration,
                text,
            },
        );
        Ok(())
    }

    fn consume_cues_into_chunk<'a>(
        cues: &mut VecDeque<WebvttCue>,
        timestamp: Duration,
        duration: Duration,
        buffer: &'a mut String,
    ) -> &'a str {
        while cues
            .front()
            .map(|cue| (cue.start_time + cue.duration) < timestamp)
            .unwrap_or(false)
        {
            cues.pop_front();
        }

        buffer.clear();

        for cue in &*cues {
            if cue.start_time > (timestamp + duration) {
                break;
            }
            let cue_start = if cue.start_time > timestamp {
                cue.start_time
            } else {
                timestamp
            };
            let cue_end = (cue.start_time + cue.duration).min(timestamp + duration);
            buffer.push_str(&format!(
                "{:0>2}:{:0>2}:{:0>2}.{:0>3} --> {:0>2}:{:0>2}:{:0>2}.{:0>3}\n{}\n\n",
                cue_start.as_secs() / 3600,
                cue_start.as_secs() % 3600 / 60,
                cue_start.as_secs() % 60,
                cue_start.as_millis() % 1000,
                cue_end.as_secs() / 3600,
                cue_end.as_secs() % 3600 / 60,
                cue_end.as_secs() % 60,
                cue_end.as_millis() % 1000,
                cue.text.0
            ))
        }
        buffer.as_str()
    }

    pub fn try_mux_into_bytestream(
        &self,
        video_timestamp: Duration,
        add_header: bool,
        writer: &mut impl WebvttWrite,
    ) -> std::io::Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        let WebvttMuxerInner {
            tracks,
            webvtt_buffer,
            next_chunk_number,
            first_video_timestamp,
        } = &mut *inner;

        if add_header {
            // TODO: cache this? forward iter instead?
            let webvtt_tracks = tracks
                .iter()
                .map(|track| video_bytestream_tools::webvtt::WebvttTrack {
                    default: track.default,
                    autoselect: track.autoselect,
                    forced: track.forced,
                    language: &track.language,
                    name: &track.name,
                    assoc_language: track.assoc_language.as_deref(),
                    characteristics: track.characteristics.as_deref(),
                })
                .collect::<Vec<_>>();
            writer.write_webvtt_header(
                self.latency_to_video,
                self.send_frequency_hz,
                &webvtt_tracks,
            )?;
        }

        let duration_between_sends =
            Duration::from_secs_f64(1. / f64::from(self.send_frequency_hz));
        let first_video_timestamp = &*first_video_timestamp.get_or_insert(video_timestamp);
        let next_chunk_webvtt_timestamp =
            u32::try_from(*next_chunk_number).unwrap() * duration_between_sends;
        let next_chunk_video_timestamp =
            *first_video_timestamp + self.latency_to_video + next_chunk_webvtt_timestamp;
        let send_ahead = self.video_frame_time * SEND_AHEAD_FRAMES;
        if next_chunk_video_timestamp > video_timestamp + send_ahead {
            return Ok(add_header);
        }
        let video_offset = video_timestamp - (*first_video_timestamp + next_chunk_webvtt_timestamp);
        if video_offset.as_millis() > u128::from(u16::MAX) {
            // The offset is sent as u16 milliseconds, so a chunk this late
            // (after a long gap in the video, for example) can't be sent.
            // Drop it rather than panic in the writer.
            *next_chunk_number += 1;
            return Ok(add_header);
        }
        let chunk_number = *next_chunk_number;
        // TODO: return an error type that allows skipping chunks if the writer fails?
        for (track_index, track) in tracks.iter_mut().enumerate() {
            let webvtt_payload = Self::consume_cues_into_chunk(
                &mut track.cues,
                next_chunk_webvtt_timestamp,
                duration_between_sends,
                webvtt_buffer,
            );
            writer.write_webvtt_payload(
                u8::try_from(track_index).unwrap(),
                chunk_number,
                0,
                video_offset,
                webvtt_payload,
            )?;
        }
        *next_chunk_number += 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{WebvttMuxer, WebvttMuxerBuilder, WebvttString, SEND_AHEAD_FRAMES};
    use std::time::Duration;
    use video_bytestream_tools::webvtt::{WebvttTrack, WebvttWrite};

    const FRAME_TIME_30_FPS: Duration = Duration::from_nanos(33_333_333);
    const START: Duration = Duration::from_secs(100);

    /// Records each payload's video offset instead of writing bytes.
    struct OffsetRecorder(Vec<Duration>);

    impl WebvttWrite for OffsetRecorder {
        fn write_webvtt_header(
            &mut self,
            _max_latency_to_video: Duration,
            _send_frequency_hz: u8,
            _subtitle_tracks: &[WebvttTrack],
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn write_webvtt_payload(
            &mut self,
            _track_index: u8,
            _chunk_number: u64,
            _chunk_version: u8,
            video_offset: Duration,
            _webvtt_payload: &str,
        ) -> std::io::Result<()> {
            // The real writers send the offset as u16 milliseconds and panic
            // if it doesn't fit.
            let millis = u16::try_from(video_offset.as_millis());
            assert!(millis.is_ok(), "video offset {video_offset:?} is too large");
            self.0.push(video_offset);
            Ok(())
        }
    }

    fn english_muxer(latency_ms: u64, send_frequency_hz: u8) -> WebvttMuxer {
        let latency = Duration::from_millis(latency_ms);
        let mut builder = WebvttMuxerBuilder::new(latency, send_frequency_hz, FRAME_TIME_30_FPS);
        let name = WebvttString("English".to_string());
        let lang = WebvttString("en".to_string());
        let added = builder.add_track(false, false, false, name, lang, None, None);
        assert!(added.is_ok());
        builder.create_muxer()
    }

    /// Timestamps of `count` consecutive frames at 30 fps.
    fn frames(start: Duration, count: u32) -> impl Iterator<Item = Duration> {
        (0..count).map(move |frame| start + FRAME_TIME_30_FPS * frame)
    }

    /// Muxes one frame per timestamp and returns the offset of each chunk written.
    fn mux(muxer: &WebvttMuxer, timestamps: impl Iterator<Item = Duration>) -> Vec<Duration> {
        let mut writer = OffsetRecorder(Vec::new());
        for (index, ts) in timestamps.enumerate() {
            let result = muxer.try_mux_into_bytestream(ts, index == 0, &mut writer);
            assert!(result.is_ok());
        }
        writer.0
    }

    #[test]
    fn low_latency_does_not_panic() {
        // At 30 fps these used to panic within the first second of video.
        for latency_ms in [0, 10, 33, 50, 66] {
            let offsets = mux(&english_muxer(latency_ms, 2), frames(START, 300));
            assert!(offsets.len() > 1, "no chunks at {latency_ms} ms");
        }
    }

    #[test]
    fn latency_is_at_least_send_ahead_window() {
        let minimum = FRAME_TIME_30_FPS * SEND_AHEAD_FRAMES;
        assert_eq!(english_muxer(0, 2).latency_to_video, minimum);
        assert_eq!(english_muxer(50, 2).latency_to_video, minimum);
        let latency = Duration::from_millis(100);
        assert_eq!(english_muxer(100, 2).latency_to_video, latency);
    }

    #[test]
    fn send_frequency_leaves_two_frames_per_chunk() {
        // 30 fps allows at most 15 chunks per second.
        assert_eq!(english_muxer(10_000, 60).send_frequency_hz, 15);
        assert_eq!(english_muxer(10_000, 15).send_frequency_hz, 15);
        assert_eq!(english_muxer(10_000, 2).send_frequency_hz, 2);
        assert_eq!(english_muxer(10_000, 0).send_frequency_hz, 1);
    }

    #[test]
    fn high_send_frequency_keeps_up_with_video() {
        // At 60 Hz the muxer used to fall further behind every frame until
        // the offset overflowed, about two minutes in.
        let offsets = mux(&english_muxer(10_000, 60), frames(START, 30 * 300));
        let latency = Duration::from_secs(10);
        assert!(offsets.iter().all(|offset| *offset <= latency));
    }

    #[test]
    fn long_video_gap_does_not_panic() {
        // A minute of video, a 60 second gap, then another minute.
        let before = frames(START, 30 * 60);
        let after = frames(START + Duration::from_secs(120), 30 * 60);
        let offsets = mux(&english_muxer(10_000, 2), before.chain(after));
        // Chunks too late to send are dropped, and the muxer catches up.
        let latency = Duration::from_secs(10);
        assert!(offsets.last().is_some_and(|offset| *offset <= latency));
    }
}
