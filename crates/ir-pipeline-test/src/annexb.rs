//! Annex-B (start-code) H.264 bitstream → AVCC (length-prefixed) packaging,
//! plus AVCDecoderConfigurationRecord construction. openh264 emits Annex-B;
//! MP4 and WebCodecs want AVCC.

/// Split an Annex-B stream into NAL units (without start codes).
/// Handles both 3-byte and 4-byte start codes.
pub fn split_nals(annexb: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 {
            if annexb[i + 2] == 1 {
                starts.push((i, i + 3));
                i += 3;
                continue;
            }
            if i + 4 <= annexb.len() && annexb[i + 2] == 0 && annexb[i + 3] == 1 {
                starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    for (n, &(sc_start, payload_start)) in starts.iter().enumerate() {
        let _ = sc_start;
        let end = starts
            .get(n + 1)
            .map_or(annexb.len(), |&(next_sc, _)| next_sc);
        // Trailing zero bytes before the next start code belong to it.
        let mut end = end;
        while end > payload_start && annexb[end - 1] == 0 {
            end -= 1;
        }
        if end > payload_start {
            nals.push(&annexb[payload_start..end]);
        }
    }
    nals
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1F)
}

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Build an AVCDecoderConfigurationRecord (`avcC` box payload) from SPS/PPS.
pub fn build_avcc_record(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(11 + sps.len() + pps.len());
    r.push(1); // configurationVersion
    r.push(sps[1]); // AVCProfileIndication
    r.push(sps[2]); // profile_compatibility
    r.push(sps[3]); // AVCLevelIndication
    r.push(0xFF); // lengthSizeMinusOne = 3 (4-byte lengths)
    r.push(0xE1); // numOfSequenceParameterSets = 1
    r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    r.extend_from_slice(sps);
    r.push(1); // numOfPictureParameterSets
    r.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    r.extend_from_slice(pps);
    r
}

/// Repackage NALs as AVCC (4-byte length prefixes), dropping SPS/PPS/AUD
/// (parameter sets live in the avcC record; AUDs are noise here).
pub fn to_avcc_payload(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        match nal_type(nal) {
            NAL_SPS | NAL_PPS | NAL_AUD => continue,
            _ => {
                out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    out
}

/// Convert an AVCC payload (4-byte length-prefixed NALs) back to Annex-B.
/// Used by tests to feed openh264's decoder, and later by any player path
/// that wants Annex-B.
pub fn avcc_to_annexb(avcc: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    let mut pos = 0;
    while pos + 4 <= avcc.len() {
        let len = u32::from_be_bytes(avcc[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[pos..pos + len]);
        pos += len;
    }
    out
}

/// Extract SPS/PPS from an AVCDecoderConfigurationRecord as an Annex-B
/// stream (what a decoder needs before the first IDR).
pub fn parameter_sets_annexb(record: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 5;
    let num_sps = (record[pos] & 0x1F) as usize;
    pos += 1;
    for _ in 0..num_sps {
        let len = u16::from_be_bytes(record[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&record[pos..pos + len]);
        pos += len;
    }
    let num_pps = record[pos] as usize;
    pos += 1;
    for _ in 0..num_pps {
        let len = u16::from_be_bytes(record[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&record[pos..pos + len]);
        pos += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_annexb_roundtrip() {
        let idr = [0x65u8, 1, 2, 3];
        let sei = [0x06u8, 9];
        let avcc = [&4u32.to_be_bytes()[..], &idr, &2u32.to_be_bytes()[..], &sei].concat();
        let annexb = avcc_to_annexb(&avcc);
        let nals = split_nals(&annexb);
        assert_eq!(nals, vec![&idr[..], &sei[..]]);
    }

    #[test]
    fn extracts_parameter_sets_from_record() {
        let sps = [0x67, 0x42, 0xC0, 0x1E];
        let pps = [0x68, 0xCE];
        let rec = build_avcc_record(&sps, &pps);
        let annexb = parameter_sets_annexb(&rec);
        let nals = split_nals(&annexb);
        assert_eq!(nals, vec![&sps[..], &pps[..]]);
    }

    #[test]
    fn splits_mixed_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS, 4-byte sc
            0, 0, 1, 0x68, 0xBB, // PPS, 3-byte sc
            0, 0, 0, 1, 0x65, 0x11, 0x22, // IDR slice
        ];
        let nals = split_nals(&stream);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), NAL_SPS);
        assert_eq!(nal_type(nals[1]), NAL_PPS);
        assert_eq!(nal_type(nals[2]), 5);
        assert_eq!(nals[2], &[0x65, 0x11, 0x22]);
    }

    #[test]
    fn avcc_payload_drops_parameter_sets() {
        let sps = [0x67, 0x42, 0xC0, 0x1E];
        let pps = [0x68, 0xCE];
        let idr = [0x65, 0x88, 0x84];
        let nals: Vec<&[u8]> = vec![&sps, &pps, &idr];
        let avcc = to_avcc_payload(&nals);
        assert_eq!(avcc, [&3u32.to_be_bytes()[..], &idr[..]].concat());
    }

    #[test]
    fn avcc_record_layout() {
        let sps = [0x67, 0x42, 0xC0, 0x1E, 0x01];
        let pps = [0x68, 0xCE];
        let rec = build_avcc_record(&sps, &pps);
        assert_eq!(rec[0], 1);
        assert_eq!(rec[1], 0x42);
        assert_eq!(rec[3], 0x1E);
        assert_eq!(rec[4] & 0x03, 3); // 4-byte lengths
        assert_eq!(rec[5] & 0x1F, 1); // one SPS
        assert_eq!(&rec[8..13], &sps);
    }
}
