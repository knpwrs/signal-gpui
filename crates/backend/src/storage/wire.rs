//! Just enough of the protobuf wire format to edit one field of a record and
//! copy every other byte through. prost drops fields it doesn't know, and
//! Signal clients must carry unknown Storage Service fields through intact.

/// One top-level field of an encoded message.
pub(crate) struct Field<'a> {
    pub number: u32,
    /// The whole field: key, length prefix and value.
    pub raw: &'a [u8],
    /// The value of a length-delimited field.
    pub payload: Option<&'a [u8]>,
    /// The value of a varint field.
    pub varint: Option<u64>,
}

/// Splits an encoded message into its top-level fields. `None` if malformed
/// (or if it uses the long-deprecated group encoding).
pub(crate) fn fields(mut buf: &[u8]) -> Option<Vec<Field<'_>>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let start = buf;
        let key = varint(&mut buf)?;
        let number = u32::try_from(key >> 3).ok().filter(|n| *n > 0)?;
        let mut value = None;
        let payload = match key & 7 {
            0 => {
                value = Some(varint(&mut buf)?);
                None
            }
            1 => {
                buf = buf.get(8..)?;
                None
            }
            2 => {
                let len = usize::try_from(varint(&mut buf)?).ok()?;
                let payload = buf.get(..len)?;
                buf = &buf[len..];
                Some(payload)
            }
            5 => {
                buf = buf.get(4..)?;
                None
            }
            _ => return None,
        };
        out.push(Field { number, raw: &start[..start.len() - buf.len()], payload, varint: value });
    }
    Some(out)
}

/// Appends a length-delimited field.
pub(crate) fn put_len(out: &mut Vec<u8>, number: u32, payload: &[u8]) {
    put_varint(out, (u64::from(number) << 3) | 2);
    put_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// Appends a varint field.
pub(crate) fn put_varint_field(out: &mut Vec<u8>, number: u32, value: u64) {
    put_varint(out, u64::from(number) << 3);
    put_varint(out, value);
}

fn varint(buf: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for i in 0..10 {
        let byte = *buf.get(i)?;
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            *buf = &buf[i + 1..];
            return Some(value);
        }
    }
    None
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_every_wire_type_and_round_trips() {
        let mut msg = vec![0x08, 0x96, 0x01]; // 1: varint 150
        msg.extend([0x11, 1, 2, 3, 4, 5, 6, 7, 8]); // 2: fixed64
        put_len(&mut msg, 3, b"abc");
        msg.extend([0x25, 9, 9, 9, 9]); // 4: fixed32
        put_len(&mut msg, 300, &[0; 200]); // multi-byte key and length

        let parsed = fields(&msg).unwrap();
        assert_eq!(parsed.iter().map(|f| f.number).collect::<Vec<_>>(), [1, 2, 3, 4, 300]);
        assert_eq!(parsed[2].payload, Some(&b"abc"[..]));
        assert_eq!(parsed[0].varint, Some(150));
        let mut one = Vec::new();
        put_varint_field(&mut one, 1, 150);
        assert_eq!(one, msg[..3]);
        assert_eq!(parsed.iter().flat_map(|f| f.raw.iter().copied()).collect::<Vec<_>>(), msg);
    }

    #[test]
    fn rejects_truncated_input() {
        let mut msg = Vec::new();
        put_len(&mut msg, 3, b"abc");
        assert!(fields(&msg[..msg.len() - 1]).is_none());
        assert!(fields(&[0x08]).is_none());
        assert!(fields(&[0x0b]).is_none()); // group start
    }
}
