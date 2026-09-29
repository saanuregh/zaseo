use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Paseo's dictation stream is 16 kHz mono PCM16 in one-second chunks.
pub(crate) const DICTATION_SAMPLE_RATE: u32 = 16_000;
pub(crate) const CHUNK_SAMPLES: usize = DICTATION_SAMPLE_RATE as usize;

pub(crate) enum AudioMessage {
    Samples(Vec<f32>),
    Failed(String),
    Stop,
}

/// Holds the microphone open; dropping it stops capture.
pub(crate) struct Recorder {
    _stream: cpal::Stream,
}

pub(crate) struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Opens the default microphone and sends its interleaved samples, scaled to -1..1.
pub(crate) fn start_capture(
    sender: async_channel::Sender<AudioMessage>,
) -> Result<(Recorder, CaptureFormat)> {
    let device = cpal::default_host()
        .default_input_device()
        .context("No microphone is available")?;
    let supported = device
        .default_input_config()
        .map_err(|error| anyhow!("Could not read the microphone format: {error}"))?;
    let format = CaptureFormat {
        sample_rate: supported.sample_rate(),
        channels: supported.channels(),
    };
    if format.channels == 0 || format.sample_rate == 0 {
        bail!("The microphone reported an empty audio format");
    }
    let config = supported.config();
    let on_error = {
        let sender = sender.clone();
        move |error: cpal::StreamError| {
            if sender
                .try_send(AudioMessage::Failed(format!(
                    "Microphone capture failed: {error}"
                )))
                .is_err()
            {
                log::error!("Microphone capture failed after dictation ended: {error}");
            }
        }
    };
    fn forward<T>(
        sender: async_channel::Sender<AudioMessage>,
        convert: fn(T) -> f32,
    ) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static
    where
        T: Copy + 'static,
    {
        move |data: &[T], _| {
            let samples = data.iter().copied().map(convert).collect();
            if sender.try_send(AudioMessage::Samples(samples)).is_err() {
                log::debug!("Dictation stopped before audio was delivered");
            }
        }
    }
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            forward::<f32>(sender, |sample| sample),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            forward::<i16>(sender, |sample| f32::from(sample) / 32_768.0),
            on_error,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config,
            forward::<u16>(sender, |sample| (f32::from(sample) - 32_768.0) / 32_768.0),
            on_error,
            None,
        ),
        format => bail!("Unsupported microphone sample format {format:?}"),
    }
    .map_err(|error| anyhow!("Could not open the microphone: {error}"))?;
    stream
        .play()
        .map_err(|error| anyhow!("Could not start the microphone: {error}"))?;
    Ok((Recorder { _stream: stream }, format))
}

/// Downmixes interleaved audio to mono and resamples it to 16 kHz PCM16, keeping state between
/// buffers so chunk boundaries don't click.
pub(crate) struct Pcm16Encoder {
    channels: usize,
    step: f64,
    position: f64,
    pending: Vec<f32>,
}

impl Pcm16Encoder {
    pub(crate) fn new(format: &CaptureFormat) -> Self {
        Self {
            channels: usize::from(format.channels.max(1)),
            step: f64::from(format.sample_rate) / f64::from(DICTATION_SAMPLE_RATE),
            position: 0.0,
            pending: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, interleaved: &[f32], output: &mut Vec<i16>) {
        self.pending.extend(
            interleaved
                .chunks(self.channels)
                .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32),
        );
        while self.position + 1.0 < self.pending.len() as f64 {
            let index = self.position.floor() as usize;
            let fraction = (self.position - index as f64) as f32;
            let (Some(current), Some(next)) =
                (self.pending.get(index), self.pending.get(index + 1))
            else {
                break;
            };
            let sample = current + (next - current) * fraction;
            output.push((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16);
            self.position += self.step;
        }
        let consumed = (self.position.floor() as usize).min(self.pending.len());
        self.pending.drain(..consumed);
        self.position -= consumed as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_downmixes_and_resamples_across_buffers() {
        let mut encoder = Pcm16Encoder::new(&CaptureFormat {
            sample_rate: 48_000,
            channels: 2,
        });
        let mut output = Vec::new();
        let one_second_stereo = vec![0.5f32; 48_000 * 2];
        for buffer in one_second_stereo.chunks(1_000) {
            encoder.push(buffer, &mut output);
        }
        assert!(
            (15_990..=16_000).contains(&output.len()),
            "{} samples",
            output.len()
        );
        assert!(output.iter().all(|sample| *sample == 16_383));
    }

    #[test]
    fn encoder_passes_16_khz_mono_through() {
        let mut encoder = Pcm16Encoder::new(&CaptureFormat {
            sample_rate: 16_000,
            channels: 1,
        });
        let mut output = Vec::new();
        encoder.push(&[0.0, 1.0, -1.0, 0.25], &mut output);
        encoder.push(&[0.5], &mut output);
        assert_eq!(output, vec![0, 32_767, -32_767, 8_191]);
    }
}
