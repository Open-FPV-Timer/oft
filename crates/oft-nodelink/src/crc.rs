//! CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection, no final xor.

/// Compute CRC-16/CCITT-FALSE over `data`.
pub const fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    let mut i = 0;
    while i < data.len() {
        crc ^= (data[i] as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            bit += 1;
        }
        i += 1;
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_value() {
        // Standard check value for CRC-16/CCITT-FALSE.
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn empty() {
        assert_eq!(crc16(&[]), 0xFFFF);
    }
}
