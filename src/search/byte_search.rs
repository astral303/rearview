//! Searches for lowercase ASCII text that ignore ASCII case and read the
//! text's bytes with SIMD instructions.
//!
//! A text's ASCII bytes alone settle such a match unless the text holds a
//! character outside ASCII whose lowercase holds ASCII: LATIN CAPITAL LETTER I
//! WITH DOT ABOVE lowercases to `i` and a combining dot, KELVIN SIGN to `k`.

use memchr::arch::all::packedpair::Pair;
use memchr::memmem;
use std::sync::LazyLock;

pub(crate) const DOTTED_CAPITAL_I: &str = "\u{130}";
pub(crate) const KELVIN_SIGN: &str = "\u{212A}";

static DOTTED_CAPITAL_I_FINDER: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(DOTTED_CAPITAL_I));
static KELVIN_SIGN_FINDER: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(KELVIN_SIGN));

/// A lowercase ASCII needle, found ignoring ASCII case. Built once per
/// search and used for every text.
#[derive(Clone, Debug)]
pub(crate) struct AsciiCaselessFinder {
    needle: Box<[u8]>,
    /// The needle byte a search looks for first, the rarest in typical text
    /// by memchr's byte frequencies. Each occurrence of it, in either case,
    /// is a candidate match to compare.
    anchor: usize,
}

impl AsciiCaselessFinder {
    pub(crate) fn new(needle: &[u8]) -> Self {
        debug_assert!(
            needle
                .iter()
                .all(|byte| byte.is_ascii() && !byte.is_ascii_uppercase()),
            "the needle is lowercase ASCII: {needle:?}"
        );
        let anchor = Pair::new(needle).map_or(0, |pair| usize::from(pair.index1()));
        Self {
            needle: needle.into(),
            anchor,
        }
    }

    pub(crate) fn needle(&self) -> &[u8] {
        &self.needle
    }

    /// False when `haystack` holds neither the anchor byte, in either case,
    /// nor a character lowercasing to it: `haystack` lowercased then cannot
    /// hold the needle.
    pub(crate) fn anchor_may_occur_in(&self, haystack: &str) -> bool {
        let Some(&lower) = self.needle.get(self.anchor) else {
            return false;
        };
        let upper = lower.to_ascii_uppercase();
        memchr::memchr2(lower, upper, haystack.as_bytes()).is_some()
            || NonAsciiCapitals::matchable_by(&[lower])
                .first_in(haystack)
                .is_some()
    }

    /// The byte where the needle next occurs in `haystack` from byte `from`,
    /// ignoring ASCII case. An empty needle occurs nowhere.
    pub(crate) fn find(&self, haystack: &[u8], from: usize) -> Option<usize> {
        let needle = &*self.needle;
        let last_start = haystack.len().checked_sub(needle.len())?;
        let lower = *needle.get(self.anchor)?;
        let upper = lower.to_ascii_uppercase();
        let mut start = from;
        while start <= last_start {
            let window = &haystack[start + self.anchor..=last_start + self.anchor];
            let offset = if lower == upper {
                memchr::memchr(lower, window)
            } else {
                memchr::memchr2(lower, upper, window)
            }?;
            let candidate = start + offset;
            if haystack[candidate..candidate + needle.len()].eq_ignore_ascii_case(needle) {
                return Some(candidate);
            }
            start = candidate + 1;
        }
        None
    }
}

/// The characters outside ASCII whose lowercase holds a letter of a
/// lowercase ASCII needle. From the first of them in a text, the needle's
/// matches there depend on more than the text's ASCII bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NonAsciiCapitals {
    dotted_capital_i: bool,
    kelvin_sign: bool,
}

impl NonAsciiCapitals {
    pub(crate) fn matchable_by(needle: &[u8]) -> Self {
        Self {
            dotted_capital_i: needle.contains(&b'i'),
            kelvin_sign: needle.contains(&b'k'),
        }
    }

    /// The byte where `text` first holds one of them.
    pub(crate) fn first_in(self, text: &str) -> Option<usize> {
        let dotted_capital_i = self
            .dotted_capital_i
            .then(|| DOTTED_CAPITAL_I_FINDER.find(text.as_bytes()))
            .flatten();
        let kelvin_sign = self
            .kelvin_sign
            .then(|| KELVIN_SIGN_FINDER.find(text.as_bytes()))
            .flatten();
        dotted_capital_i.into_iter().chain(kelvin_sign).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every start of `needle` in `haystack` from `from`, one byte at a time.
    fn find_each_byte(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        (from..=haystack.len().checked_sub(needle.len())?)
            .find(|&start| haystack[start..start + needle.len()].eq_ignore_ascii_case(needle))
            .filter(|_| !needle.is_empty())
    }

    #[test]
    fn finder_finds_what_comparing_each_position_finds() {
        let needles = ["cache", "refresh", "a", "zz", "x9", "_", "elan", "q"];
        let haystacks = [
            "",
            "c",
            "cache",
            "CACHE at the start and the end: cAcHe",
            "caches, mycache, recache_cache",
            "a cach cache",
            "refreshed REFRESH Refresh",
            "élan ÉLAN elan, café cache ü cache",
            "zzz zZ Zz",
            "x9X9 _x_ q",
        ];
        for needle in needles {
            let finder = AsciiCaselessFinder::new(needle.as_bytes());
            for haystack in haystacks {
                for from in 0..=haystack.len() + 1 {
                    assert_eq!(
                        finder.find(haystack.as_bytes(), from),
                        find_each_byte(haystack.as_bytes(), needle.as_bytes(), from),
                        "the finder finds {needle:?} in {haystack:?} from {from} where each \
                         position compared finds it"
                    );
                }
            }
        }
    }

    #[test]
    fn empty_needle_occurs_nowhere() {
        assert_eq!(AsciiCaselessFinder::new(b"").find(b"text", 0), None);
    }

    #[test]
    fn only_kelvin_sign_and_dotted_capital_i_lowercase_to_ascii_outside_ascii() {
        let lowercasing_to_ascii = (0..=u32::from(char::MAX))
            .filter_map(char::from_u32)
            .filter(|ch| !ch.is_ascii() && ch.to_lowercase().any(|lower| lower.is_ascii()))
            .map(String::from)
            .collect::<Vec<_>>();

        assert_eq!(lowercasing_to_ascii, [DOTTED_CAPITAL_I, KELVIN_SIGN]);
    }

    #[test]
    fn non_ascii_capitals_are_those_lowercasing_to_needles_letters() {
        let text = "ok \u{212A} then \u{130}";
        let first_in = |needle: &[u8], text| NonAsciiCapitals::matchable_by(needle).first_in(text);
        assert_eq!(
            first_in(b"kit", text),
            Some(3),
            "the kelvin sign comes first"
        );
        assert_eq!(
            first_in(b"is", text),
            text.find('\u{130}'),
            "only the dotted capital i lowercases to `i`"
        );
        assert_eq!(
            first_in(b"cache", text),
            None,
            "neither lowercases to `cache`'s letters"
        );
        assert_eq!(
            first_in(b"kit", "plain text"),
            None,
            "plain text holds neither"
        );
    }
}
