//! The hash a row's `digest` column holds: the first 8 bytes of a BLAKE3
//! hash of the fields the row stores, in order. Each field enters with a
//! presence mark; a text or byte field also with its length, and a JSON
//! value as the hash of its text.

use crate::log_entry::TokenUsage;

#[derive(Default)]
pub(super) struct Digest(blake3::Hasher);

impl Digest {
    pub(super) fn text(&mut self, text: Option<&str>) {
        self.bytes(text.map(str::as_bytes));
    }

    pub(super) fn bytes(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            Some(bytes) => {
                self.0.update(&[1]);
                self.0.update(&(bytes.len() as u64).to_le_bytes());
                self.0.update(bytes);
            }
            None => {
                self.0.update(&[0]);
            }
        }
    }

    /// Hashes `value`'s JSON text as one field without building the text:
    /// the field holds the text's own hash.
    pub(super) fn json(&mut self, value: Option<&serde_json::Value>) {
        let Some(value) = value else {
            return self.bytes(None);
        };
        let mut text = blake3::Hasher::new();
        serde_json::to_writer(&mut text, value).expect("a JSON value writes to a hasher");
        self.bytes(Some(text.finalize().as_bytes()));
    }

    pub(super) fn number(&mut self, number: Option<i64>) {
        match number {
            Some(number) => {
                self.0.update(&[1]);
                self.0.update(&number.to_le_bytes());
            }
            None => {
                self.0.update(&[0]);
            }
        }
    }

    pub(super) fn flag(&mut self, flag: Option<bool>) {
        self.number(flag.map(i64::from));
    }

    pub(super) fn usage(&mut self, usage: Option<&TokenUsage>) {
        self.flag(Some(usage.is_some()));
        if let Some(usage) = usage {
            for count in [
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
            ] {
                self.number(Some(count as i64));
            }
        }
    }

    pub(super) fn finish(&self) -> i64 {
        let hash = self.0.finalize();
        let bytes: [u8; 8] = hash.as_bytes()[..8]
            .try_into()
            .expect("a hash holds 8 bytes");
        i64::from_le_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::Digest;

    fn json_digest(record: &str) -> i64 {
        let value: serde_json::Value = serde_json::from_str(record).unwrap();
        let mut digest = Digest::default();
        digest.json(Some(&value));
        digest.finish()
    }

    #[test]
    fn two_parses_of_one_record_hash_alike() {
        let record =
            r#"{"command":"cargo test","timeout":120000,"nested":{"b":[1,2.5,null],"a":"x"}}"#;

        assert_eq!(json_digest(record), json_digest(record));
    }

    /// Turning on `serde_json`'s `preserve_order` feature fails this test,
    /// because the hash then follows the order of the keys.
    #[test]
    fn one_object_in_two_key_orders_hashes_alike() {
        assert_eq!(
            json_digest(r#"{"command":"cargo test","nested":{"b":[1,null],"a":"x"}}"#),
            json_digest(r#"{"nested":{"a":"x","b":[1,null]},"command":"cargo test"}"#)
        );
    }

    #[test]
    fn json_digest_differs_from_an_absent_value() {
        let mut absent = Digest::default();
        absent.json(None);

        assert_ne!(json_digest("null"), absent.finish());
    }
}
