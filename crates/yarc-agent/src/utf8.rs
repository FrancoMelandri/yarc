//! Turns arbitrary byte chunks into text without cutting characters in half.

/// Holds back an incomplete trailing UTF-8 sequence until its rest arrives.
/// Invalid bytes become U+FFFD, like `String::from_utf8_lossy`.
#[derive(Default)]
pub struct Utf8Chunker {
    pending: Vec<u8>,
}

impl Utf8Chunker {
    /// Text for `bytes` (and anything held back before); `None` if nothing is complete yet.
    pub fn push(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        let keep = incomplete_tail(&self.pending);
        let ready: Vec<u8> = self.pending.drain(..self.pending.len() - keep).collect();
        (!ready.is_empty()).then(|| String::from_utf8_lossy(&ready).into_owned())
    }

    /// At the end of the stream whatever is left is as good as it gets.
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.pending);
        (!rest.is_empty()).then(|| String::from_utf8_lossy(&rest).into_owned())
    }
}

/// How many bytes at the end form the start of a valid, not yet complete, character.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let b = bytes[bytes.len() - back];
        if b & 0b1100_0000 == 0b1000_0000 {
            continue; // a continuation byte: keep looking for the lead byte
        }
        let need = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        return if back < need { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_characters_pass_straight_through() {
        let mut c = Utf8Chunker::default();
        assert_eq!(c.push("héllo ✓\n".as_bytes()).as_deref(), Some("héllo ✓\n"));
        assert_eq!(c.finish(), None);
    }

    #[test]
    fn a_character_split_across_chunks_is_rejoined() {
        let bytes = "a✓b".as_bytes(); // ✓ is 3 bytes
        for cut in 1..bytes.len() {
            let mut c = Utf8Chunker::default();
            let mut text = String::new();
            text.extend(c.push(&bytes[..cut]));
            text.extend(c.push(&bytes[cut..]));
            text.extend(c.finish());
            assert_eq!(text, "a✓b", "cut at {cut}");
        }
    }

    #[test]
    fn invalid_bytes_are_replaced_and_a_dangling_lead_byte_is_flushed_at_the_end() {
        let mut c = Utf8Chunker::default();
        assert_eq!(c.push(&[b'a', 0xff, b'b']).as_deref(), Some("a\u{fffd}b"));
        assert_eq!(c.push(&[0xE2, 0x9C]), None, "held back");
        assert_eq!(c.finish().as_deref(), Some("\u{fffd}"));
    }
}
