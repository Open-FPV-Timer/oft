//! Consistent Overhead Byte Stuffing. Encoded data contains no `0x00` bytes.

/// Worst-case encoded length for `n` input bytes (no delimiter).
pub const fn max_encoded_len(n: usize) -> usize {
    n + n / 254 + 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CobsError {
    /// Output buffer too small.
    Overflow,
    /// A code byte points past the end of the input, or a zero byte was found.
    Malformed,
}

/// Encode `src` into `dst`. Returns the encoded length. Does not append the delimiter.
pub fn encode(src: &[u8], dst: &mut [u8]) -> Result<usize, CobsError> {
    if dst.len() < max_encoded_len(src.len()) {
        return Err(CobsError::Overflow);
    }
    let mut code_idx = 0;
    let mut out = 1;
    let mut code: u8 = 1;
    for &b in src {
        if b == 0 {
            dst[code_idx] = code;
            code_idx = out;
            out += 1;
            code = 1;
        } else {
            dst[out] = b;
            out += 1;
            code += 1;
            if code == 0xFF {
                dst[code_idx] = code;
                code_idx = out;
                out += 1;
                code = 1;
            }
        }
    }
    dst[code_idx] = code;
    Ok(out)
}

/// Decode `src` (without delimiter) into `dst`. Returns the decoded length.
pub fn decode(src: &[u8], dst: &mut [u8]) -> Result<usize, CobsError> {
    let mut i = 0;
    let mut out = 0;
    while i < src.len() {
        let code = src[i];
        if code == 0 {
            return Err(CobsError::Malformed);
        }
        i += 1;
        let run = code as usize - 1;
        if i + run > src.len() {
            return Err(CobsError::Malformed);
        }
        if out + run > dst.len() {
            return Err(CobsError::Overflow);
        }
        for k in 0..run {
            let b = src[i + k];
            if b == 0 {
                return Err(CobsError::Malformed);
            }
            dst[out + k] = b;
        }
        out += run;
        i += run;
        // A zero follows every block except a full 254-byte block and the final block.
        if code != 0xFF && i < src.len() {
            if out >= dst.len() {
                return Err(CobsError::Overflow);
            }
            dst[out] = 0;
            out += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let mut enc = [0u8; 2048];
        let mut dec = [0u8; 2048];
        let n = encode(data, &mut enc).unwrap();
        assert!(!enc[..n].contains(&0), "encoded data must not contain zero");
        assert!(n <= max_encoded_len(data.len()));
        let m = decode(&enc[..n], &mut dec).unwrap();
        assert_eq!(&dec[..m], data);
    }

    #[test]
    fn known_vectors() {
        let mut enc = [0u8; 16];
        let n = encode(&[0x11, 0x22, 0x00, 0x33], &mut enc).unwrap();
        assert_eq!(&enc[..n], &[0x03, 0x11, 0x22, 0x02, 0x33]);
        let n = encode(&[0x00], &mut enc).unwrap();
        assert_eq!(&enc[..n], &[0x01, 0x01]);
        let n = encode(&[], &mut enc).unwrap();
        assert_eq!(&enc[..n], &[0x01]);
    }

    #[test]
    fn roundtrips() {
        roundtrip(&[]);
        roundtrip(&[0]);
        roundtrip(&[0, 0, 0]);
        roundtrip(&[1, 2, 3]);
        let mut v = [0u8; 1000];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        roundtrip(&v);
        // Runs around the 254-byte block boundary.
        for len in [253, 254, 255, 508, 509] {
            let ones = [1u8; 600];
            roundtrip(&ones[..len]);
        }
    }

    #[test]
    fn rejects_truncated() {
        let mut dec = [0u8; 16];
        assert_eq!(decode(&[0x05, 1, 2], &mut dec), Err(CobsError::Malformed));
    }
}
