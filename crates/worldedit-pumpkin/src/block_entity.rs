//! NBT framing between Pumpkin's unnamed roots and the engine's named roots.

pub fn from_host(bytes: &[u8]) -> Result<Box<[u8]>, &'static str> {
    if bytes.first() != Some(&10) {
        return Err("block entity NBT has no compound root");
    }
    let mut named = Vec::with_capacity(bytes.len() + 2);
    named.extend_from_slice(&[10, 0, 0]);
    named.extend_from_slice(&bytes[1..]);
    Ok(named.into_boxed_slice())
}

pub fn to_host(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    if !bytes.starts_with(&[10, 0, 0]) {
        return Err("block entity NBT must have an empty compound root name");
    }
    let mut unnamed = Vec::with_capacity(bytes.len() - 2);
    unnamed.push(10);
    unnamed.extend_from_slice(&bytes[3..]);
    Ok(unnamed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pumpkin_nbt_framing_preserves_payload() {
        let unnamed = [10, 3, 0, 1, b'x', 0, 0, 0, 42, 0];
        let named = [10, 0, 0, 3, 0, 1, b'x', 0, 0, 0, 42, 0];
        assert_eq!(from_host(&unnamed).unwrap().as_ref(), named);
        assert_eq!(to_host(&named).unwrap(), unnamed);
        assert!(to_host(&[10, 0, 1, b'x', 0]).is_err());
        assert!(from_host(&[3]).is_err());
    }
}
