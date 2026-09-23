//! Exact decimal values for typed comparison.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An exact decimal value: `mantissa × 10^-scale`.
///
/// The representation is normalized on construction: trailing fractional zeros
/// are stripped (`1.10` becomes `11 × 10^-1`) and every zero, including `-0`,
/// becomes `0 × 10^0`. Equality and ordering are exact, so `1.10 == 1.1` and
/// `10 == 10.0`.
#[derive(Debug, Clone, Copy)]
pub struct ExactDecimal {
    mantissa: i128,
    scale: u32,
}

impl ExactDecimal {
    /// Build a value from an integer mantissa and a decimal scale, normalizing
    /// the representation.
    pub fn new(mantissa: i128, scale: u32) -> Self {
        let mut mantissa = mantissa;
        let mut scale = scale;
        if mantissa == 0 {
            scale = 0;
        } else {
            while scale > 0 && mantissa % 10 == 0 {
                mantissa /= 10;
                scale -= 1;
            }
        }
        Self { mantissa, scale }
    }

    /// Parse the exact decimal literal grammar: an optional sign, ASCII digits,
    /// and an optional `.` fractional part.
    ///
    /// Thousands separators, exponents, whitespace, and non-ASCII digits are
    /// refused. Both `+` and `-` are accepted as the optional sign; the
    /// canonical form emits `-` only for negative values.
    pub fn parse(text: &str) -> Result<Self, DecimalParseError> {
        if text.is_empty() {
            return Err(DecimalParseError::Empty);
        }
        let (negative, rest) = match text.as_bytes()[0] {
            b'+' => (false, &text[1..]),
            b'-' => (true, &text[1..]),
            _ => (false, text),
        };
        if rest.is_empty() {
            return Err(DecimalParseError::SignWithoutDigits);
        }
        if rest.bytes().filter(|byte| *byte == b'.').count() > 1 {
            return Err(DecimalParseError::MultipleDecimalPoints);
        }
        let (integer, fraction) = match rest.split_once('.') {
            Some((integer, fraction)) => (integer, Some(fraction)),
            None => (rest, None),
        };
        if integer.is_empty() {
            return Err(DecimalParseError::MissingIntegerDigits);
        }
        let fraction = match fraction {
            Some("") => return Err(DecimalParseError::MissingFractionDigits),
            Some(fraction) => fraction,
            None => "",
        };
        if !integer.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(DecimalParseError::InvalidCharacter);
        }
        // Trailing fractional zeros never change the value; strip them before
        // building the mantissa so long zero runs cannot push a representable
        // value out of range.
        let fraction = fraction.trim_end_matches('0');
        let mut digits = String::with_capacity(integer.len() + fraction.len());
        digits.push_str(integer);
        digits.push_str(fraction);
        let mantissa = digits
            .parse::<i128>()
            .map_err(|_| DecimalParseError::OutOfRange)?;
        let scale = u32::try_from(fraction.len()).map_err(|_| DecimalParseError::OutOfRange)?;
        Ok(Self::new(
            if negative { -mantissa } else { mantissa },
            scale,
        ))
    }

    /// Normalized integer mantissa of the value.
    pub fn mantissa(self) -> i128 {
        self.mantissa
    }

    /// Normalized scale: the value equals `mantissa × 10^-scale`.
    pub fn scale(self) -> u32 {
        self.scale
    }

    /// Canonical string form, used for deterministic serialization.
    pub fn to_canonical_string(self) -> String {
        let mut out = String::new();
        if self.mantissa < 0 {
            out.push('-');
        }
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;
        if scale == 0 {
            out.push_str(&digits);
        } else if digits.len() <= scale {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', scale - digits.len()));
            out.push_str(&digits);
        } else {
            let split = digits.len() - scale;
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        }
        out
    }

    /// Compare magnitudes of two nonzero values without scaling overflow. If
    /// aligning scales would exceed `u128`, the side that would overflow is
    /// necessarily larger than the other, which always fits in `u128`.
    fn cmp_magnitude(left: u128, left_scale: u32, right: u128, right_scale: u32) -> Ordering {
        match left_scale.cmp(&right_scale) {
            Ordering::Equal => left.cmp(&right),
            Ordering::Less => {
                let factor = 10u128.checked_pow(right_scale - left_scale);
                match factor.and_then(|factor| left.checked_mul(factor)) {
                    Some(scaled) => scaled.cmp(&right),
                    None => Ordering::Greater,
                }
            }
            Ordering::Greater => {
                let factor = 10u128.checked_pow(left_scale - right_scale);
                match factor.and_then(|factor| right.checked_mul(factor)) {
                    Some(scaled) => left.cmp(&scaled),
                    None => Ordering::Less,
                }
            }
        }
    }
}

/// Reason a raw literal was refused by [`ExactDecimal::parse`].
///
/// Also recorded as the `reason` of `ParsedCell::Malformed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecimalParseError {
    /// The input is empty.
    Empty,
    /// A sign with no digits, such as `-` or `+`.
    SignWithoutDigits,
    /// A `.` without integer digits, such as `.5`.
    MissingIntegerDigits,
    /// A `.` without fractional digits, such as `5.`.
    MissingFractionDigits,
    /// More than one `.`, such as `1.2.3`.
    MultipleDecimalPoints,
    /// A character outside the accepted grammar: grouping separators,
    /// exponents, non-ASCII digits, whitespace, or other symbols.
    InvalidCharacter,
    /// In the grammar but not representable as an i128 mantissa plus u32 scale.
    OutOfRange,
}

impl fmt::Display for DecimalParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            DecimalParseError::Empty => "empty decimal literal",
            DecimalParseError::SignWithoutDigits => "decimal literal has a sign but no digits",
            DecimalParseError::MissingIntegerDigits => "decimal literal has no integer digits",
            DecimalParseError::MissingFractionDigits => {
                "decimal literal has a '.' without fractional digits"
            }
            DecimalParseError::MultipleDecimalPoints => "decimal literal has more than one '.'",
            DecimalParseError::InvalidCharacter => {
                "decimal literal contains a character outside the accepted grammar"
            }
            DecimalParseError::OutOfRange => {
                "decimal literal does not fit the exact decimal representation"
            }
        };
        f.write_str(message)
    }
}

impl std::error::Error for DecimalParseError {}

impl PartialEq for ExactDecimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ExactDecimal {}

impl PartialOrd for ExactDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExactDecimal {
    fn cmp(&self, other: &Self) -> Ordering {
        let by_sign = self.mantissa.signum().cmp(&other.mantissa.signum());
        if by_sign != Ordering::Equal {
            return by_sign;
        }
        if self.mantissa == 0 {
            return Ordering::Equal;
        }
        let by_magnitude = Self::cmp_magnitude(
            self.mantissa.unsigned_abs(),
            self.scale,
            other.mantissa.unsigned_abs(),
            other.scale,
        );
        if self.mantissa < 0 {
            by_magnitude.reverse()
        } else {
            by_magnitude
        }
    }
}

impl fmt::Display for ExactDecimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_canonical_string())
    }
}

impl FromStr for ExactDecimal {
    type Err = DecimalParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for ExactDecimal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_canonical_string())
    }
}

impl<'de> Deserialize<'de> for ExactDecimal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> ExactDecimal {
        ExactDecimal::parse(text).unwrap()
    }

    #[test]
    fn accepts_exact_decimal_grammar() {
        for text in [
            "0", "10", "10000", "-3", "+7", "1.5", "-1.5", "1.0", "0.001", "-0.25",
        ] {
            assert!(
                ExactDecimal::parse(text).is_ok(),
                "expected {text:?} to parse"
            );
        }
    }

    #[test]
    fn equality_normalizes_scale_and_signed_zero() {
        assert_eq!(dec("1.10"), dec("1.1"));
        assert_eq!(dec("10"), dec("10.0"));
        assert_eq!(dec("1.00"), dec("1"));
        assert_eq!(dec("-0"), dec("0"));
        assert_eq!(dec("-0.00"), dec("0.0"));
        assert_eq!(ExactDecimal::new(0, 4), ExactDecimal::new(0, 0));
        assert_ne!(dec("1.1"), dec("1.01"));
    }

    #[test]
    fn ordering_is_exact_across_scales() {
        assert_eq!(dec("1.10").cmp(&dec("1.1")), Ordering::Equal);
        assert_eq!(dec("2").cmp(&dec("1.5")), Ordering::Greater);
        assert_eq!(dec("1.5").cmp(&dec("2")), Ordering::Less);
        assert_eq!(dec("1.05").cmp(&dec("1.1")), Ordering::Less);
        assert_eq!(dec("10.0").cmp(&dec("9.99")), Ordering::Greater);
        assert_eq!(dec("-1.5").cmp(&dec("-2")), Ordering::Greater);
    }

    #[test]
    fn negative_values_parse_and_order() {
        let d = dec("-2.50");
        assert_eq!(d.mantissa(), -25);
        assert_eq!(d.scale(), 1);
        assert_eq!(d.to_canonical_string(), "-2.5");
        assert!(dec("-3") < dec("-2.5"));
        assert!(dec("-2.5") < dec("-2"));
        assert!(dec("-2") < dec("0"));
        assert!(dec("-0.5") < dec("0.5"));
    }

    #[test]
    fn construction_normalizes_trailing_zeros_and_zero_scale() {
        let d = ExactDecimal::new(110, 2);
        assert_eq!(d.mantissa(), 11);
        assert_eq!(d.scale(), 1);
        let d = ExactDecimal::new(10, 1);
        assert_eq!(d.mantissa(), 1);
        assert_eq!(d.scale(), 0);
        assert_eq!(ExactDecimal::new(-250, 2), dec("-2.5"));
        let z = ExactDecimal::new(0, 5);
        assert_eq!(z.mantissa(), 0);
        assert_eq!(z.scale(), 0);
        assert_eq!(ExactDecimal::new(1, 0).scale(), 0);
    }

    #[test]
    fn canonical_string_is_deterministic() {
        assert_eq!(dec("1.10").to_canonical_string(), "1.1");
        assert_eq!(dec("10.0").to_canonical_string(), "10");
        assert_eq!(dec("-0").to_canonical_string(), "0");
        assert_eq!(dec("0.001").to_canonical_string(), "0.001");
        assert_eq!(dec("-2.5").to_canonical_string(), "-2.5");
        assert_eq!(dec("+7").to_canonical_string(), "7");
        assert_eq!(ExactDecimal::new(1, 5).to_canonical_string(), "0.00001");
        assert_eq!(dec("10.05").to_canonical_string(), "10.05");
    }

    #[test]
    fn serde_round_trip() {
        for text in ["1.1", "-2.5", "10", "0.001", "0"] {
            let d = dec(text);
            let json = serde_json::to_string(&d).unwrap();
            let back: ExactDecimal = serde_json::from_str(&json).unwrap();
            assert_eq!(d, back);
        }
    }

    #[test]
    fn serde_serializes_canonical_string() {
        assert_eq!(
            serde_json::to_value(dec("1.10")).unwrap(),
            serde_json::json!("1.1")
        );
        assert_eq!(
            serde_json::to_value(dec("10.0")).unwrap(),
            serde_json::json!("10")
        );
        assert_eq!(
            serde_json::to_value(dec("-0.00")).unwrap(),
            serde_json::json!("0")
        );
        let back: ExactDecimal = serde_json::from_str("\"-2.50\"").unwrap();
        assert_eq!(
            serde_json::to_value(back).unwrap(),
            serde_json::json!("-2.5")
        );
        assert!(serde_json::from_str::<ExactDecimal>("\"1e5\"").is_err());
    }

    #[test]
    fn rejects_invalid_literals() {
        assert_eq!(ExactDecimal::parse(""), Err(DecimalParseError::Empty));
        assert_eq!(
            ExactDecimal::parse("-"),
            Err(DecimalParseError::SignWithoutDigits)
        );
        assert_eq!(
            ExactDecimal::parse("+"),
            Err(DecimalParseError::SignWithoutDigits)
        );
        assert_eq!(
            ExactDecimal::parse("1.2.3"),
            Err(DecimalParseError::MultipleDecimalPoints)
        );
        assert_eq!(
            ExactDecimal::parse("10,000"),
            Err(DecimalParseError::InvalidCharacter)
        );
        assert_eq!(
            ExactDecimal::parse("1e5"),
            Err(DecimalParseError::InvalidCharacter)
        );
        assert_eq!(
            ExactDecimal::parse("１２３"),
            Err(DecimalParseError::InvalidCharacter)
        );
        assert_eq!(
            ExactDecimal::parse(".5"),
            Err(DecimalParseError::MissingIntegerDigits)
        );
        assert_eq!(
            ExactDecimal::parse("5."),
            Err(DecimalParseError::MissingFractionDigits)
        );
        assert_eq!(
            ExactDecimal::parse(" 1"),
            Err(DecimalParseError::InvalidCharacter)
        );
        assert_eq!(
            ExactDecimal::parse("1 "),
            Err(DecimalParseError::InvalidCharacter)
        );
    }

    #[test]
    fn rejects_unrepresentable_literals() {
        assert_eq!(
            ExactDecimal::parse("9999999999999999999999999999999999999999"),
            Err(DecimalParseError::OutOfRange)
        );
    }

    #[test]
    fn comparison_handles_extreme_magnitudes() {
        let max = ExactDecimal::new(i128::MAX, 0);
        let tiny = ExactDecimal::new(1, 38);
        assert!(max > tiny);
        assert!(tiny < ExactDecimal::new(1, 5));
        assert!(ExactDecimal::new(1, 40) < ExactDecimal::new(1, 38));
        assert!(ExactDecimal::new(1, 0) > ExactDecimal::new(1, 40));
        assert!(ExactDecimal::new(-1, 0) < ExactDecimal::new(-1, 40));
    }
}
