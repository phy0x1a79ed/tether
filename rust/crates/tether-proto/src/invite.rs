//! The invite code: the whole credential, in a form a person can say out loud.
//!
//! A code is a **slot** and a **phrase**, written as plain tokens with nothing
//! to quote or punctuate:
//!
//! ```text
//! 7 anchor kettle
//! ```
//!
//! The two halves are secret in opposite ways, and keeping them apart is the
//! reason the design is safe at this size.
//!
//! The **slot** is issued by the relay and is public. It is how two sockets
//! find each other, so it travels in a URL and lands in every access log on the
//! path, and none of that matters because it authenticates nobody.
//!
//! The **phrase** is the secret and the relay never learns it, in plaintext or
//! hashed or in any other form. That is not fastidiousness. Two words carry
//! twenty bits, so anything derived from the phrase and handed to the relay
//! could be reversed by enumerating the whole space in milliseconds, and a
//! relay that knows the phrase can stand in the middle of a
//! password-authenticated exchange — which is precisely the attack that
//! exchange exists to stop. The slot exists so the phrase never has to travel.
//!
//! Twenty bits is small, and it is defensible only because of what surrounds
//! it: a session is one-shot, it expires in minutes, and it is destroyed after
//! a few failed handshakes. Every guess must be a live attempt against a live
//! session. Those three properties are load-bearing, not hardening.
//!
//! Raising the word count costs the minting side one constant and costs the
//! owner one more word to read, because the launcher takes tokens as ordinary
//! arguments.

use std::fmt;

use crate::words::{WORDS, WORD_BITS};

/// Words in a freshly minted phrase. Two is the deliberate floor, not a
/// default that drifted: see the module docs for what makes it safe.
pub const DEFAULT_WORDS: usize = 2;

/// The most words a code may carry. Nothing needs this many; it exists so a
/// malformed argument list is rejected rather than hashed.
pub const MAX_WORDS: usize = 8;

/// The highest slot the relay may issue. Also the ceiling on live sessions,
/// which is why it is small: a slot is read aloud, so it stays short.
pub const MAX_SLOT: u32 = 999;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InviteError {
    #[error("an invite code is a slot then {DEFAULT_WORDS} or more words, like `7 anchor kettle`")]
    TooFewTokens,
    #[error("that is more than {MAX_WORDS} words")]
    TooManyWords,
    #[error("`{0}` is not a slot number")]
    BadSlot(String),
    #[error("slot {0} is out of range")]
    SlotOutOfRange(u32),
    #[error("`{0}` is not one of the invite words")]
    UnknownWord(String),
}

/// A relay-issued session slot. Public, and the only thing the relay is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u32);

impl Slot {
    pub fn new(n: u32) -> Result<Self, InviteError> {
        if n == 0 || n > MAX_SLOT {
            return Err(InviteError::SlotOutOfRange(n));
        }
        Ok(Self(n))
    }

    pub fn get(self) -> u32 {
        self.0
    }

    /// Parse a slot out of a URL path segment.
    ///
    /// Deliberately strict: no sign, no leading zero, no whitespace. The public
    /// edge refuses anything that does not match this shape before it consults
    /// anything else, so the shape has to be one both ends agree on exactly.
    pub fn parse(s: &str) -> Result<Self, InviteError> {
        let bad = || InviteError::BadSlot(s.to_string());
        if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        if s.len() > 1 && s.starts_with('0') {
            return Err(bad());
        }
        Self::new(s.parse::<u32>().map_err(|_| bad())?)
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The secret half of an invite code: the words, and nothing else.
///
/// Held apart from the slot as its own type so that handing the phrase to the
/// relay has to be written on purpose rather than happening because a struct
/// was passed whole.
#[derive(Clone, PartialEq, Eq)]
pub struct Phrase {
    words: Vec<&'static str>,
}

impl Phrase {
    /// Mint a phrase from the operating system's randomness.
    ///
    /// Ten bits per word out of a 1024-word list, so every word is uniform and
    /// there is no rejection loop to get subtly wrong.
    pub fn mint(word_count: usize) -> Result<Self, InviteError> {
        if word_count < 1 {
            return Err(InviteError::TooFewTokens);
        }
        if word_count > MAX_WORDS {
            return Err(InviteError::TooManyWords);
        }
        let mut seed = vec![0u8; word_count * 2];
        getrandom::getrandom(&mut seed).expect("the OS randomness source failed");
        let mask = (1u16 << WORD_BITS) - 1;
        let words = seed
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| WORDS[(u16::from_le_bytes(*c) & mask) as usize])
            .collect();
        Ok(Self { words })
    }

    /// Read a phrase the owner typed.
    ///
    /// Case and surrounding whitespace are forgiven because they are typing,
    /// not secret. An unknown word is not: it is reported by name so the owner
    /// is told which word to say again, rather than watching a handshake fail
    /// for reasons nobody can see.
    pub fn parse<S: AsRef<str>>(tokens: &[S]) -> Result<Self, InviteError> {
        if tokens.is_empty() {
            return Err(InviteError::TooFewTokens);
        }
        if tokens.len() > MAX_WORDS {
            return Err(InviteError::TooManyWords);
        }
        let mut words = Vec::with_capacity(tokens.len());
        for t in tokens {
            let w = t.as_ref().trim().to_ascii_lowercase();
            match WORDS.binary_search(&w.as_str()) {
                Ok(i) => words.push(WORDS[i]),
                Err(_) => return Err(InviteError::UnknownWord(w)),
            }
        }
        Ok(Self { words })
    }

    pub fn words(&self) -> &[&'static str] {
        &self.words
    }

    /// The bytes the password-authenticated exchange is run over.
    pub fn as_password(&self) -> Vec<u8> {
        self.words.join(" ").into_bytes()
    }

    /// How many bits of guessing this phrase costs an attacker.
    pub fn entropy_bits(&self) -> u32 {
        self.words.len() as u32 * WORD_BITS
    }
}

impl fmt::Display for Phrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.words.join(" "))
    }
}

/// Deliberately not the words. A phrase reaches a log only because something
/// printed it, and the easiest way for that to happen is a struct that derives
/// `Debug` and gets logged whole by something two layers away.
impl fmt::Debug for Phrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Phrase({} words, redacted)", self.words.len())
    }
}

/// A slot and a phrase: what the operator reads out and the owner types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCode {
    pub slot: Slot,
    pub phrase: Phrase,
}

impl InviteCode {
    /// Read the tokens the launcher was invoked with: slot first, then words.
    pub fn parse<S: AsRef<str>>(tokens: &[S]) -> Result<Self, InviteError> {
        let tokens: Vec<&str> = tokens
            .iter()
            .map(|t| t.as_ref().trim())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.len() < 2 {
            return Err(InviteError::TooFewTokens);
        }
        Ok(Self {
            slot: Slot::parse(tokens[0])?,
            phrase: Phrase::parse(&tokens[1..])?,
        })
    }
}

impl fmt::Display for InviteCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.slot, self.phrase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything `words.rs` claims about the list, re-checked here so the
    /// claim cannot quietly stop being true. The list is the credential
    /// vocabulary; a word that slips in against these rules is one the owner
    /// mishears, and the failure surfaces as a handshake that will not complete
    /// for no visible reason.
    mod vocabulary {
        use super::*;

        fn edit_distance(a: &str, b: &str) -> usize {
            if a.len().abs_diff(b.len()) > 1 {
                return 2;
            }
            let (a, b) = (a.as_bytes(), b.as_bytes());
            let mut prev: Vec<usize> = (0..=b.len()).collect();
            for (i, &ca) in a.iter().enumerate() {
                let mut cur = vec![i + 1];
                for (j, &cb) in b.iter().enumerate() {
                    cur.push(
                        (prev[j + 1] + 1)
                            .min(cur[j] + 1)
                            .min(prev[j] + usize::from(ca != cb)),
                    );
                }
                prev = cur;
            }
            prev[b.len()]
        }

        #[test]
        fn the_list_is_a_power_of_two_so_minting_is_unbiased() {
            assert_eq!(WORDS.len(), 1usize << WORD_BITS);
        }

        #[test]
        fn the_list_is_sorted_and_free_of_duplicates() {
            for pair in WORDS.windows(2) {
                assert!(pair[0] < pair[1], "out of order or duplicated: {pair:?}");
            }
        }

        #[test]
        fn every_word_is_short_lowercase_ascii() {
            for w in WORDS {
                assert!((3..=8).contains(&w.len()), "{w} is the wrong length");
                assert!(
                    w.bytes().all(|b| b.is_ascii_lowercase()),
                    "{w} is not lowercase ascii"
                );
            }
        }

        #[test]
        fn no_word_opens_with_a_silent_letter() {
            for w in WORDS {
                let opening = &w[..2];
                assert!(
                    !matches!(opening, "kn" | "wr" | "gn" | "ps" | "pn" | "rh"),
                    "{w} cannot be spelled by someone who has only heard it"
                );
            }
        }

        #[test]
        fn no_two_words_share_a_three_letter_opening() {
            for pair in WORDS.windows(2) {
                assert_ne!(&pair[0][..3], &pair[1][..3], "{pair:?} open alike");
            }
        }

        #[test]
        fn no_two_words_are_one_edit_apart() {
            for (i, a) in WORDS.iter().enumerate() {
                for b in &WORDS[i + 1..] {
                    assert!(
                        edit_distance(a, b) >= 2,
                        "{a} and {b} are one mistake apart"
                    );
                }
            }
        }
    }

    #[test]
    fn a_minted_phrase_round_trips_through_what_the_owner_types() {
        for _ in 0..64 {
            let minted = Phrase::mint(DEFAULT_WORDS).unwrap();
            let typed: Vec<String> = minted
                .to_string()
                .split(' ')
                .map(|w| format!("  {} ", w.to_uppercase()))
                .collect();
            assert_eq!(Phrase::parse(&typed).unwrap(), minted);
        }
    }

    #[test]
    fn the_default_phrase_carries_the_entropy_the_limits_assume() {
        assert_eq!(Phrase::mint(DEFAULT_WORDS).unwrap().entropy_bits(), 20);
    }

    #[test]
    fn an_unknown_word_is_named_rather_than_swallowed() {
        let err = Phrase::parse(&["anchor", "quokka"]).unwrap_err();
        assert_eq!(err, InviteError::UnknownWord("quokka".into()));
    }

    #[test]
    fn a_phrase_does_not_print_itself_when_something_logs_it() {
        let phrase = Phrase::parse(&["anchor", "kettle"]).unwrap();
        let logged = format!("{phrase:?}");
        assert!(!logged.contains("anchor"), "{logged}");
        assert!(!logged.contains("kettle"), "{logged}");
    }

    #[test]
    fn a_code_reads_back_the_way_it_was_read_out() {
        let code = InviteCode::parse(&["7", "anchor", "kettle"]).unwrap();
        assert_eq!(code.slot.get(), 7);
        assert_eq!(code.to_string(), "7 anchor kettle");
    }

    #[test]
    fn a_slot_is_a_plain_number_and_nothing_else() {
        for bad in ["", "0", "007", "-1", "1 ", "1a", "1000", "٧"] {
            assert!(Slot::parse(bad).is_err(), "{bad} was accepted as a slot");
        }
        for good in ["1", "7", "42", "999"] {
            assert!(Slot::parse(good).is_ok(), "{good} was refused");
        }
    }

    #[test]
    fn a_code_without_a_slot_is_refused_rather_than_guessed_at() {
        assert_eq!(
            InviteCode::parse(&["anchor", "kettle"]).unwrap_err(),
            InviteError::BadSlot("anchor".into())
        );
        assert_eq!(
            InviteCode::parse(&["7"]).unwrap_err(),
            InviteError::TooFewTokens
        );
    }
}
