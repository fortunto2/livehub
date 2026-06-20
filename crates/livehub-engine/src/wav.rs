//! Minimal 16-bit PCM WAV writer (no external dependency) — for beam's local
//! clean-audio backup recording. The RIFF/data sizes are patched on `finalize`.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

pub struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u32,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let byte_rate = sample_rate * channels as u32 * 2;
        let block_align = channels * 2;
        file.write_all(b"RIFF")?;
        file.write_all(&0u32.to_le_bytes())?; // RIFF size — patched in finalize
        file.write_all(b"WAVE")?;
        file.write_all(b"fmt ")?;
        file.write_all(&16u32.to_le_bytes())?; // fmt chunk size
        file.write_all(&1u16.to_le_bytes())?; // PCM
        file.write_all(&channels.to_le_bytes())?;
        file.write_all(&sample_rate.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&block_align.to_le_bytes())?;
        file.write_all(&16u16.to_le_bytes())?; // bits per sample
        file.write_all(b"data")?;
        file.write_all(&0u32.to_le_bytes())?; // data size — patched in finalize
        Ok(WavWriter { file, data_bytes: 0 })
    }

    /// Append interleaved i16 samples.
    pub fn write(&mut self, samples: &[i16]) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.data_bytes += bytes.len() as u32;
        Ok(())
    }

    /// Patch the RIFF + data sizes and flush.
    pub fn finalize(mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + self.data_bytes).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&self.data_bytes.to_le_bytes())?;
        f.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_valid_wav_header() {
        let path = std::env::temp_dir().join("livehub_wav_test.wav");
        let mut w = WavWriter::create(&path, 48_000, 2).unwrap();
        w.write(&[0i16, 0, 1000, -1000]).unwrap(); // 2 stereo frames
        w.finalize().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // data size = 4 samples * 2 bytes = 8
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 8);
        let _ = std::fs::remove_file(&path);
    }
}
