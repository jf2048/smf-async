use futures_util::io::{self as fio, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::*;

pub async fn write<W: AsyncWrite + AsyncSeek + Unpin>(
    writer: &mut W,
    format: Format,
    division: Division,
) -> fio::Result<Writer<'_, W>> {
    writer.write_all(b"MThd").await?;
    writer.write_all(&6u32.to_be_bytes()).await?;
    writer.write_all(&(format as u16).to_be_bytes()).await?;
    let track_count_pos = writer.stream_position().await?;
    writer.write_all(&0u16.to_be_bytes()).await?;
    let division = match division {
        Division::PPQN(d) => d,
        Division::SMPTE { fps, tpf } => ((-(fps as i8) as i16) << 8) as u16 + tpf as u16,
    };
    writer.write_all(&division.to_be_bytes()).await?;
    Ok(Writer {
        writer,
        track_count_pos,
        track_count: 0,
        track_len_pos: 0,
        last_status: 0,
        sysex_continuation: false,
    })
}

#[must_use]
#[derive(Debug)]
pub struct Writer<'w, W: AsyncWrite + AsyncSeek + Unpin> {
    writer: &'w mut W,
    track_count_pos: u64,
    track_count: u16,
    track_len_pos: u64,
    last_status: u8,
    sysex_continuation: bool,
}

impl<'w, W: AsyncWrite + AsyncSeek + Unpin> Writer<'w, W> {
    pub async fn track(&mut self) -> fio::Result<TrackWriter<'_, 'w, W>> {
        self.writer.write_all(b"MTrk").await?;
        self.track_len_pos = self.writer.stream_position().await?;
        self.writer.write_all(&0u32.to_be_bytes()).await?;
        self.last_status = 0;
        self.track_count += 1;
        Ok(TrackWriter(self))
    }
    pub async fn finish(self) -> fio::Result<()> {
        if self.track_count > 0 {
            let pos = self.writer.stream_position().await?;
            self.writer
                .seek(fio::SeekFrom::Start(self.track_count_pos))
                .await?;
            self.writer
                .write_all(&self.track_count.to_be_bytes())
                .await?;
            self.writer.seek(fio::SeekFrom::Start(pos)).await?;
        }
        Ok(())
    }
}

#[must_use]
#[derive(Debug)]
pub struct TrackWriter<'t, 'w, W: AsyncWrite + AsyncSeek + Unpin>(&'t mut Writer<'w, W>);

impl<'t, 'w, W: AsyncWrite + AsyncSeek + Unpin> TrackWriter<'t, 'w, W> {
    pub async fn vlq(&mut self, v: u32) -> fio::Result<()> {
        debug_assert!(v <= 0xfffffff);

        #[inline]
        const fn b(v: u32, byte: u32) -> u8 {
            let byte = byte * 7;
            let last = if byte > 0 { 0x80 } else { 0 };
            ((v & (0x7f << byte)) >> byte) as u8 | last
        }

        let w = &mut self.0.writer;
        if v > 0x1fffff {
            w.write_all(&[b(v, 3), b(v, 2), b(v, 1), b(v, 0)]).await
        } else if v > 0x3fff {
            w.write_all(&[b(v, 2), b(v, 1), b(v, 0)]).await
        } else if v > 0x7f {
            w.write_all(&[b(v, 1), b(v, 0)]).await
        } else {
            w.write_all(&[b(v, 0)]).await
        }
    }
    pub async fn raw_event(&mut self, delta: u32, data: &[u8]) -> fio::Result<()> {
        self.vlq(delta).await?;
        self.0.writer.write_all(data).await
    }
    pub async fn midi_event(&mut self, delta: u32, data: &[u8]) -> fio::Result<()> {
        debug_assert!(!self.0.sysex_continuation);
        debug_assert!(data[0] >= 0x80 && data[0] < 0xF0);
        debug_assert!(if data[0] >= 0xC0 && data[0] < 0xF0 {
            data.len() == 2
        } else {
            data.len() == 3
        });
        debug_assert!(data.iter().skip(1).all(|b| *b < 0x80));
        if data[0] == self.0.last_status {
            self.raw_event(delta, &data[1..data.len()]).await?;
        } else {
            self.raw_event(delta, data).await?;
            self.0.last_status = data[0];
        }
        Ok(())
    }
    pub async fn meta_event(&mut self, delta: u32, id: u8, data: &[u8]) -> fio::Result<()> {
        debug_assert!(!self.0.sysex_continuation);
        debug_assert!(id < 0x80);
        self.0.last_status = 0;
        self.vlq(delta).await?;
        self.0.writer.write_all(&[0xFFu8, id]).await?;
        self.vlq(data.len().try_into().unwrap()).await?;
        self.0.writer.write_all(data).await?;
        Ok(())
    }
    pub async fn escaped_event(&mut self, delta: u32, data: &[u8]) -> fio::Result<()> {
        debug_assert!(!self.0.sysex_continuation);
        self.0.last_status = 0;
        self.vlq(delta).await?;
        self.0.writer.write_all(&[0xF7u8]).await?;
        self.vlq(data.len().try_into().unwrap()).await?;
        self.0.writer.write_all(data).await?;
        Ok(())
    }
    pub async fn sysex_event(&mut self, delta: u32, data: &[u8]) -> fio::Result<()> {
        let status = data[0];
        debug_assert!(status == 0xF0 || status == 0xF7);
        if status == 0xF0 {
            debug_assert!(!self.0.sysex_continuation);
            self.0.sysex_continuation = true;
        } else if status == 0xF7 {
            debug_assert!(self.0.sysex_continuation);
        }
        if data.len() > 1 && data.last() == Some(&0xF7) {
            debug_assert!(data.iter().skip(1).take(data.len() - 2).all(|b| *b < 0x80));
            self.0.sysex_continuation = false;
        } else {
            debug_assert!(data.iter().skip(1).all(|b| *b < 0x80));
        }
        self.0.last_status = 0;
        self.vlq(delta).await?;
        self.0.writer.write_all(&[status]).await?;
        self.vlq((data.len() - 1).try_into().unwrap()).await?;
        self.0.writer.write_all(&data[1..data.len()]).await?;
        Ok(())
    }
    pub async fn finish(self) -> fio::Result<()> {
        let track_len =
            u32::try_from(self.0.writer.stream_position().await? - (self.0.track_len_pos + 4))
                .unwrap();
        if track_len > 0 {
            self.0
                .writer
                .seek(fio::SeekFrom::Start(self.0.track_len_pos))
                .await?;
            self.0.writer.write_all(&track_len.to_be_bytes()).await?;
            self.0
                .writer
                .seek(fio::SeekFrom::Current(track_len as i64))
                .await?;
        }
        Ok(())
    }
}
