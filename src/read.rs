use futures_util::io::{self as fio, AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

use crate::*;

pub trait ReadError {
    fn io_error(error: fio::Error) -> Self;
}

impl ReadError for fio::Error {
    #[inline]
    fn io_error(error: fio::Error) -> Self {
        error
    }
}

#[allow(unused)]
pub trait ReadHandler {
    type Error: ReadError;
    fn header(
        &mut self,
        format: Format,
        num_tracks: u16,
        division: Division,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
    fn track(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn midi_event(&mut self, delta: u32, data: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }
    fn meta_event(&mut self, delta: u32, id: u8, data: Vec<u8>) -> Result<(), Self::Error> {
        Ok(())
    }
    fn escaped_event(&mut self, delta: u32, data: Vec<u8>) -> Result<(), Self::Error> {
        Ok(())
    }
    fn sysex_event(&mut self, delta: u32, data: Vec<u8>) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[inline]
async fn read_u8<R: AsyncRead + Unpin>(reader: &mut R) -> fio::Result<u8> {
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf).await?;
    Ok(buf[0])
}
#[inline]
async fn read_u16_be<R: AsyncRead + Unpin>(reader: &mut R) -> fio::Result<u16> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf).await?;
    Ok(u16::from_be_bytes(buf))
}
#[inline]
async fn read_u32_be<R: AsyncRead + Unpin>(reader: &mut R) -> fio::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf).await?;
    Ok(u32::from_be_bytes(buf))
}
async fn read_vlq<R: AsyncRead + AsyncSeek + Unpin>(reader: &mut R) -> fio::Result<u32> {
    let mut value = 0u32;
    let mut count = 0;
    loop {
        let c = read_u8(reader).await?;
        value = (value << 7) | ((c & 0x7f) as u32);
        if c & 0x80 == 0 {
            break;
        }
        count += 1;
        if count >= 4 {
            let pos = reader.stream_position().await?;
            return Err(fio::Error::new(
                fio::ErrorKind::InvalidData,
                format!("VLQ too long (byte {:#04X} at {:#x})", c, pos),
            ));
        }
    }
    Ok(value)
}
async fn read_vlq_event<R: AsyncRead + AsyncSeek + Unpin>(reader: &mut R) -> fio::Result<Vec<u8>> {
    let length = read_vlq(reader).await?;
    let mut data = vec![0u8; length as usize];
    if length > 0 {
        reader.read_exact(&mut data).await?;
    }
    Ok(data)
}
#[inline]
async fn read_chunk_type<R: AsyncRead + Unpin>(reader: &mut R) -> fio::Result<[u8; 4]> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

#[inline]
fn validate_u7(offset: u64, data: &[u8]) -> fio::Result<()> {
    for (i, b) in data.iter().enumerate() {
        if *b >= 0x80 {
            return Err(fio::Error::new(
                fio::ErrorKind::InvalidData,
                format!("invalid byte {:#04X} at offset {:#x}", b, i as u64 + offset),
            ));
        }
    }
    Ok(())
}

pub async fn read<H, R>(handler: &mut H, reader: &mut R) -> Result<(), H::Error>
where
    H: ReadHandler,
    R: AsyncRead + AsyncSeek + Unpin,
{
    let magic = read_chunk_type(reader).await.map_err(H::Error::io_error)?;
    let header_len = read_u32_be(reader).await.map_err(H::Error::io_error)?;
    let format = read_u16_be(reader).await.map_err(H::Error::io_error)?;
    let num_tracks = read_u16_be(reader).await.map_err(H::Error::io_error)?;
    let division = read_u16_be(reader).await.map_err(H::Error::io_error)?;

    if &magic != b"MThd" {
        return Err(H::Error::io_error(fio::Error::new(
            fio::ErrorKind::InvalidData,
            format!("invalid MIDI header: {:?}", magic),
        )));
    }
    if header_len != 6 {
        return Err(H::Error::io_error(fio::Error::new(
            fio::ErrorKind::InvalidData,
            format!("invalid MIDI header length: {}", header_len),
        )));
    }
    let format = match format {
        0 => Format::Single,
        1 => Format::Multiple,
        2 => Format::Sequential,
        _ => {
            return Err(H::Error::io_error(fio::Error::new(
                fio::ErrorKind::InvalidData,
                format!("invalid MIDI format: {}", format),
            )))
        }
    };
    let division = if (division as i16) < 0 {
        let fps = -(division as i16 >> 8) as u8;
        if fps != 24 && fps != 25 && fps != 29 && fps != 30 {
            return Err(H::Error::io_error(fio::Error::new(
                fio::ErrorKind::InvalidData,
                format!("invalid SMPTE format: {:#04X}", -(fps as i8)),
            )));
        }
        Division::SMPTE {
            fps,
            tpf: division as u8,
        }
    } else {
        Division::PPQN(division)
    };
    handler.header(format, num_tracks, division)?;
    for track_index in 0..num_tracks {
        let magic = read_chunk_type(reader).await.map_err(H::Error::io_error)?;
        let track_len = read_u32_be(reader).await.map_err(H::Error::io_error)?;
        if &magic != b"MTrk" {
            return Err(H::Error::io_error(fio::Error::new(
                fio::ErrorKind::InvalidData,
                format!("invalid track header: {:?}", magic),
            )));
        }
        handler.track()?;

        let track_end =
            reader.stream_position().await.map_err(H::Error::io_error)? + (track_len as u64);
        let mut sysex_continuation = false;
        let mut last_status = 0;

        loop {
            let current_pos = reader.stream_position().await.map_err(H::Error::io_error)?;
            if current_pos == track_end {
                break;
            }
            if current_pos > track_end {
                return Err(H::Error::io_error(fio::Error::new(
                    fio::ErrorKind::InvalidData,
                    format!(
                        "read past end of track {} at {:#x}",
                        track_index, current_pos
                    ),
                )));
            }
            let delta = read_vlq(reader).await.map_err(H::Error::io_error)?;
            let status = read_u8(reader).await.map_err(H::Error::io_error)?;
            let running_status = if (0x80..0xF0).contains(&status) {
                last_status = status;
                false
            } else {
                true
            };
            if sysex_continuation {
                if status != 0xF7 {
                    let pos = reader.stream_position().await.map_err(H::Error::io_error)?;
                    return Err(H::Error::io_error(fio::Error::new(
                        fio::ErrorKind::InvalidData,
                        format!(
                            "expected sysex continuation 0xF7, got {:#04X} in track {} at {:#x}",
                            status, track_index, pos,
                        ),
                    )));
                }
                last_status = 0;

                let data = read_vlq_event(reader).await.map_err(H::Error::io_error)?;
                if data.last() == Some(&0xF7) {
                    sysex_continuation = false;
                };
                let pos = reader.stream_position().await.map_err(H::Error::io_error)?;
                validate_u7(pos, &data).map_err(H::Error::io_error)?;
                handler.sysex_event(delta, data)?;
            } else if status == 0xF0 {
                last_status = 0;

                let data = read_vlq_event(reader).await.map_err(H::Error::io_error)?;
                if data.last() != Some(&0xF7) {
                    sysex_continuation = true;
                }
                let pos = reader.stream_position().await.map_err(H::Error::io_error)?;
                validate_u7(pos, &data).map_err(H::Error::io_error)?;
                handler.sysex_event(delta, data)?;
            } else if status == 0xF7 {
                last_status = 0;
                let data = read_vlq_event(reader).await.map_err(H::Error::io_error)?;
                handler.escaped_event(delta, data)?;
            } else if status == 0xFF {
                last_status = 0;
                let meta_type = read_u8(reader).await.map_err(H::Error::io_error)?;
                validate_u7(current_pos + 1, &[meta_type]).map_err(H::Error::io_error)?;
                let data = read_vlq_event(reader).await.map_err(H::Error::io_error)?;
                handler.meta_event(delta, meta_type, data)?;
            } else if last_status != 0 {
                let length = match last_status & 0xF0 {
                    0xC0 | 0xD0 => 2,
                    _ => 3,
                };
                let mut data = [0u8; 3];
                data[0] = last_status;
                let offset = if running_status {
                    data[1] = status;
                    1
                } else {
                    0
                };
                let range = 1 + offset..length;
                if range.end - range.start > 0 {
                    reader
                        .read_exact(&mut data[range])
                        .await
                        .map_err(H::Error::io_error)?;
                }
                validate_u7(current_pos + (1 - offset as u64), &data[1..length])
                    .map_err(H::Error::io_error)?;
                handler.midi_event(delta, &data[0..length])?;
            } else {
                let pos = reader.stream_position().await.map_err(H::Error::io_error)?;
                return Err(H::Error::io_error(fio::Error::new(
                    fio::ErrorKind::InvalidData,
                    format!(
                        "expected valid status byte, got {:#04X} in track {} at {:#x}",
                        status, track_index, pos,
                    ),
                )));
            }
        }
    }
    Ok(())
}
