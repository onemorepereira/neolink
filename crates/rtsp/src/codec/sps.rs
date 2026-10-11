//! Picture size from H.264 and H.265 sequence parameter sets

use super::annexb::VideoCodec;
use super::bits::{unescape, BitError, BitReader};

/// Coded picture size after cropping
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PictureSize {
    pub width: u32,
    pub height: u32,
}

/// The size from an SPS NAL (with its NAL header)
pub fn picture_size(codec: VideoCodec, sps: &[u8]) -> Result<PictureSize, BitError> {
    match codec {
        VideoCodec::H264 => h264(sps),
        VideoCodec::H265 => h265(sps),
    }
}

fn crop_units(chroma_format_idc: u32, separate_planes: bool) -> (u32, u32) {
    // (SubWidthC, SubHeightC); 4:0:0 and separate planes crop in luma samples
    match (chroma_format_idc, separate_planes) {
        (1, false) => (2, 2),
        (2, false) => (2, 1),
        _ => (1, 1),
    }
}

fn h264(nal: &[u8]) -> Result<PictureSize, BitError> {
    let rbsp = unescape(nal.get(1..).ok_or(BitError)?);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.bits(8)?;
    r.skip(16)?; // constraint flags, level_idc
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut separate_planes = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_planes = r.bit()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            let n = r.ue()?;
            if n > 255 {
                return Err(BitError);
            }
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? as u64 + 1;
    let height_map_units = r.ue()? as u64 + 1;
    let frame_mbs_only = r.bit()?;
    if !frame_mbs_only {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let (mut crop_x, mut crop_y) = (0u64, 0u64);
    if r.bit()? {
        let (l, rt, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (sub_w, sub_h) = crop_units(chroma_format_idc, separate_planes);
        let frame_factor = if frame_mbs_only { 1 } else { 2 };
        crop_x = sub_w as u64 * (l as u64 + rt as u64);
        crop_y = sub_h as u64 * frame_factor * (t as u64 + b as u64);
    }
    let width = (width_mbs * 16).checked_sub(crop_x).ok_or(BitError)?;
    let height = ((2 - frame_mbs_only as u64) * height_map_units * 16)
        .checked_sub(crop_y)
        .ok_or(BitError)?;
    size(width, height)
}

fn h265(nal: &[u8]) -> Result<PictureSize, BitError> {
    let rbsp = unescape(nal.get(2..).ok_or(BitError)?);
    let mut r = BitReader::new(&rbsp);
    r.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)?;
    r.bit()?; // sps_temporal_id_nesting_flag
              // profile_tier_level(1, max_sub_layers_minus1)
    r.skip(96)?; // general profile, tier, flags, level
    let mut present = vec![];
    for _ in 0..max_sub_layers_minus1 {
        present.push((r.bit()?, r.bit()?));
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?; // reserved_zero_2bits
        }
    }
    for (profile, level) in present {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()?;
    let mut separate_planes = false;
    if chroma_format_idc == 3 {
        separate_planes = r.bit()?;
    }
    let width = r.ue()? as u64;
    let height = r.ue()? as u64;
    let (mut crop_x, mut crop_y) = (0u64, 0u64);
    if r.bit()? {
        // conformance_window_flag
        let (l, rt, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (sub_w, sub_h) = crop_units(chroma_format_idc, separate_planes);
        crop_x = sub_w as u64 * (l as u64 + rt as u64);
        crop_y = sub_h as u64 * (t as u64 + b as u64);
    }
    size(
        width.checked_sub(crop_x).ok_or(BitError)?,
        height.checked_sub(crop_y).ok_or(BitError)?,
    )
}

fn size(width: u64, height: u64) -> Result<PictureSize, BitError> {
    // Anything larger than 16K is a corrupt SPS
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(BitError);
    }
    Ok(PictureSize {
        width: width as u32,
        height: height as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The Argus PT Ultra's own parameter sets, from the golden transcripts
    #[test]
    fn test_argus_sps() {
        let sub = hex("67640029ac1514a0e01064");
        assert_eq!(
            picture_size(VideoCodec::H264, &sub).unwrap(),
            PictureSize {
                width: 896,
                height: 512
            }
        );
        let main = hex("42010101600000030000030000030000030096a001e020021c7f8aad3ba24bb2");
        assert_eq!(
            picture_size(VideoCodec::H265, &main).unwrap(),
            PictureSize {
                width: 3840,
                height: 2160
            }
        );
    }

    #[test]
    fn test_garbage_does_not_panic() {
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let _ = picture_size(VideoCodec::H264, &data);
            let _ = picture_size(VideoCodec::H265, &data);
        }
        let _ = picture_size(VideoCodec::H264, &[0x67; 64]);
        let _ = picture_size(VideoCodec::H265, &[0xff; 64]);
    }
}
