use sha2::{Digest, Sha256};

/// SHA-256 content fingerprint used for stale-finding and transaction
/// checks. It is intentionally public so every layer computes exactly the
/// same value instead of growing subtly incompatible hash schemes.
pub fn content_hash(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

/// Stable short hash for user-facing IDs and workspace namespaces. Twelve
/// hex characters retain enough entropy for local report identifiers while
/// keeping them readable in CLI output.
pub fn stable_hash(bytes: impl AsRef<[u8]>) -> String {
    content_hash(bytes).chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_sha256_and_stable() {
        assert_eq!(
            content_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(stable_hash("abc"), "ba7816bf8f01");
    }
}
