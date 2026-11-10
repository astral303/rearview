//! The hash a row's `digest` column holds: what the row stores, fed field by
//! field, each with its length or a presence mark, so two rows hash alike
//! only when every field matches.

use crate::log_entry::TokenUsage;

#[derive(Default)]
pub(super) struct Digest(blake3::Hasher);

impl Digest {
    pub(super) fn text(&mut self, text: Option<&str>) {
        match text {
            Some(text) => {
                self.0.update(&[1]);
                self.0.update(&(text.len() as u64).to_le_bytes());
                self.0.update(text.as_bytes());
            }
            None => {
                self.0.update(&[0]);
            }
        }
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
