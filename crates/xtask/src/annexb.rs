//! Just enough Annex-B and ADTS parsing to cut encoder output into camera-sized units

/// Byte ranges of the NAL units in an Annex-B buffer, start codes included
fn nal_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = vec![];
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // Include the leading zero of a 4-byte start code
            starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = vec![];
    for (n, &s) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(data.len());
        out.push((s, end));
    }
    out
}

/// The NAL header offset within a range (past the start code)
fn header_at(data: &[u8], (s, _): (usize, usize)) -> usize {
    if data[s + 2] == 1 {
        s + 3
    } else {
        s + 4
    }
}

/// One access unit
pub struct AccessUnit {
    /// Annex-B bytes with start codes
    pub data: Vec<u8>,
    /// Contains an IRAP/IDR picture
    pub irap: bool,
}

/// Split an Annex-B elementary stream into access units
///
/// A new AU starts at a parameter set, AUD or prefix SEI that follows a VCL NAL, or at a
/// VCL NAL that begins a new picture (first slice flag / `first_mb_in_slice == 0`).
pub fn split_access_units(data: &[u8], h265: bool) -> Vec<AccessUnit> {
    let mut aus: Vec<AccessUnit> = vec![];
    let mut cur_start: Option<usize> = None;
    let mut cur_has_vcl = false;
    let mut cur_irap = false;
    for range in nal_ranges(data) {
        let h = header_at(data, range);
        if h >= range.1 {
            continue;
        }
        let (vcl, first_slice, starter, irap) = if h265 {
            let t = (data[h] >> 1) & 0x3f;
            let vcl = t < 32;
            let first = vcl && data.get(h + 2).map(|b| b & 0x80 != 0).unwrap_or(false);
            let starter = matches!(t, 32..=35 | 39 | 41..=44 | 48..=55);
            (vcl, first, starter, (16..=21).contains(&t))
        } else {
            let t = data[h] & 0x1f;
            let vcl = (1..=5).contains(&t);
            let first = vcl && data.get(h + 1).map(|b| b & 0x80 != 0).unwrap_or(false);
            let starter = matches!(t, 6..=9 | 14..=18);
            (vcl, first, starter, t == 5)
        };
        let boundary = cur_has_vcl && (starter || first_slice);
        if boundary {
            aus.push(AccessUnit {
                data: data[cur_start.unwrap()..range.0].to_vec(),
                irap: cur_irap,
            });
            cur_start = None;
            cur_has_vcl = false;
            cur_irap = false;
        }
        if cur_start.is_none() {
            cur_start = Some(range.0);
        }
        cur_has_vcl |= vcl;
        cur_irap |= irap;
    }
    if let (Some(s), true) = (cur_start, cur_has_vcl) {
        aus.push(AccessUnit {
            data: data[s..].to_vec(),
            irap: cur_irap,
        });
    }
    aus
}

/// Split an ADTS stream into frames (headers included)
pub fn split_adts(data: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut out = vec![];
    let mut i = 0;
    while i < data.len() {
        anyhow::ensure!(
            i + 7 <= data.len() && data[i] == 0xff && data[i + 1] & 0xf0 == 0xf0,
            "bad ADTS sync at {}",
            i
        );
        let len = (((data[i + 3] & 3) as usize) << 11)
            | ((data[i + 4] as usize) << 3)
            | ((data[i + 5] as usize) >> 5);
        anyhow::ensure!(
            len >= 7 && i + len <= data.len(),
            "bad ADTS length at {}",
            i
        );
        out.push(data[i..i + len].to_vec());
        i += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_h265() {
        // VPS SPS PPS IDR(first) IDR(second slice) | TRAIL_R(first) | TRAIL_R(first)
        let es: Vec<u8> = [
            &[0, 0, 0, 1, 0x40, 1, 0xaa][..],
            &[0, 0, 0, 1, 0x42, 1, 0xaa],
            &[0, 0, 0, 1, 0x44, 1, 0xaa],
            &[0, 0, 1, 0x26, 1, 0x80, 0xaa],
            &[0, 0, 1, 0x26, 1, 0x00, 0xaa],
            &[0, 0, 1, 0x02, 1, 0x80, 0xaa],
            &[0, 0, 1, 0x02, 1, 0x80, 0xaa],
        ]
        .concat();
        let aus = split_access_units(&es, true);
        assert_eq!(aus.len(), 3);
        assert!(aus[0].irap && !aus[1].irap && !aus[2].irap);
        assert_eq!(aus[0].data.len(), 7 * 3 + 7 * 2);
        assert_eq!(aus[1].data, vec![0, 0, 1, 0x02, 1, 0x80, 0xaa]);
    }

    #[test]
    fn test_split_h264() {
        // SPS PPS IDR | non-IDR | non-IDR
        let es: Vec<u8> = [
            &[0, 0, 0, 1, 0x67, 0x64][..],
            &[0, 0, 0, 1, 0x68, 0xee],
            &[0, 0, 0, 1, 0x65, 0x88],
            &[0, 0, 0, 1, 0x41, 0x9a],
            &[0, 0, 0, 1, 0x41, 0x9a],
        ]
        .concat();
        let aus = split_access_units(&es, false);
        assert_eq!(aus.len(), 3);
        assert!(aus[0].irap && !aus[1].irap);
        assert_eq!(aus[0].data.len(), 18);
    }

    #[test]
    fn test_split_adts() {
        let frame = [
            0xff, 0xf1, 0x60, 0x40, 0x01, 0x7f, 0xfc, 0x01, 0x18, 0x20, 0x07,
        ];
        let stream = [frame, frame].concat();
        let frames = split_adts(&stream).unwrap();
        assert_eq!(frames, vec![frame.to_vec(), frame.to_vec()]);
        assert!(split_adts(&stream[..15]).is_err());
    }
}
