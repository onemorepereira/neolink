//! Bit reading for parameter sets: emulation-prevention removal and Exp-Golomb

/// Remove emulation-prevention bytes (`00 00 03` → `00 00`) from a NAL payload
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader over an RBSP; every read fails cleanly past the end
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Ran out of bits, or a value out of its legal range
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitError;

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Result<bool, BitError> {
        let byte = *self.data.get(self.pos / 8).ok_or(BitError)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1 == 1;
        self.pos += 1;
        Ok(bit)
    }

    /// Up to 32 bits
    pub fn bits(&mut self, n: u32) -> Result<u32, BitError> {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u32;
        }
        Ok(v)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), BitError> {
        if self.pos + n > self.data.len() * 8 {
            return Err(BitError);
        }
        self.pos += n;
        Ok(())
    }

    /// Unsigned Exp-Golomb, limited to 32-bit values
    pub fn ue(&mut self) -> Result<u32, BitError> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(BitError);
            }
        }
        let rest = self.bits(zeros)?;
        Ok(((1u64 << zeros) - 1 + rest as u64).min(u32::MAX as u64) as u32)
    }

    /// Signed Exp-Golomb
    pub fn se(&mut self) -> Result<i32, BitError> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unescape() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
        assert_eq!(unescape(&[1, 0, 3, 0]), vec![1, 0, 3, 0]);
    }

    #[test]
    fn test_exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101 -> ue 0, 1, 2, 3, 4
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&data);
        let v: Vec<u32> = (0..5).map(|_| r.ue().unwrap()).collect();
        assert_eq!(v, [0, 1, 2, 3, 4]);
        let mut r = BitReader::new(&[0b0100_1100]);
        assert_eq!((r.se().unwrap(), r.se().unwrap()), (1, -1));
        assert!(BitReader::new(&[0, 0, 0, 0, 0]).ue().is_err());
    }
}
