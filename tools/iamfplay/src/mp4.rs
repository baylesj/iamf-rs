//! Minimal ISO-BMFF demuxer for IAMF-in-MP4 (non-fragmented): finds the
//! `soun` track whose sample entry is `iamf`, pulls the descriptor OBUs
//! from its `iacb` configuration box, and concatenates the samples
//! (temporal units) in order. Just enough for playback of typical
//! Eclipsa Audio files; fragmented (`moof`) files are rejected.

use std::fmt;

pub(crate) struct Mp4Iamf {
    /// Descriptor OBUs from the `iacb` box.
    pub descriptors: Vec<u8>,
    /// All samples (temporal-unit OBUs) concatenated in track order.
    pub media: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct Mp4Error(String);

impl fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mp4: {}", self.0)
    }
}

impl std::error::Error for Mp4Error {}

fn err<T>(message: impl Into<String>) -> Result<T, Mp4Error> {
    Err(Mp4Error(message.into()))
}

pub(crate) fn is_mp4(data: &[u8]) -> bool {
    data.len() >= 8 && &data[4..8] == b"ftyp"
}

// -- Box walking ------------------------------------------------------------

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Mp4Error> {
        if self.remaining() < n {
            return err("truncated box");
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Mp4Error> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, Mp4Error> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, Mp4Error> {
        Ok(u64::from_be_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    /// Reads one box header, returning (fourcc, body).
    fn next_box(&mut self) -> Result<([u8; 4], &'a [u8]), Mp4Error> {
        let start = self.pos;
        let size32 = self.u32()?;
        let fourcc: [u8; 4] = self.bytes(4)?.try_into().unwrap();
        let size = match size32 {
            0 => self.data.len() - start, // to end of enclosing space
            1 => {
                let large = self.u64()?;
                usize::try_from(large).map_err(|_| Mp4Error("box too large".into()))?
            }
            s => s as usize,
        };
        let header = self.pos - start;
        if size < header {
            return err("box size smaller than header");
        }
        let body = &self.data[self.pos..];
        let body_len = size - header;
        if body.len() < body_len {
            return err("truncated box");
        }
        self.pos += body_len;
        Ok((fourcc, &body[..body_len]))
    }
}

fn find_box(container: &[u8], fourcc: [u8; 4]) -> Option<&[u8]> {
    let mut c = Cursor::new(container);
    while c.remaining() >= 8 {
        match c.next_box() {
            Ok((name, body)) if name == fourcc => return Some(body),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn boxes(container: &[u8], fourcc: [u8; 4]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut c = Cursor::new(container);
    while c.remaining() >= 8 {
        match c.next_box() {
            Ok((name, body)) if name == fourcc => out.push(body),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

// -- Track parsing ----------------------------------------------------------

struct SampleTables {
    /// Per-sample sizes in bytes.
    sizes: Vec<u32>,
    /// Chunk byte offsets in the file.
    chunk_offsets: Vec<u64>,
    /// (first_chunk, samples_per_chunk), 1-based as in stsc.
    chunk_runs: Vec<(u32, u32)>,
}

fn parse_stbl(stbl: &[u8]) -> Result<(Vec<u8>, SampleTables), Mp4Error> {
    let stsd = find_box(stbl, *b"stsd").ok_or(Mp4Error("no stsd".into()))?;
    let mut c = Cursor::new(stsd);
    c.u32()?; // version/flags
    let entry_count = c.u32()?;
    let mut descriptors = None;
    for _ in 0..entry_count {
        let (name, body) = c.next_box()?;
        if &name != b"iamf" {
            continue;
        }
        // AudioSampleEntry: 6 reserved + 2 data_reference_index, then
        // version/revision/vendor (8), channelcount (2), samplesize (2),
        // pre_defined (2), reserved (2), samplerate (4) = 28 bytes.
        if body.len() < 28 {
            return err("iamf sample entry too small");
        }
        let iacb = find_box(&body[28..], *b"iacb").ok_or(Mp4Error("no iacb box".into()))?;
        let mut b = Cursor::new(iacb);
        let version = b.u8()?;
        if version != 1 {
            return err(format!("unsupported iacb configuration_version {version}"));
        }
        // configOBUs_size is a leb128.
        let mut size = 0u32;
        for shift in (0..).step_by(7) {
            let byte = b.u8()?;
            size |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }
        descriptors = Some(b.bytes(size as usize)?.to_vec());
    }
    let Some(descriptors) = descriptors else {
        return err("no iamf sample entry in stsd");
    };

    let stsz = find_box(stbl, *b"stsz").ok_or(Mp4Error("no stsz".into()))?;
    let mut c = Cursor::new(stsz);
    c.u32()?; // version/flags
    let uniform = c.u32()?;
    let sample_count = c.u32()? as usize;
    if sample_count > (1 << 24) {
        return err("implausible sample count");
    }
    let sizes = if uniform != 0 {
        vec![uniform; sample_count]
    } else {
        (0..sample_count)
            .map(|_| c.u32())
            .collect::<Result<_, _>>()?
    };

    let chunk_offsets = if let Some(stco) = find_box(stbl, *b"stco") {
        let mut c = Cursor::new(stco);
        c.u32()?;
        let n = c.u32()? as usize;
        (0..n)
            .map(|_| c.u32().map(u64::from))
            .collect::<Result<Vec<_>, _>>()?
    } else if let Some(co64) = find_box(stbl, *b"co64") {
        let mut c = Cursor::new(co64);
        c.u32()?;
        let n = c.u32()? as usize;
        (0..n).map(|_| c.u64()).collect::<Result<Vec<_>, _>>()?
    } else {
        return err("no stco/co64");
    };

    let stsc = find_box(stbl, *b"stsc").ok_or(Mp4Error("no stsc".into()))?;
    let mut c = Cursor::new(stsc);
    c.u32()?;
    let n = c.u32()? as usize;
    let mut chunk_runs = Vec::with_capacity(n);
    for _ in 0..n {
        let first_chunk = c.u32()?;
        let samples_per_chunk = c.u32()?;
        c.u32()?; // sample_description_index
        chunk_runs.push((first_chunk, samples_per_chunk));
    }

    Ok((
        descriptors,
        SampleTables {
            sizes,
            chunk_offsets,
            chunk_runs,
        },
    ))
}

/// Demuxes an IAMF track from a non-fragmented MP4 file.
pub(crate) fn demux(file: &[u8]) -> Result<Mp4Iamf, Mp4Error> {
    let moov = find_box(file, *b"moov").ok_or(Mp4Error("no moov box".into()))?;

    let mut found = None;
    for trak in boxes(moov, *b"trak") {
        let Some(mdia) = find_box(trak, *b"mdia") else {
            continue;
        };
        let Some(minf) = find_box(mdia, *b"minf") else {
            continue;
        };
        let Some(stbl) = find_box(minf, *b"stbl") else {
            continue;
        };
        if let Ok(parsed) = parse_stbl(stbl) {
            found = Some(parsed);
            break;
        }
    }
    let Some((descriptors, tables)) = found else {
        if find_box(file, *b"moof").is_some() {
            return err("fragmented MP4 is not supported");
        }
        return err("no IAMF audio track found");
    };
    if tables.sizes.is_empty() && find_box(file, *b"moof").is_some() {
        return err("fragmented MP4 is not supported");
    }

    // Expand chunk runs into per-sample file offsets.
    let mut media = Vec::with_capacity(tables.sizes.iter().map(|&s| s as usize).sum());
    let mut sample = 0usize;
    for (run, &(first_chunk, per_chunk)) in tables.chunk_runs.iter().enumerate() {
        let last_chunk = tables
            .chunk_runs
            .get(run + 1)
            .map_or(tables.chunk_offsets.len() as u32 + 1, |&(next_first, _)| {
                next_first
            });
        for chunk in first_chunk..last_chunk {
            let Some(&base) = tables.chunk_offsets.get(chunk as usize - 1) else {
                break;
            };
            let mut offset = base as usize;
            for _ in 0..per_chunk {
                let Some(&size) = tables.sizes.get(sample) else {
                    break;
                };
                let end = offset + size as usize;
                if end > file.len() {
                    return err("sample outside file bounds");
                }
                media.extend_from_slice(&file[offset..end]);
                offset = end;
                sample += 1;
            }
        }
    }
    if sample < tables.sizes.len() {
        return err(format!(
            "chunk tables cover only {sample} of {} samples",
            tables.sizes.len()
        ));
    }

    Ok(Mp4Iamf { descriptors, media })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(fourcc: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&fourcc);
        out.extend_from_slice(body);
        out
    }

    /// Muxes descriptors + equally-sized samples into a minimal MP4 with
    /// two samples per chunk, then demuxes it back.
    #[test]
    fn roundtrip() {
        let descriptors = vec![0xAA; 37];
        let samples: Vec<Vec<u8>> = (0u8..6).map(|i| vec![i; 100 + usize::from(i)]).collect();

        let ftyp = boxed(*b"ftyp", b"iamf\0\0\0\0iamf");
        let mut mdat_body = Vec::new();
        let mut sample_offsets = Vec::new();
        for s in &samples {
            sample_offsets.push(mdat_body.len());
            mdat_body.extend_from_slice(s);
        }
        let mdat = boxed(*b"mdat", &mdat_body);
        let mdat_start = ftyp.len() + 8; // data begins after the mdat header

        let mut iacb = vec![1u8]; // configuration_version
        iacb.push(descriptors.len() as u8); // leb128 (fits one byte)
        iacb.extend_from_slice(&descriptors);
        let mut entry = vec![0u8; 28];
        entry.extend_from_slice(&boxed(*b"iacb", &iacb));
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend_from_slice(&boxed(*b"iamf", &entry));

        let mut stsz = vec![0u8; 8]; // version/flags + uniform=0
        stsz.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        for s in &samples {
            stsz.extend_from_slice(&(s.len() as u32).to_be_bytes());
        }
        // Three chunks of two samples each.
        let mut stco = vec![0, 0, 0, 0, 0, 0, 0, 3];
        for chunk in 0..3 {
            stco.extend_from_slice(
                &((mdat_start + sample_offsets[chunk * 2]) as u32).to_be_bytes(),
            );
        }
        let mut stsc = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsc.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
        stsc.extend_from_slice(&2u32.to_be_bytes()); // samples_per_chunk
        stsc.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index

        let stbl_body = [
            boxed(*b"stsd", &stsd),
            boxed(*b"stsz", &stsz),
            boxed(*b"stco", &stco),
            boxed(*b"stsc", &stsc),
        ]
        .concat();
        let hdlr = boxed(
            *b"hdlr",
            &[b"\0\0\0\0\0\0\0\0soun".to_vec(), vec![0; 14]].concat(),
        );
        let minf = boxed(*b"minf", &boxed(*b"stbl", &stbl_body));
        let mdia = boxed(*b"mdia", &[hdlr, minf].concat());
        let moov = boxed(*b"moov", &boxed(*b"trak", &mdia));

        let file = [ftyp, mdat, moov].concat();
        assert!(is_mp4(&file));
        let demuxed = demux(&file).expect("roundtrip demux");
        assert_eq!(demuxed.descriptors, descriptors);
        assert_eq!(demuxed.media, samples.concat());
    }

    #[test]
    fn rejects_non_iamf() {
        assert!(demux(&boxed(*b"moov", &[])).is_err());
    }
}
