//! Identifier helpers.
//!
//! The TS server makes every id with `crypto.randomUUID()` (Effect `Crypto.randomUUIDv4`):
//! lowercase, hyphenated, version 4. Several ids carry a persisted prefix convention (see the plan
//! §5.2: `server:<tag>:<uuid>`, `provider:<runtimeEventId>:<tag>:<uuid>`, …); the helpers below
//! build those so every crate formats them the same way.

use rand::RngCore;

/// A random version-4 UUID in the canonical lowercase hyphenated form, like `crypto.randomUUID()`.
pub fn uuid_v4() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

/// True when `value` is a canonical lowercase hyphenated UUID of any version.
pub fn is_canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && uuid::Uuid::try_parse(value)
            .map(|parsed| parsed.hyphenated().to_string() == value)
            .unwrap_or(false)
}

/// `server:<tag>:<uuid>`: commands the server dispatches on its own behalf.
pub fn server_command_id(tag: &str) -> String {
    format!("server:{tag}:{}", uuid_v4())
}

/// `provider:<runtimeEventId>:<tag>:<uuid>`: commands derived from a provider runtime event.
pub fn provider_command_id(runtime_event_id: &str, tag: &str) -> String {
    format!("provider:{runtime_event_id}:{tag}:{}", uuid_v4())
}

/// `n` cryptographically random bytes (OS RNG), like Effect `Crypto.randomBytes`.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_is_canonical_and_versioned() {
        let id = uuid_v4();
        assert!(is_canonical_uuid(&id), "{id}");
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(uuid_v4(), uuid_v4());
    }

    #[test]
    fn rejects_uppercase_and_braced_forms() {
        assert!(!is_canonical_uuid("6F9619FF-8B86-D011-B42D-00CF4FC964FF"));
        assert!(!is_canonical_uuid("{6f9619ff-8b86-d011-b42d-00cf4fc964ff}"));
        assert!(is_canonical_uuid("6f9619ff-8b86-d011-b42d-00cf4fc964ff"));
    }

    #[test]
    fn command_ids_follow_the_persisted_conventions() {
        let id = server_command_id("auto-settle");
        assert!(id.starts_with("server:auto-settle:"));
        assert!(is_canonical_uuid(&id["server:auto-settle:".len()..]));
        assert!(provider_command_id("evt-1", "session-set").starts_with("provider:evt-1:session-set:"));
        assert_eq!(random_bytes(32).len(), 32);
    }
}
