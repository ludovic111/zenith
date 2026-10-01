//! Hand-written wire primitives used by the generated types.
//!
//! Each one reproduces how `Schema.toCodecJson` (effect 4.0.0-rc.115) encodes a value, see
//! docs/zenith-code-rust-plan.md §1.5:
//!
//! | Effect schema | Rust |
//! |---|---|
//! | `Schema.Number` (unrefined), `Schema.Finite` | [`JsNumber`]: `NaN`/`Infinity`/`-Infinity` as strings |
//! | `Schema.DateTimeUtc` | [`DateTimeUtc`]: ISO string with milliseconds and `Z` |
//! | `Schema.Option(X)` | [`EOption`]: `{"_tag":"Some","value":…}` / `{"_tag":"None"}` |
//! | `Schema.Uint8Array` | [`Base64Bytes`]: standard base64 |
//! | `Schema.Never` | [`Never`]: uninhabited |
//! | `ForwardCompatibleArray(X)` | [`LenientVec`]: elements that do not decode are dropped |
//! | `ForwardCompatibleOptional`, `ForwardCompatibleNullable`, `OmittedWhenNull` | [`lenient_option`], [`lenient_double_option`] |
//! | `Schema.optionalKey(Schema.NullOr(X))` | `Option<Option<T>>` with [`double_option`] |

use std::fmt;

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// -------------------------------------------------------------------------------------------
// Numbers
// -------------------------------------------------------------------------------------------

/// A JavaScript number as `Schema.Number` encodes it: finite values are JSON numbers (integral
/// values without a fractional part, like `JSON.stringify`), and `NaN`, `Infinity` and
/// `-Infinity` are the strings `"NaN"`, `"Infinity"` and `"-Infinity"`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct JsNumber(pub f64);

impl JsNumber {
    /// The underlying value.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl From<f64> for JsNumber {
    fn from(value: f64) -> Self {
        Self(value)
    }
}

impl From<i64> for JsNumber {
    fn from(value: i64) -> Self {
        // Precision loss above 2^53 matches JavaScript.
        #[allow(clippy::cast_precision_loss)]
        Self(value as f64)
    }
}

impl From<JsNumber> for f64 {
    fn from(value: JsNumber) -> Self {
        value.0
    }
}

impl fmt::Display for JsNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// 2^63, as an `f64`.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

impl Serialize for JsNumber {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let v = self.0;
        if v.is_nan() {
            serializer.serialize_str("NaN")
        } else if v.is_infinite() {
            serializer.serialize_str(if v > 0.0 { "Infinity" } else { "-Infinity" })
        } else if v.fract() == 0.0 && v.abs() < TWO_POW_63 {
            // Exact: `v` is integral and in range.
            #[allow(clippy::cast_possible_truncation)]
            serializer.serialize_i64(v as i64)
        } else if v.fract() == 0.0 && v > 0.0 && v < 2.0 * TWO_POW_63 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            serializer.serialize_u64(v as u64)
        } else {
            serializer.serialize_f64(v)
        }
    }
}

impl<'de> Deserialize<'de> for JsNumber {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = JsNumber;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"a number or one of "NaN", "Infinity", "-Infinity""#)
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<JsNumber, E> {
                Ok(JsNumber(v))
            }
            #[allow(clippy::cast_precision_loss)]
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<JsNumber, E> {
                Ok(JsNumber(v as f64))
            }
            #[allow(clippy::cast_precision_loss)]
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<JsNumber, E> {
                Ok(JsNumber(v as f64))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<JsNumber, E> {
                match v {
                    "NaN" => Ok(JsNumber(f64::NAN)),
                    "Infinity" => Ok(JsNumber(f64::INFINITY)),
                    "-Infinity" => Ok(JsNumber(f64::NEG_INFINITY)),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(v), &self)),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

// -------------------------------------------------------------------------------------------
// Dates
// -------------------------------------------------------------------------------------------

/// `Schema.DateTimeUtc`: a JavaScript `Date` in milliseconds since the Unix epoch, encoded like
/// `Date.prototype.toISOString` (`2026-10-01T12:00:00.000Z`: always milliseconds and `Z`; years
/// outside 0..=9999 as `±YYYYYY`). The whole `Date` range (±8.64e15 ms) is supported.
///
/// Decoding accepts `YYYY-MM-DD`, and `YYYY-MM-DDTHH:MM[:SS[.fff…]]` followed by `Z`, `±HH:MM`
/// or nothing (read as UTC; JavaScript would use the local zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DateTimeUtc(i64);

const MS_PER_DAY: i64 = 86_400_000;
/// `Date` limits: ±100,000,000 days around the epoch.
const MAX_DATE_MS: i64 = 8_640_000_000_000_000;

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl DateTimeUtc {
    /// The current time, truncated to milliseconds (the wire precision).
    #[must_use]
    pub fn now() -> Self {
        Self(jiff::Timestamp::now().as_millisecond())
    }

    /// From milliseconds since the Unix epoch (`new Date(ms)`).
    ///
    /// # Errors
    /// When `ms` is outside the `Date` range (±8.64e15).
    pub fn from_millis(ms: i64) -> Result<Self, String> {
        if ms.abs() <= MAX_DATE_MS {
            Ok(Self(ms))
        } else {
            Err(format!("{ms} ms is outside the Date range"))
        }
    }

    /// Milliseconds since the Unix epoch (`Date.getTime()`).
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// As a jiff timestamp (`None` outside jiff's range, years -9999..=9999).
    #[must_use]
    pub fn to_timestamp(self) -> Option<jiff::Timestamp> {
        jiff::Timestamp::from_millisecond(self.0).ok()
    }

    /// The wire form, like `Date.prototype.toISOString`.
    #[must_use]
    pub fn to_iso_string(self) -> String {
        let days = self.0.div_euclid(MS_PER_DAY);
        let in_day = self.0.rem_euclid(MS_PER_DAY);
        let (y, m, d) = civil_from_days(days);
        let year = if (0..=9999).contains(&y) {
            format!("{y:04}")
        } else if y < 0 {
            format!("-{:06}", -y)
        } else {
            format!("+{y:06}")
        };
        format!(
            "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            in_day / 3_600_000,
            in_day / 60_000 % 60,
            in_day / 1000 % 60,
            in_day % 1000
        )
    }

    /// Parses an ISO 8601 date or date-time (see the type's documentation).
    ///
    /// # Errors
    /// When the string is not such a date-time or is outside the `Date` range.
    pub fn parse(s: &str) -> Result<Self, String> {
        parse_iso(s.trim())
            .ok_or_else(|| format!("invalid DateTimeUtc: {s:?}"))
            .and_then(Self::from_millis)
    }
}

fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    let num = |i: &mut usize, len: usize| -> Option<i64> {
        let part = s.get(*i..*i + len)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        *i += len;
        part.parse().ok()
    };
    let expect = |i: &mut usize, c: u8| -> Option<()> { (b.get(*i) == Some(&c)).then(|| *i += 1) };
    let year = match b.first()? {
        b'+' | b'-' => {
            let negative = b[0] == b'-';
            i = 1;
            let y = num(&mut i, 6)?;
            if negative {
                -y
            } else {
                y
            }
        }
        _ => num(&mut i, 4)?,
    };
    expect(&mut i, b'-')?;
    let month = num(&mut i, 2)?;
    expect(&mut i, b'-')?;
    let day = num(&mut i, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut ms = days_from_civil(year, month, day) * MS_PER_DAY;
    if i == b.len() {
        return Some(ms);
    }
    if b[i] != b'T' && b[i] != b't' && b[i] != b' ' {
        return None;
    }
    i += 1;
    let hour = num(&mut i, 2)?;
    expect(&mut i, b':')?;
    let minute = num(&mut i, 2)?;
    let mut second = 0;
    let mut millis = 0;
    if b.get(i) == Some(&b':') {
        i += 1;
        second = num(&mut i, 2)?;
        if b.get(i) == Some(&b'.') || b.get(i) == Some(&b',') {
            i += 1;
            let start = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            let frac = s.get(start..i)?;
            if frac.is_empty() {
                return None;
            }
            let padded = format!("{frac:0<3}");
            millis = padded[..3].parse::<i64>().ok()?;
        }
    }
    if hour > 24 || minute > 59 || second > 59 {
        return None;
    }
    ms += ((hour * 60 + minute) * 60 + second) * 1000 + millis;
    match b.get(i) {
        None => Some(ms),
        Some(b'Z' | b'z') if i + 1 == b.len() => Some(ms),
        Some(&sign @ (b'+' | b'-')) => {
            i += 1;
            let oh = num(&mut i, 2)?;
            if b.get(i) == Some(&b':') {
                i += 1;
            }
            let om = num(&mut i, 2)?;
            if i != b.len() {
                return None;
            }
            let offset = (oh * 60 + om) * 60_000;
            Some(if sign == b'+' { ms - offset } else { ms + offset })
        }
        _ => None,
    }
}

impl TryFrom<jiff::Timestamp> for DateTimeUtc {
    type Error = String;
    fn try_from(value: jiff::Timestamp) -> Result<Self, String> {
        Self::from_millis(value.as_millisecond())
    }
}

impl fmt::Display for DateTimeUtc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_iso_string())
    }
}

impl Serialize for DateTimeUtc {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_iso_string())
    }
}

impl<'de> Deserialize<'de> for DateTimeUtc {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        Self::parse(&s).map_err(D::Error::custom)
    }
}

// -------------------------------------------------------------------------------------------
// Effect Option
// -------------------------------------------------------------------------------------------

/// `Schema.Option(X)`: `{"_tag":"Some","value":…}` or `{"_tag":"None"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct EOption<T>(pub Option<T>);

impl<T> EOption<T> {
    /// `Some(value)`.
    pub const fn some(value: T) -> Self {
        Self(Some(value))
    }
    /// `None`.
    pub const fn none() -> Self {
        Self(None)
    }
}

impl<T> From<Option<T>> for EOption<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

impl<T> From<EOption<T>> for Option<T> {
    fn from(value: EOption<T>) -> Self {
        value.0
    }
}

impl<T> std::ops::Deref for EOption<T> {
    type Target = Option<T>;
    fn deref(&self) -> &Option<T> {
        &self.0
    }
}

impl<T: Serialize> Serialize for EOption<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        if let Some(value) = &self.0 {
            let mut s = serializer.serialize_struct("Some", 2)?;
            s.serialize_field("_tag", "Some")?;
            s.serialize_field("value", value)?;
            s.end()
        } else {
            let mut s = serializer.serialize_struct("None", 1)?;
            s.serialize_field("_tag", "None")?;
            s.end()
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for EOption<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "_tag")]
        enum Wire<T> {
            Some { value: T },
            None,
        }
        Ok(match Wire::<T>::deserialize(deserializer)? {
            Wire::Some { value } => Self(Some(value)),
            Wire::None => Self(None),
        })
    }
}

// -------------------------------------------------------------------------------------------
// Bytes, Never
// -------------------------------------------------------------------------------------------

/// `Schema.Uint8Array`: standard (padded) base64.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Base64Bytes(pub Vec<u8>);

impl Serialize for Base64Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use base64::Engine as _;
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Base64Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use base64::Engine as _;
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s.as_bytes()))
            .map(Self)
            .map_err(D::Error::custom)
    }
}

/// `Schema.Never`: no value has this type (the error of an RPC that cannot fail).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Never {}

impl Serialize for Never {
    fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        match *self {}
    }
}

impl<'de> Deserialize<'de> for Never {
    fn deserialize<D: Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(D::Error::custom("Schema.Never has no values"))
    }
}

// -------------------------------------------------------------------------------------------
// Forward-compatible helpers (contracts baseSchemas.ts)
// -------------------------------------------------------------------------------------------

/// `ForwardCompatibleArray(X)`: encodes as a plain array; decoding drops the elements this
/// build cannot decode instead of failing (a newer server may send members we do not know).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct LenientVec<T>(pub Vec<T>);

impl<T> std::ops::Deref for LenientVec<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Vec<T> {
        &self.0
    }
}

impl<T> std::ops::DerefMut for LenientVec<T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.0
    }
}

impl<T> From<Vec<T>> for LenientVec<T> {
    fn from(value: Vec<T>) -> Self {
        Self(value)
    }
}

impl<T> From<LenientVec<T>> for Vec<T> {
    fn from(value: LenientVec<T>) -> Self {
        value.0
    }
}

impl<T> IntoIterator for LenientVec<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a LenientVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T> FromIterator<T> for LenientVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<T: Serialize> Serialize for LenientVec<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for LenientVec<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
        Ok(Self(values.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect()))
    }
}

/// `deserialize_with` for forward-compatible optional/nullable fields: a value this build
/// cannot decode reads as `None` (absent, or `null` for `ForwardCompatibleNullable`).
///
/// # Errors
/// Only when the input is not JSON.
pub fn lenient_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<Option<T>>(v).ok().flatten())
}

/// `deserialize_with` for `ForwardCompatibleOptional(Schema.NullOr(X))`: absent → `None`
/// (via `#[serde(default)]`), `null` → `Some(None)`, a value this build cannot decode → `None`.
///
/// # Errors
/// Only when the input is not JSON.
pub fn lenient_double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    if v.is_null() {
        return Ok(Some(None));
    }
    Ok(serde_json::from_value::<T>(v).ok().map(Some))
}

/// Used by the generated `deserialize_with` of `withDecodingDefault` fields: `null` reads as a
/// missing key, which decodes to the default.
///
/// # Errors
/// When the value is neither `null` nor a `T`.
pub fn null_as_default<'de, D, T>(deserializer: D, default: fn() -> T) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_else(default))
}

/// `deserialize_with` for required `Schema.NullOr(X)` keys: like `Option<T>`'s own
/// `Deserialize`, except that serde reports a missing key instead of reading it as `None`.
///
/// # Errors
/// When the value is neither `null` nor a `T`.
pub fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// `deserialize_with` for optional `Schema.Unknown` fields: a present `null` is a value
/// (`Some(Value::Null)`); only an absent key is `None` (via `#[serde(default)]`).
///
/// # Errors
/// Only when the input is not JSON.
pub fn value_present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// `with` module for `Schema.optionalKey(Schema.NullOr(X))` (and `Schema.optional` of a
/// nullable): absent → `None`, `null` → `Some(None)`, value → `Some(Some(v))`. Use with
/// `#[serde(default, skip_serializing_if = "Option::is_none")]`.
pub mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// Writes `null` for `Some(None)`.
    ///
    /// # Errors
    /// When the inner value fails to serialize.
    #[allow(clippy::ref_option)]
    pub fn serialize<S, T>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match value {
            Some(inner) => inner.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    /// Reads a present key: `null` → `Some(None)`.
    ///
    /// # Errors
    /// When the value does not decode as `T`.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}

// -------------------------------------------------------------------------------------------
// Support for the generated code
// -------------------------------------------------------------------------------------------

/// `serde_json::from_value`, used by the generated union decoders.
///
/// # Errors
/// When the value does not decode as `T`.
pub fn from_value<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(value)
}

/// The error of a tagged union whose discriminator is missing or unknown.
#[must_use]
pub fn unknown_tag(union: &str, key: &str, found: Option<&str>) -> serde_json::Error {
    match found {
        Some(tag) => serde_json::Error::custom(format!("{union}: unknown {key} {tag:?}")),
        None => serde_json::Error::custom(format!("{union}: missing string discriminator {key:?}")),
    }
}

/// The error of an untagged union that no member accepted.
#[must_use]
pub fn no_member(union: &str, value: &serde_json::Value) -> String {
    let mut shown = value.to_string();
    if shown.len() > 200 {
        let mut cut = 200;
        while !shown.is_char_boundary(cut) {
            cut -= 1;
        }
        shown.truncate(cut);
        shown.push('…');
    }
    format!("{union}: no member matches {shown}")
}

/// The decoding default of a `withDecodingDefault` field, from its JSON encoding.
///
/// # Panics
/// When the generated JSON does not decode as `T` (a generator bug; `registry::check_defaults`
/// exercises every default in tests).
#[must_use]
pub fn json_default<T: DeserializeOwned>(json: &str) -> T {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("invalid decoding default {json}: {e}"))
}

#[doc(hidden)]
pub fn expect_literal<'de, D: Deserializer<'de>>(deserializer: D, expected: &serde_json::Value) -> Result<(), D::Error> {
    let v = serde_json::Value::deserialize(deserializer)?;
    let matches = match (&v, expected) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => v == *expected,
    };
    if matches {
        Ok(())
    } else {
        Err(D::Error::custom(format_args!("expected literal {expected}, found {v}")))
    }
}

/// A branded string (`ThreadId`, `ProjectId`, …): the plain string on the wire.
macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps a string.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            /// The plain string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
pub(crate) use string_newtype;

/// A branded integer (`RpcClientId`): the plain number on the wire.
macro_rules! int_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                Self(value)
            }
        }
        impl From<$name> for i64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}
pub(crate) use int_newtype;

/// A single-literal unit type: serializes to the literal, deserializes only from it.
macro_rules! lit_type {
    ($name:ident, $ty:ty, $value:expr, $ser:ident, $json:expr) => {
        #[doc = concat!("The literal `", stringify!($value), "`.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name;

        impl $name {
            /// The literal.
            pub const VALUE: $ty = $value;
        }
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.$ser($value)
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                $crate::prim::expect_literal(deserializer, &$json).map(|()| $name)
            }
        }
    };
}
pub(crate) use lit_type;

macro_rules! lit_str {
    ($name:ident, $value:literal) => {
        $crate::prim::lit_type!($name, &'static str, $value, serialize_str, serde_json::Value::String($value.to_owned()));
    };
}
pub(crate) use lit_str;

macro_rules! lit_int {
    ($name:ident, $value:literal) => {
        $crate::prim::lit_type!($name, i64, $value, serialize_i64, serde_json::Value::from($value as i64));
    };
}
pub(crate) use lit_int;

#[allow(unused_macros)]
macro_rules! lit_f64 {
    ($name:ident, $value:literal) => {
        $crate::prim::lit_type!($name, f64, $value, serialize_f64, serde_json::Value::from($value as f64));
    };
}
#[allow(unused_imports)]
pub(crate) use lit_f64;

macro_rules! lit_bool {
    ($name:ident, $value:literal) => {
        $crate::prim::lit_type!($name, bool, $value, serialize_bool, serde_json::Value::Bool($value));
    };
}
pub(crate) use lit_bool;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn js_number_encoding() {
        assert_eq!(serde_json::to_value(JsNumber(f64::NAN)).unwrap(), json!("NaN"));
        assert_eq!(serde_json::to_value(JsNumber(f64::INFINITY)).unwrap(), json!("Infinity"));
        assert_eq!(serde_json::to_value(JsNumber(f64::NEG_INFINITY)).unwrap(), json!("-Infinity"));
        assert_eq!(serde_json::to_value(JsNumber(3.0)).unwrap(), json!(3));
        assert_eq!(serde_json::to_value(JsNumber(-0.0)).unwrap(), json!(0));
        assert_eq!(serde_json::to_value(JsNumber(1.5)).unwrap(), json!(1.5));
        let n: JsNumber = serde_json::from_value(json!("NaN")).unwrap();
        assert!(n.0.is_nan());
        let n: JsNumber = serde_json::from_value(json!(-2)).unwrap();
        assert!((n.0 + 2.0).abs() < f64::EPSILON);
        assert!(serde_json::from_value::<JsNumber>(json!("12")).is_err());
    }

    #[test]
    fn date_time_encoding() {
        let d: DateTimeUtc = serde_json::from_value(json!("2026-10-01T12:00:00Z")).unwrap();
        assert_eq!(serde_json::to_value(d).unwrap(), json!("2026-10-01T12:00:00.000Z"));
        let d: DateTimeUtc = serde_json::from_value(json!("2026-10-01T14:00:00.123456+02:00")).unwrap();
        assert_eq!(d.to_iso_string(), "2026-10-01T12:00:00.123Z");
        let iso = |ms| DateTimeUtc::from_millis(ms).unwrap().to_iso_string();
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(iso(8_640_000_000_000_000), "+275760-09-13T00:00:00.000Z");
        assert_eq!(iso(-8_640_000_000_000_000), "-271821-04-20T00:00:00.000Z");
        assert_eq!(iso(-62_198_755_200_000), "-000001-01-01T00:00:00.000Z");
        for text in ["+275760-09-13T00:00:00.000Z", "-271821-04-20T00:00:00.000Z", "2024-02-29T23:59:59.999Z"] {
            assert_eq!(DateTimeUtc::parse(text).unwrap().to_iso_string(), text);
        }
        assert_eq!(DateTimeUtc::parse("2026-10-01").unwrap().to_iso_string(), "2026-10-01T00:00:00.000Z");
        assert!(DateTimeUtc::parse("+275760-09-13T00:00:00.001Z").is_err());
        assert!(DateTimeUtc::parse("yesterday").is_err());
    }

    #[test]
    fn option_encoding() {
        assert_eq!(serde_json::to_value(EOption::some(1)).unwrap(), json!({"_tag": "Some", "value": 1}));
        assert_eq!(serde_json::to_value(EOption::<i32>::none()).unwrap(), json!({"_tag": "None"}));
        let o: EOption<i32> = serde_json::from_value(json!({"value": 2, "_tag": "Some"})).unwrap();
        assert_eq!(o, EOption::some(2));
    }

    #[test]
    fn lenient_vec_drops_unknown_members() {
        #[derive(Deserialize, Debug, PartialEq)]
        enum K {
            #[serde(rename = "a")]
            A,
        }
        let v: LenientVec<K> = serde_json::from_value(json!(["a", "b", "a"])).unwrap();
        assert_eq!(v.0, vec![K::A, K::A]);
    }

    #[test]
    fn base64_bytes() {
        let b = Base64Bytes(vec![1, 2, 3, 250]);
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v, json!("AQID+g=="));
        assert_eq!(serde_json::from_value::<Base64Bytes>(v).unwrap(), b);
    }
}
