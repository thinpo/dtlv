use thiserror::Error;

pub type EntityId = u64;
pub type AttrId = u32;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("invalid type tag: {0:#x}")]
    InvalidTag(u8),
    #[error("invalid utf-8 string")]
    InvalidUtf8,
    #[error("invalid escaped byte sequence")]
    InvalidEscape,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    I64(i64),
    F64(f64),
    Instant(i64),
    Ref(EntityId),
    Keyword(String),
    Str(String),
    Bytes(Vec<u8>),
    Uuid([u8; 16]),
    Tuple(Vec<Value>),
    Doc(String),
}

impl Value {
    pub fn tag(&self) -> u8 {
        match self {
            Value::Bool(_) => 0x01,
            Value::I64(_) => 0x02,
            Value::F64(_) => 0x03,
            Value::Instant(_) => 0x04,
            Value::Ref(_) => 0x05,
            Value::Keyword(_) => 0x06,
            Value::Str(_) => 0x07,
            Value::Bytes(_) => 0x08,
            Value::Uuid(_) => 0x09,
            Value::Tuple(_) => 0x0a,
            Value::Doc(_) => 0x0b,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Keyword(s) | Value::Doc(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(v) | Value::Instant(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::F64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Option<EntityId> {
        match self {
            Value::Ref(e) => Some(*e),
            _ => None,
        }
    }
}

#[inline]
pub fn encode_i64_sortable(v: i64) -> [u8; 8] {
    ((v as u64) ^ (1u64 << 63)).to_be_bytes()
}

#[inline]
pub fn decode_i64_sortable(bytes: [u8; 8]) -> i64 {
    (u64::from_be_bytes(bytes) ^ (1u64 << 63)) as i64
}

#[inline]
pub fn encode_f64_sortable(v: f64) -> [u8; 8] {
    let bits = v.to_bits();
    let encoded = if (bits >> 63) != 0 {
        !bits
    } else {
        bits ^ (1u64 << 63)
    };
    encoded.to_be_bytes()
}

#[inline]
pub fn decode_f64_sortable(bytes: [u8; 8]) -> f64 {
    let encoded = u64::from_be_bytes(bytes);
    let bits = if (encoded >> 63) == 0 {
        !encoded
    } else {
        encoded ^ (1u64 << 63)
    };
    f64::from_bits(bits)
}

/// Encode a Value into an order-preserving byte buffer.
pub fn encode_value(val: &Value, out: &mut Vec<u8>) {
    out.push(val.tag());
    match val {
        Value::Bool(b) => out.push(if *b { 1 } else { 0 }),
        Value::I64(v) | Value::Instant(v) => out.extend_from_slice(&encode_i64_sortable(*v)),
        Value::F64(v) => out.extend_from_slice(&encode_f64_sortable(*v)),
        Value::Ref(e) => out.extend_from_slice(&e.to_be_bytes()),
        Value::Keyword(s) | Value::Str(s) | Value::Doc(s) => out.extend_from_slice(s.as_bytes()),
        Value::Bytes(b) => out.extend_from_slice(b),
        Value::Uuid(u) => out.extend_from_slice(u),
        Value::Tuple(elems) => {
            let mut tmp = Vec::with_capacity(32);
            for el in elems {
                tmp.clear();
                encode_value(el, &mut tmp);
                encode_escaped_bytes(&tmp, out);
            }
        }
    }
}

/// Decode a Value from a slice encoded by `encode_value`.
pub fn decode_value(bytes: &[u8]) -> Result<Value, CodecError> {
    if bytes.is_empty() {
        return Err(CodecError::UnexpectedEof);
    }
    let tag = bytes[0];
    let rest = &bytes[1..];
    match tag {
        0x01 => {
            if rest.is_empty() {
                return Err(CodecError::UnexpectedEof);
            }
            Ok(Value::Bool(rest[0] != 0))
        }
        0x02 | 0x04 => {
            let arr: [u8; 8] = rest
                .try_into()
                .map_err(|_| CodecError::UnexpectedEof)?;
            let v = decode_i64_sortable(arr);
            Ok(if tag == 0x02 {
                Value::I64(v)
            } else {
                Value::Instant(v)
            })
        }
        0x03 => {
            let arr: [u8; 8] = rest
                .try_into()
                .map_err(|_| CodecError::UnexpectedEof)?;
            Ok(Value::F64(decode_f64_sortable(arr)))
        }
        0x05 => {
            let arr: [u8; 8] = rest
                .try_into()
                .map_err(|_| CodecError::UnexpectedEof)?;
            Ok(Value::Ref(u64::from_be_bytes(arr)))
        }
        0x06 | 0x07 | 0x0b => {
            let s = std::str::from_utf8(rest).map_err(|_| CodecError::InvalidUtf8)?;
            Ok(match tag {
                0x06 => Value::Keyword(s.to_owned()),
                0x07 => Value::Str(s.to_owned()),
                _ => Value::Doc(s.to_owned()),
            })
        }
        0x08 => Ok(Value::Bytes(rest.to_vec())),
        0x09 => {
            let arr: [u8; 16] = rest
                .try_into()
                .map_err(|_| CodecError::UnexpectedEof)?;
            Ok(Value::Uuid(arr))
        }
        0x0a => {
            let mut elems = Vec::new();
            let mut cur = rest;
            while !cur.is_empty() {
                let (raw_el, next) = decode_escaped_bytes(cur)?;
                elems.push(decode_value(&raw_el)?);
                cur = next;
            }
            Ok(Value::Tuple(elems))
        }
        other => Err(CodecError::InvalidTag(other)),
    }
}

/// Append byte-stuffed slice (0x00 -> 0x00 0x01, terminated with 0x00 0x00)
/// so variable-length keys preserve lexicographical order before trailing fixed fields.
pub fn encode_escaped_bytes(raw: &[u8], out: &mut Vec<u8>) {
    for &b in raw {
        if b == 0x00 {
            out.push(0x00);
            out.push(0x01);
        } else {
            out.push(b);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

pub fn decode_escaped_bytes(input: &[u8]) -> Result<(Vec<u8>, &[u8]), CodecError> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x00 {
            if i + 1 >= input.len() {
                return Err(CodecError::UnexpectedEof);
            }
            match input[i + 1] {
                0x00 => return Ok((out, &input[i + 2..])),
                0x01 => {
                    out.push(0x00);
                    i += 2;
                }
                _ => return Err(CodecError::InvalidEscape),
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    Err(CodecError::UnexpectedEof)
}

/// EAV key: [e: 8B][a: 4B][encoded_value...]
pub fn encode_eav_key(e: EntityId, a: AttrId, val: &Value, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&e.to_be_bytes());
    out.extend_from_slice(&a.to_be_bytes());
    encode_value_indexed(val, out);
}

/// AEV key: [a: 4B][e: 8B][encoded_value...]
pub fn encode_aev_key(a: AttrId, e: EntityId, val: &Value, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&a.to_be_bytes());
    out.extend_from_slice(&e.to_be_bytes());
    encode_value_indexed(val, out);
}

pub fn encode_ea_prefix(e: EntityId, a: AttrId, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&e.to_be_bytes());
    out.extend_from_slice(&a.to_be_bytes());
}

/// AVE prefix: [a: 4B][escaped(encoded_value)]
/// Encoding for index keys. `-0.0` and `0.0` are equal values, so both are keyed as `0.0`; otherwise a lookup
/// for one misses the other.
pub fn encode_value_indexed(val: &Value, out: &mut Vec<u8>) {
    match val {
        Value::F64(x) if *x == 0.0 => encode_value(&Value::F64(0.0), out),
        Value::Tuple(elems) => {
            out.push(val.tag());
            let mut tmp = Vec::with_capacity(32);
            for el in elems {
                tmp.clear();
                encode_value_indexed(el, &mut tmp);
                encode_escaped_bytes(&tmp, out);
            }
        }
        other => encode_value(other, out),
    }
}

pub fn encode_av_prefix(a: AttrId, val: &Value, out: &mut Vec<u8>) {
    thread_local! {
        static SCRATCH: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(64));
    }
    out.clear();
    out.extend_from_slice(&a.to_be_bytes());
    SCRATCH.with(|cell| {
        let mut tmp = cell.borrow_mut();
        tmp.clear();
        encode_value_indexed(val, &mut tmp);
        encode_escaped_bytes(&tmp, out);
    });
}

/// AVE full key: [a: 4B][escaped(encoded_value)][e: 8B]
pub fn encode_ave_key(a: AttrId, val: &Value, e: EntityId, out: &mut Vec<u8>) {
    encode_av_prefix(a, val, out);
    out.extend_from_slice(&e.to_be_bytes());
}

pub fn decode_ave_entity(ave_key: &[u8]) -> Result<EntityId, CodecError> {
    if ave_key.len() < 12 {
        return Err(CodecError::UnexpectedEof);
    }
    let tail: [u8; 8] = ave_key[ave_key.len() - 8..]
        .try_into()
        .map_err(|_| CodecError::UnexpectedEof)?;
    Ok(u64::from_be_bytes(tail))
}

/// VAE key: [target_e: 8B][a: 4B][src_e: 8B]
pub fn encode_vae_key(target: EntityId, a: AttrId, src: EntityId, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&target.to_be_bytes());
    out.extend_from_slice(&a.to_be_bytes());
    out.extend_from_slice(&src.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_ordering() {
        let vals = vec![
            Value::I64(i64::MIN),
            Value::I64(-100),
            Value::I64(0),
            Value::I64(42),
            Value::I64(i64::MAX),
        ];
        let encoded: Vec<Vec<u8>> = vals
            .iter()
            .map(|v| {
                let mut buf = Vec::new();
                encode_value(v, &mut buf);
                buf
            })
            .collect();
        for i in 0..encoded.len() - 1 {
            assert!(encoded[i] < encoded[i + 1], "i64 sort order broken");
        }
        for (v, b) in vals.iter().zip(encoded.iter()) {
            assert_eq!(decode_value(b).unwrap(), *v);
        }

        let floats = vec![-1e10_f64, -1.5, -0.001, 0.0, 0.001, 1.5, 1e10];
        let f_enc: Vec<Vec<u8>> = floats
            .iter()
            .map(|&f| {
                let mut buf = Vec::new();
                encode_value(&Value::F64(f), &mut buf);
                buf
            })
            .collect();
        for i in 0..f_enc.len() - 1 {
            assert!(f_enc[i] < f_enc[i + 1], "f64 sort order broken");
        }
        for (&f, b) in floats.iter().zip(f_enc.iter()) {
            assert_eq!(decode_value(b).unwrap(), Value::F64(f));
        }
    }

    #[test]
    fn ave_prefix_disjointness() {
        let mut k_aa = Vec::new();
        let mut k_aapl = Vec::new();
        encode_ave_key(1, &Value::Str("AA".into()), 10, &mut k_aa);
        encode_ave_key(1, &Value::Str("AAPL".into()), 1, &mut k_aapl);

        let mut pref_aa = Vec::new();
        encode_av_prefix(1, &Value::Str("AA".into()), &mut pref_aa);
        assert!(k_aa.starts_with(&pref_aa));
        assert!(!k_aapl.starts_with(&pref_aa));
        assert_eq!(decode_ave_entity(&k_aa).unwrap(), 10);
    }
}
