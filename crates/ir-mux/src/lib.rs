//! Minimal in-memory MP4 muxer for a single H.264 video track.
//!
//! Produces a moov-first ("fast start") progressive MP4 from already-encoded
//! AVCC packets: exact per-frame durations in `stts`, keyframes in `stss`,
//! BT.709 tagging in `colr`. One chunk holding all samples keeps
//! `stsc`/`stco` trivial. No B-frames are supported (pts == dts), which is
//! the pipeline contract everywhere in this project.

pub mod h264;

use ir_types::{Codec, CodecConfig, ColorInfo, EncodedPacket};

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("no packets to mux")]
    Empty,
    #[error("first packet must be a keyframe")]
    NoLeadingKeyframe,
    #[error("packet pts must be strictly increasing")]
    NonMonotonicPts,
}

/// Media timescale: 90 kHz, the conventional choice for video.
const TIMESCALE: u64 = 90_000;
/// Movie-header timescale (coarse is fine; only used for durations).
const MOVIE_TIMESCALE: u64 = 1_000;

/// Mux packets (whole GOPs, first packet a keyframe, pts ascending) into a
/// complete MP4 file in memory.
pub fn mux_h264(codec: &CodecConfig, packets: &[EncodedPacket]) -> Result<Vec<u8>, MuxError> {
    if packets.is_empty() {
        return Err(MuxError::Empty);
    }
    if !packets[0].keyframe {
        return Err(MuxError::NoLeadingKeyframe);
    }
    if packets.windows(2).any(|w| w[1].pts <= w[0].pts) {
        return Err(MuxError::NonMonotonicPts);
    }

    let Codec::H264 { avcc } = &codec.codec;

    // Cumulative tick positions (rebased so the first sample lands at 0),
    // then diffed — per-sample rounding this way cannot drift.
    let base = packets[0].pts;
    let ticks: Vec<u64> = packets
        .iter()
        .map(|p| to_ticks(p.pts - base))
        .chain(std::iter::once(to_ticks(
            packets.last().unwrap().end_pts() - base,
        )))
        .collect();
    let deltas: Vec<u32> = ticks.windows(2).map(|w| (w[1] - w[0]) as u32).collect();
    let media_duration: u64 = ticks[ticks.len() - 1];
    let movie_duration = media_duration * MOVIE_TIMESCALE / TIMESCALE;

    let ftyp = boxed(
        b"ftyp",
        &[
            b"isom".as_slice(),
            &0x200u32.to_be_bytes(),
            b"isom",
            b"iso2",
            b"avc1",
            b"mp41",
        ]
        .concat(),
    );

    let mdat_payload_len: usize = packets.iter().map(|p| p.data.len()).sum();

    // stco holds the absolute file offset of the single chunk. moov's size
    // doesn't depend on the offset's value (fixed 4 bytes), so build moov
    // with a placeholder, then patch.
    let mut moov = build_moov(
        codec,
        avcc,
        &deltas,
        packets,
        media_duration,
        movie_duration,
        0,
    );
    let chunk_offset = (ftyp.len() + moov.len() + 8) as u32;
    moov = build_moov(
        codec,
        avcc,
        &deltas,
        packets,
        media_duration,
        movie_duration,
        chunk_offset,
    );

    let mut out = Vec::with_capacity(ftyp.len() + moov.len() + 8 + mdat_payload_len);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&((mdat_payload_len + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for p in packets {
        out.extend_from_slice(&p.data);
    }
    Ok(out)
}

fn to_ticks(d: std::time::Duration) -> u64 {
    // Round to nearest tick.
    (d.as_nanos() as u64 * TIMESCALE + 500_000_000) / 1_000_000_000
}

#[allow(clippy::too_many_arguments)]
fn build_moov(
    codec: &CodecConfig,
    avcc: &[u8],
    deltas: &[u32],
    packets: &[EncodedPacket],
    media_duration: u64,
    movie_duration: u64,
    chunk_offset: u32,
) -> Vec<u8> {
    let mvhd = full_box(b"mvhd", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        p.extend_from_slice(&(MOVIE_TIMESCALE as u32).to_be_bytes());
        p.extend_from_slice(&(movie_duration as u32).to_be_bytes());
        p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        p.extend_from_slice(&[0u8; 10]); // reserved
        p.extend_from_slice(&identity_matrix());
        p.extend_from_slice(&[0u8; 24]); // pre_defined
        p.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID
        p
    });

    let tkhd = full_box(b"tkhd", 0, 0x7, &{
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        p.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        p.extend_from_slice(&1u32.to_be_bytes()); // track_ID
        p.extend_from_slice(&0u32.to_be_bytes()); // reserved
        p.extend_from_slice(&(movie_duration as u32).to_be_bytes());
        p.extend_from_slice(&[0u8; 8]); // reserved
        p.extend_from_slice(&0u16.to_be_bytes()); // layer
        p.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
        p.extend_from_slice(&0u16.to_be_bytes()); // volume (video)
        p.extend_from_slice(&0u16.to_be_bytes()); // reserved
        p.extend_from_slice(&identity_matrix());
        p.extend_from_slice(&(codec.width << 16).to_be_bytes());
        p.extend_from_slice(&(codec.height << 16).to_be_bytes());
        p
    });

    let mdhd = full_box(b"mdhd", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&(TIMESCALE as u32).to_be_bytes());
        p.extend_from_slice(&(media_duration as u32).to_be_bytes());
        p.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
        p.extend_from_slice(&0u16.to_be_bytes());
        p
    });

    let hdlr = full_box(b"hdlr", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(b"vide");
        p.extend_from_slice(&[0u8; 12]);
        p.extend_from_slice(b"VideoHandler\0");
        p
    });

    let vmhd = full_box(b"vmhd", 0, 1, &[0u8; 8]);
    let dref = full_box(b"dref", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&full_box(b"url ", 0, 1, &[])); // self-contained
        p
    });
    let dinf = boxed(b"dinf", &dref);

    // --- stbl ---
    let avc1 = build_avc1(codec, avcc);
    let stsd = full_box(b"stsd", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&avc1);
        p
    });

    // stts: run-length encode consecutive equal deltas.
    let stts = full_box(b"stts", 0, 0, &{
        let mut entries: Vec<(u32, u32)> = Vec::new();
        for &d in deltas {
            match entries.last_mut() {
                Some((count, delta)) if *delta == d => *count += 1,
                _ => entries.push((1, d)),
            }
        }
        let mut p = Vec::new();
        p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (count, delta) in entries {
            p.extend_from_slice(&count.to_be_bytes());
            p.extend_from_slice(&delta.to_be_bytes());
        }
        p
    });

    let stss = full_box(b"stss", 0, 0, &{
        let keyframes: Vec<u32> = packets
            .iter()
            .enumerate()
            .filter(|(_, p)| p.keyframe)
            .map(|(i, _)| i as u32 + 1) // 1-based sample numbers
            .collect();
        let mut p = Vec::new();
        p.extend_from_slice(&(keyframes.len() as u32).to_be_bytes());
        for k in keyframes {
            p.extend_from_slice(&k.to_be_bytes());
        }
        p
    });

    // All samples in one chunk.
    let stsc = full_box(b"stsc", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
        p.extend_from_slice(&(packets.len() as u32).to_be_bytes()); // samples_per_chunk
        p.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
        p
    });

    let stsz = full_box(b"stsz", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_be_bytes()); // sample_size: per-sample
        p.extend_from_slice(&(packets.len() as u32).to_be_bytes());
        for pkt in packets {
            p.extend_from_slice(&(pkt.data.len() as u32).to_be_bytes());
        }
        p
    });

    let stco = full_box(b"stco", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&chunk_offset.to_be_bytes());
        p
    });

    let stbl = boxed(b"stbl", &[stsd, stts, stss, stsc, stsz, stco].concat());
    let minf = boxed(b"minf", &[vmhd, dinf, stbl].concat());
    let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = boxed(b"trak", &[tkhd, mdia].concat());
    boxed(b"moov", &[mvhd, trak].concat())
}

fn build_avc1(codec: &CodecConfig, avcc: &[u8]) -> Vec<u8> {
    let avc_c = boxed(b"avcC", avcc);
    let colr = boxed(b"colr", &{
        let mut p = Vec::new();
        p.extend_from_slice(b"nclx");
        let ColorInfo::Bt709Limited = codec.color;
        p.extend_from_slice(&1u16.to_be_bytes()); // primaries: BT.709
        p.extend_from_slice(&1u16.to_be_bytes()); // transfer: BT.709
        p.extend_from_slice(&1u16.to_be_bytes()); // matrix: BT.709
        p.push(0); // full_range_flag = 0 (limited)
        p
    });

    boxed(b"avc1", &{
        let mut p = Vec::new();
        p.extend_from_slice(&[0u8; 6]); // reserved
        p.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        p.extend_from_slice(&[0u8; 16]); // pre_defined/reserved
        p.extend_from_slice(&(codec.width as u16).to_be_bytes());
        p.extend_from_slice(&(codec.height as u16).to_be_bytes());
        p.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution 72dpi
        p.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution
        p.extend_from_slice(&0u32.to_be_bytes()); // reserved
        p.extend_from_slice(&1u16.to_be_bytes()); // frame_count
        p.extend_from_slice(&[0u8; 32]); // compressorname (empty, padded)
        p.extend_from_slice(&0x0018u16.to_be_bytes()); // depth 24
        p.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined
        p.extend_from_slice(&avc_c);
        p.extend_from_slice(&colr);
        p
    })
}

fn identity_matrix() -> [u8; 36] {
    let mut m = [0u8; 36];
    m[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    m[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    m[32..36].copy_from_slice(&0x4000_0000u32.to_be_bytes());
    m
}

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + payload.len());
    p.push(version);
    p.extend_from_slice(&flags.to_be_bytes()[1..]);
    p.extend_from_slice(payload);
    boxed(kind, &p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;

    fn codec() -> CodecConfig {
        CodecConfig {
            codec: Codec::H264 {
                avcc: Bytes::from_static(&[0x01, 0x42, 0xC0, 0x1E, 0xFF]),
            },
            width: 640,
            height: 480,
            nominal_fps: 60,
            color: ColorInfo::Bt709Limited,
        }
    }

    fn pkt(ms: u64, keyframe: bool, fill: u8, len: usize) -> EncodedPacket {
        EncodedPacket {
            pts: Duration::from_millis(ms),
            duration: Duration::from_millis(16),
            keyframe,
            data: Bytes::from(vec![fill; len]),
        }
    }

    /// Walk top-level boxes: (type, payload_range) pairs.
    fn top_boxes(data: &[u8]) -> Vec<([u8; 4], std::ops::Range<usize>)> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
            out.push((kind, pos + 8..pos + size));
            pos += size;
        }
        assert_eq!(pos, data.len(), "trailing garbage");
        out
    }

    fn find_box<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> &'a [u8] {
        let mut cur = data;
        for (i, want) in path.iter().enumerate() {
            let mut pos = 0;
            // stsd payload starts with version/flags + entry_count before
            // child boxes; handled by callers passing exact offsets. Here we
            // only descend through pure container boxes.
            let mut found = None;
            while pos + 8 <= cur.len() {
                let size = u32::from_be_bytes(cur[pos..pos + 4].try_into().unwrap()) as usize;
                let kind = &cur[pos + 4..pos + 8];
                if kind == *want {
                    found = Some(&cur[pos + 8..pos + size]);
                    break;
                }
                pos += size;
            }
            cur = found.unwrap_or_else(|| {
                panic!(
                    "box {:?} not found at depth {i}",
                    String::from_utf8_lossy(*want)
                )
            });
        }
        cur
    }

    fn packets_1s_gop(secs: u64) -> Vec<EncodedPacket> {
        (0..secs * 60)
            .map(|i| {
                pkt(
                    i * 1000 / 60,
                    i % 60 == 0,
                    (i % 251) as u8,
                    50 + (i as usize % 13),
                )
            })
            .collect()
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(mux_h264(&codec(), &[]), Err(MuxError::Empty)));
        assert!(matches!(
            mux_h264(&codec(), &[pkt(0, false, 0, 10)]),
            Err(MuxError::NoLeadingKeyframe)
        ));
        assert!(matches!(
            mux_h264(&codec(), &[pkt(10, true, 0, 10), pkt(10, false, 0, 10)]),
            Err(MuxError::NonMonotonicPts)
        ));
    }

    #[test]
    fn moov_before_mdat_and_offsets_correct() {
        let packets = packets_1s_gop(3);
        let mp4 = mux_h264(&codec(), &packets).unwrap();

        let tops = top_boxes(&mp4);
        let kinds: Vec<&[u8; 4]> = tops.iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, vec![b"ftyp", b"moov", b"mdat"]);

        // stco's single chunk offset must point at the first sample's bytes.
        let stbl = find_box(&mp4, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
        let stco = find_box(stbl, &[b"stco"]);
        let offset = u32::from_be_bytes(stco[8..12].try_into().unwrap()) as usize;
        assert_eq!(
            &mp4[offset..offset + packets[0].data.len()],
            &packets[0].data[..]
        );

        // mdat payload must start exactly at that offset.
        let (_, mdat_range) = tops.iter().find(|(k, _)| k == b"mdat").unwrap().clone();
        assert_eq!(mdat_range.start, offset);
    }

    #[test]
    fn sample_tables_match_input() {
        let packets = packets_1s_gop(2);
        let mp4 = mux_h264(&codec(), &packets).unwrap();
        let stbl = find_box(&mp4, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);

        // stsz: count and per-sample sizes.
        let stsz = find_box(stbl, &[b"stsz"]);
        assert_eq!(u32::from_be_bytes(stsz[4..8].try_into().unwrap()), 0);
        let count = u32::from_be_bytes(stsz[8..12].try_into().unwrap()) as usize;
        assert_eq!(count, packets.len());
        for (i, p) in packets.iter().enumerate() {
            let sz = u32::from_be_bytes(stsz[12 + i * 4..16 + i * 4].try_into().unwrap());
            assert_eq!(sz as usize, p.data.len());
        }

        // stss: exactly the keyframe sample numbers (1-based).
        let stss = find_box(stbl, &[b"stss"]);
        let n = u32::from_be_bytes(stss[4..8].try_into().unwrap()) as usize;
        assert_eq!(n, 2);
        assert_eq!(u32::from_be_bytes(stss[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_be_bytes(stss[12..16].try_into().unwrap()), 61);

        // stts: sum of (count * delta) == last end pts in ticks.
        let stts = find_box(stbl, &[b"stts"]);
        let entries = u32::from_be_bytes(stts[4..8].try_into().unwrap()) as usize;
        let mut total: u64 = 0;
        let mut samples: u64 = 0;
        for i in 0..entries {
            let c = u32::from_be_bytes(stts[8 + i * 8..12 + i * 8].try_into().unwrap()) as u64;
            let d = u32::from_be_bytes(stts[12 + i * 8..16 + i * 8].try_into().unwrap()) as u64;
            samples += c;
            total += c * d;
        }
        assert_eq!(samples, packets.len() as u64);
        let expect = super::to_ticks(packets.last().unwrap().end_pts() - packets[0].pts);
        assert_eq!(total, expect);
    }

    #[test]
    fn avcc_and_colr_embedded() {
        let packets = packets_1s_gop(1);
        let mp4 = mux_h264(&codec(), &packets).unwrap();
        let stbl = find_box(&mp4, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
        // stsd payload: version/flags(4) + entry_count(4), then avc1.
        let stsd = find_box(stbl, &[b"stsd"]);
        let avc1 = &stsd[8..];
        assert_eq!(&avc1[4..8], b"avc1");
        // Child boxes start after the 78-byte VisualSampleEntry header.
        let avc1_children = &avc1[8 + 78..];
        let avc_c = find_box(avc1_children, &[b"avcC"]);
        assert_eq!(avc_c, &[0x01, 0x42, 0xC0, 0x1E, 0xFF]);
        let colr = find_box(avc1_children, &[b"colr"]);
        assert_eq!(&colr[0..4], b"nclx");
        assert_eq!(colr[10], 0); // limited range
    }
}
