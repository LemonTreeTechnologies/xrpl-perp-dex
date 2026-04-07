//! Shared types for the Perp DEX orchestrator.
//!
//! FP8 is the fixed-point format used throughout: 8 decimal places,
//! represented as i64 internally (1.0 = 100_000_000).

use std::fmt;
use std::ops::{Add, Sub, Mul, Div, Neg};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ── FP8 fixed-point type ────────────────────────────────────────

const FP8_SCALE: i64 = 100_000_000; // 10^8

/// Fixed-point number with 8 decimal places.
/// Internal representation: value * 10^8 stored as i64.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FP8(pub i64);

#[allow(dead_code)]
impl FP8 {
    pub const ZERO: FP8 = FP8(0);
    pub const ONE: FP8 = FP8(FP8_SCALE);
    pub const SCALE: i64 = FP8_SCALE;

    /// Create from a float (lossy — use for price feeds only).
    pub fn from_f64(val: f64) -> Self {
        FP8((val * FP8_SCALE as f64).round() as i64)
    }

    /// Convert to f64 (lossy).
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / FP8_SCALE as f64
    }

    /// Raw inner value.
    pub fn raw(self) -> i64 {
        self.0
    }

    /// Absolute value.
    pub fn abs(self) -> Self {
        FP8(self.0.abs())
    }
}

impl fmt::Display for FP8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let whole = abs / FP8_SCALE as u64;
        let frac = abs % FP8_SCALE as u64;
        write!(f, "{}{}.{:08}", sign, whole, frac)
    }
}

impl FromStr for FP8 {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let negative = s.starts_with('-');
        let s = s.trim_start_matches('-');

        let (whole_str, frac_str) = match s.split_once('.') {
            Some((w, f)) => (w, f),
            None => (s, ""),
        };

        let whole: i64 = if whole_str.is_empty() {
            0
        } else {
            whole_str.parse()?
        };

        // Pad or truncate fractional part to exactly 8 digits
        let frac_padded = format!("{:0<8}", frac_str);
        let frac: i64 = frac_padded[..8].parse()?;

        let mut val = whole * FP8_SCALE + frac;
        if negative {
            val = -val;
        }
        Ok(FP8(val))
    }
}

impl Add for FP8 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        FP8(self.0 + rhs.0)
    }
}

impl Sub for FP8 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        FP8(self.0 - rhs.0)
    }
}

impl Mul for FP8 {
    type Output = Self;
    /// Multiply two FP8 values (result is rounded).
    fn mul(self, rhs: Self) -> Self {
        FP8(((self.0 as i128 * rhs.0 as i128) / FP8_SCALE as i128) as i64)
    }
}

impl Div for FP8 {
    type Output = Self;
    /// Divide two FP8 values. Returns ZERO on division by zero (safe default).
    fn div(self, rhs: Self) -> Self {
        if rhs.0 == 0 {
            return FP8::ZERO;
        }
        FP8(((self.0 as i128 * FP8_SCALE as i128) / rhs.0 as i128) as i64)
    }
}

impl Neg for FP8 {
    type Output = Self;
    fn neg(self) -> Self {
        FP8(-self.0)
    }
}

impl Serialize for FP8 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for FP8 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        FP8::from_str(&s).map_err(serde::de::Error::custom)
    }
}

// ── Enums ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Long => write!(f, "long"),
            Side::Short => write!(f, "short"),
        }
    }
}

impl FromStr for Side {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "long" => Ok(Side::Long),
            "short" => Ok(Side::Short),
            _ => anyhow::bail!("invalid side: {}", s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum PositionStatus {
    Open,
    Closed,
    Liquidated,
}

impl fmt::Display for PositionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PositionStatus::Open => write!(f, "open"),
            PositionStatus::Closed => write!(f, "closed"),
            PositionStatus::Liquidated => write!(f, "liquidated"),
        }
    }
}

// ── Structs matching enclave JSON responses ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Position {
    pub position_id: u64,
    pub user_id: String,
    pub side: Side,
    pub size: FP8,
    pub entry_price: FP8,
    pub leverage: u32,
    pub margin: FP8,
    pub status: PositionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Balance {
    pub available: FP8,
    pub locked: FP8,
    pub total: FP8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct UserBalance {
    pub user_id: String,
    pub balance: Balance,
    pub positions: Vec<Position>,
    pub unrealized_pnl: FP8,
}

// ── Helpers ─────────────────────────────────────────────────────

/// Convert f64 to FP8 string for JSON payloads (e.g., "0.55000000").
pub fn float_to_fp8_string(val: f64) -> String {
    FP8::from_f64(val).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fp8_display() {
        assert_eq!(FP8(123456789).to_string(), "1.23456789");
        assert_eq!(FP8(0).to_string(), "0.00000000");
        assert_eq!(FP8(-50000000).to_string(), "-0.50000000");
        assert_eq!(FP8(10000000000).to_string(), "100.00000000");
    }

    #[test]
    fn fp8_parse() {
        assert_eq!("1.23456789".parse::<FP8>().unwrap(), FP8(123456789));
        assert_eq!("100.00000000".parse::<FP8>().unwrap(), FP8(10000000000));
        assert_eq!("-0.50000000".parse::<FP8>().unwrap(), FP8(-50000000));
        assert_eq!("0".parse::<FP8>().unwrap(), FP8(0));
    }

    #[test]
    fn fp8_arithmetic() {
        let a = FP8::from_f64(1.5);
        let b = FP8::from_f64(2.0);
        assert_eq!((a + b).to_string(), "3.50000000");
        assert_eq!((a * b).to_string(), "3.00000000");
        assert_eq!((a / b).to_string(), "0.75000000");
    }

    #[test]
    fn fp8_subtraction() {
        let a = FP8::from_f64(5.0);
        let b = FP8::from_f64(3.0);
        assert_eq!((a - b).to_string(), "2.00000000");
        assert_eq!((b - a).to_string(), "-2.00000000");
    }

    #[test]
    fn fp8_negation() {
        let a = FP8::from_f64(1.5);
        assert_eq!((-a).to_string(), "-1.50000000");
        assert_eq!((-(-a)).to_string(), "1.50000000");
    }

    #[test]
    fn fp8_abs() {
        assert_eq!(FP8::from_f64(-3.5).abs(), FP8::from_f64(3.5));
        assert_eq!(FP8::from_f64(3.5).abs(), FP8::from_f64(3.5));
        assert_eq!(FP8::ZERO.abs(), FP8::ZERO);
    }

    #[test]
    fn fp8_raw_roundtrip() {
        let v = FP8(123456789);
        assert_eq!(v.raw(), 123456789);
        assert_eq!(FP8::ZERO.raw(), 0);
    }

    #[test]
    fn fp8_from_f64_precision() {
        // Small value
        assert_eq!(FP8::from_f64(0.00000001).raw(), 1);
        // Large value
        assert_eq!(FP8::from_f64(1000.0).raw(), 100000000000);
    }

    #[test]
    fn fp8_comparison() {
        let a = FP8::from_f64(0.55);
        let b = FP8::from_f64(0.56);
        assert!(a < b);
        assert!(b > a);
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    #[test]
    fn fp8_serde_roundtrip() {
        let val = FP8::from_f64(1.23456789);
        let json = serde_json::to_string(&val).unwrap();
        let decoded: FP8 = serde_json::from_str(&json).unwrap();
        assert_eq!(val, decoded);
    }

    #[test]
    fn fp8_parse_short_fraction() {
        // "1.5" should work (not requiring 8 decimal places)
        assert_eq!("1.5".parse::<FP8>().unwrap(), FP8(150000000));
        assert_eq!("0.1".parse::<FP8>().unwrap(), FP8(10000000));
    }

    #[test]
    fn fp8_parse_integer() {
        assert_eq!("42".parse::<FP8>().unwrap(), FP8(4200000000));
    }

    #[test]
    fn side_from_str() {
        assert_eq!("long".parse::<Side>().unwrap(), Side::Long);
        assert_eq!("short".parse::<Side>().unwrap(), Side::Short);
        assert_eq!("LONG".parse::<Side>().unwrap(), Side::Long);
        assert_eq!("SHORT".parse::<Side>().unwrap(), Side::Short);
        assert!("invalid".parse::<Side>().is_err());
    }

    #[test]
    fn side_display() {
        assert_eq!(format!("{}", Side::Long), "long");
        assert_eq!(format!("{}", Side::Short), "short");
    }

    #[test]
    fn float_to_fp8_string_helper() {
        assert_eq!(float_to_fp8_string(0.55), "0.55000000");
        assert_eq!(float_to_fp8_string(100.0), "100.00000000");
        assert_eq!(float_to_fp8_string(0.0), "0.00000000");
    }
}
