//! Parsing helpers shared by the Tcl commands.

/// Parses an OpenOCD-style number: `0x` hex, `0b` binary, `0o`/leading-zero
/// octal is not used by cfg files so plain decimal otherwise. Negative
/// values wrap to their two's complement.
pub fn parse_u64(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        u64::from_str_radix(b, 2)
    } else {
        t.parse::<u64>()
    }
    .map_err(|_| format!("invalid number: \"{s}\""))?;
    Ok(if neg { (v as i64).wrapping_neg() as u64 } else { v })
}

pub fn parse_u32(s: &str) -> Result<u32, String> {
    let v = parse_u64(s)?;
    if v > u32::MAX as u64 && (v as i64) < i32::MIN as i64 {
        return Err(format!("number out of range: \"{s}\""));
    }
    Ok(v as u32)
}

pub fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "on" | "enable" | "1" | "true" | "yes" => Ok(true),
        "off" | "disable" | "0" | "false" | "no" => Ok(false),
        _ => Err(format!("expected on|off, got \"{s}\"")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers() {
        assert_eq!(parse_u64("0xF881FD00").unwrap(), 0xF881FD00);
        assert_eq!(parse_u64("5000").unwrap(), 5000);
        assert_eq!(parse_u32("-1").unwrap(), 0xFFFF_FFFF);
        assert!(parse_u64("zz").is_err());
    }
}
