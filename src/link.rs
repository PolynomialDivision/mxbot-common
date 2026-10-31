//! Link codes: how a person proves that a Matrix account on another server is theirs.
//!
//! From that account they write `!link <username>` to the directory bot; the bot answers
//! with [`code`], and the person enters it in the identity provider's flow
//! `house-matrix-accounts` while logged in. The flow computes the same code for its logged-in
//! user and the account they named, so a code only proves the account for the house user the
//! bot's answer named, and only for 30 to 60 minutes.
//!
//! `HMAC-SHA256(key, "house-matrix-link:v1\n<username lowercased>\n<matrix id>\n<slot>")`,
//! the first 5 bytes in Crockford base32 as `XXXX-XXXX`, `slot = unix time / 1800`; the
//! current and the previous slot are accepted. The key is shared by the bot and the
//! provider only (the vault's `matrix_link_key`). The flow's policy implements the same
//! (stacks/authentik in house-infra); the test vectors below are in both.

use std::time::{SystemTime, UNIX_EPOCH};

use matrix_sdk::ruma::UserId;
use sha2::{Digest, Sha256};

/// Seconds a slot lasts.
pub const SLOT_SECONDS: u64 = 1800;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The slot of a moment.
pub fn slot_at(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / SLOT_SECONDS
}

/// The code for `username` and `account` in `slot`, as `XXXX-XXXX`.
pub fn code(key: &[u8], username: &str, account: &UserId, slot: u64) -> String {
    let message = format!(
        "house-matrix-link:v1\n{}\n{}\n{}",
        username.to_lowercase(),
        account,
        slot
    );
    let mac = hmac_sha256(key, message.as_bytes());
    let n = mac[..5].iter().fold(0u64, |n, b| (n << 8) | u64::from(*b));
    let chars: String = (0..8)
        .map(|i| ALPHABET[((n >> (35 - 5 * i)) & 31) as usize] as char)
        .collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

/// Whether `input` is the code for `username` and `account` now or in the slot before;
/// case, dashes and spaces do not matter, O reads as 0, I and L as 1.
pub fn check(key: &[u8], username: &str, account: &UserId, input: &str, now: SystemTime) -> bool {
    let given = normalize(input);
    if given.len() != 8 {
        return false;
    }
    let slot = slot_at(now);
    [slot, slot.saturating_sub(1)]
        .iter()
        .any(|s| constant_eq(&normalize(&code(key, username, account, *s)), &given))
}

fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// RFC 2104 with SHA-256 (the crate's sha2; no extra dependency for one function).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| k.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn id(s: &str) -> &UserId {
        <&UserId>::try_from(s).unwrap()
    }

    #[test]
    fn the_test_vectors_the_provider_shares() {
        let anna = id("@anna:matrix.org");
        assert_eq!(code(b"test-key", "anna", anna, 1_000_000), "W710-C4F3");
        assert_eq!(code(b"test-key", "Anna", anna, 1_000_000), "W710-C4F3");
        assert_eq!(code(b"test-key", "anna", anna, 1_000_001), "H35F-YFCA");
        assert_eq!(
            code(b"k2", "bob", id("@Bob:example.org"), 987_654),
            "E8Z3-1ZZ9"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // test case 2 (a short key) and 6 (a key longer than the block)
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(mac[..8], [0x5b, 0xdc, 0xc1, 0x46, 0xbf, 0x60, 0x75, 0x4e]);
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(mac[..8], [0x60, 0xe4, 0x31, 0x59, 0x1e, 0xe0, 0xb6, 0x7f]);
    }

    #[test]
    fn a_code_holds_for_its_slot_and_the_next_only_and_for_its_user_and_account() {
        let anna = id("@anna:matrix.org");
        let at = UNIX_EPOCH + Duration::from_secs(1_000_000 * SLOT_SECONDS + 5);
        assert!(check(b"test-key", "anna", anna, "W710-C4F3", at));
        assert!(check(b"test-key", "ANNA", anna, " w7lo c4f3 ", at));
        // the next slot still takes it, the one after does not
        let next = at + Duration::from_secs(SLOT_SECONDS);
        assert!(check(b"test-key", "anna", anna, "W710-C4F3", next));
        assert!(!check(
            b"test-key",
            "anna",
            anna,
            "W710-C4F3",
            next + Duration::from_secs(SLOT_SECONDS)
        ));
        // another user, another account, another key, garbage
        assert!(!check(b"test-key", "ben", anna, "W710-C4F3", at));
        assert!(!check(
            b"test-key",
            "anna",
            id("@anna:example.org"),
            "W710-C4F3",
            at
        ));
        assert!(!check(b"other", "anna", anna, "W710-C4F3", at));
        assert!(!check(b"test-key", "anna", anna, "W710-C4F", at));
        assert!(!check(b"test-key", "anna", anna, "", at));
    }
}
