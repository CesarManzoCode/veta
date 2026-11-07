//! `LIKE` / `NOT LIKE` (contract §10.3): `%` matches any sequence, `_` one character and
//! `\` escapes the next character.
//!
//! The pattern is compiled once. Patterns without `_` take fast paths: `'lit'` (equality),
//! `'lit%'` (prefix), `'%lit'` (suffix), `'%lit%'` (substring search over the whole string
//! buffer of the vector) and `'a%b%c'` (segments in order). `_` matches one UTF-8 character
//! (one byte if the byte does not start a valid sequence length), as the oracles do on
//! UTF-8 text; `%` advances by characters too.

use veta_core::{Result, VetaError};

/// How [`Finder::find`] scans for candidates, chosen once when the finder is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// Horspool only (CPUs other than x86-64; on x86-64 only the tests build it).
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    Scalar,
    /// 16 positions at a time on the first and last byte of the needle (SSE2, always
    /// present on x86-64).
    Sse2,
    /// 32 positions at a time (AVX2, detected at run time).
    Avx2,
}

impl Scan {
    /// The widest scan this CPU runs.
    fn detect() -> Scan {
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                return Scan::Avx2;
            }
            Scan::Sse2
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Scan::Scalar
        }
    }
}

/// Substring search for a needle fixed per query: a SIMD scan that compares the first and
/// the last byte of the needle at 16 or 32 positions at once and checks the middle bytes
/// only where both match, with Horspool for the tail and on CPUs without SIMD.
#[derive(Debug, Clone)]
pub struct Finder {
    needle: Vec<u8>,
    skip: Box<[usize; 256]>,
    scan: Scan,
}

impl Finder {
    pub fn new(needle: Vec<u8>) -> Finder {
        Finder::with_scan(needle, Scan::detect())
    }

    fn with_scan(needle: Vec<u8>, scan: Scan) -> Finder {
        let m = needle.len();
        let mut skip = Box::new([m.max(1); 256]);
        if m > 1 {
            for (i, &b) in needle[..m - 1].iter().enumerate() {
                skip[b as usize] = m - 1 - i;
            }
        }
        Finder { needle, skip, scan }
    }

    pub fn needle(&self) -> &[u8] {
        &self.needle
    }

    /// Position of the first occurrence of the needle in `hay`.
    #[inline]
    pub fn find(&self, hay: &[u8]) -> Option<usize> {
        let m = self.needle.len();
        if m == 0 {
            return Some(0);
        }
        if hay.len() < m {
            return None;
        }
        match self.scan {
            Scan::Scalar => self.horspool(hay, 0),
            #[cfg(target_arch = "x86_64")]
            Scan::Sse2 => {
                // SAFETY: SSE2 is part of the x86-64 baseline.
                unsafe { simd::find_sse2(self, hay) }
            }
            #[cfg(target_arch = "x86_64")]
            Scan::Avx2 => {
                // SAFETY: `Scan::Avx2` is only chosen when the CPU reports AVX2.
                unsafe { simd::find_avx2(self, hay) }
            }
            #[cfg(not(target_arch = "x86_64"))]
            _ => self.horspool(hay, 0),
        }
    }

    /// First occurrence at a position `≥ from`, by Horspool.
    #[inline]
    fn horspool(&self, hay: &[u8], from: usize) -> Option<usize> {
        let m = self.needle.len();
        if m == 1 {
            return hay[from..]
                .iter()
                .position(|&b| b == self.needle[0])
                .map(|p| p + from);
        }
        let last = m - 1;
        let tail = self.needle[last];
        let mut i = from;
        while i + m <= hay.len() {
            let c = hay[i + last];
            if c == tail && hay[i..i + last] == self.needle[..last] {
                return Some(i);
            }
            i += self.skip[c as usize];
        }
        None
    }
}

#[cfg(target_arch = "x86_64")]
mod simd {
    use std::arch::x86_64::*;

    use super::Finder;

    /// Whether the middle of the needle (between its first and last byte) is at `at`.
    #[inline(always)]
    fn middle_matches(hay: &[u8], at: usize, needle: &[u8]) -> bool {
        let m = needle.len();
        m <= 2 || hay[at + 1..at + m - 1] == needle[1..m - 1]
    }

    /// SSE2 scan; every position below the returned one (or below the tail handed to
    /// Horspool) was checked, so the result is the first occurrence.
    ///
    /// # Safety
    ///
    /// The CPU must support SSE2 (always true on x86-64).
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn find_sse2(finder: &Finder, hay: &[u8]) -> Option<usize> {
        let needle = finder.needle.as_slice();
        let m = needle.len();
        let first = _mm_set1_epi8(needle[0] as i8);
        let last = _mm_set1_epi8(needle[m - 1] as i8);
        let mut i = 0;
        while i + m + 15 <= hay.len() {
            // SAFETY: `i + 15 < hay.len()` and `i + m - 1 + 15 < hay.len()` by the loop
            // condition, so both unaligned 16-byte loads stay inside `hay`.
            let (a, b) = unsafe {
                (
                    _mm_loadu_si128(hay.as_ptr().add(i) as *const __m128i),
                    _mm_loadu_si128(hay.as_ptr().add(i + m - 1) as *const __m128i),
                )
            };
            let eq = _mm_and_si128(_mm_cmpeq_epi8(a, first), _mm_cmpeq_epi8(b, last));
            let mut mask = _mm_movemask_epi8(eq) as u32;
            while mask != 0 {
                let at = i + mask.trailing_zeros() as usize;
                if middle_matches(hay, at, needle) {
                    return Some(at);
                }
                mask &= mask - 1;
            }
            i += 16;
        }
        finder.horspool(hay, i)
    }

    /// AVX2 scan, as [`find_sse2`] with 32 positions per step.
    ///
    /// # Safety
    ///
    /// The CPU must support AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn find_avx2(finder: &Finder, hay: &[u8]) -> Option<usize> {
        let needle = finder.needle.as_slice();
        let m = needle.len();
        let first = _mm256_set1_epi8(needle[0] as i8);
        let last = _mm256_set1_epi8(needle[m - 1] as i8);
        let mut i = 0;
        while i + m + 31 <= hay.len() {
            // SAFETY: `i + 31 < hay.len()` and `i + m - 1 + 31 < hay.len()` by the loop
            // condition, so both unaligned 32-byte loads stay inside `hay`.
            let (a, b) = unsafe {
                (
                    _mm256_loadu_si256(hay.as_ptr().add(i) as *const __m256i),
                    _mm256_loadu_si256(hay.as_ptr().add(i + m - 1) as *const __m256i),
                )
            };
            let eq = _mm256_and_si256(_mm256_cmpeq_epi8(a, first), _mm256_cmpeq_epi8(b, last));
            let mut mask = _mm256_movemask_epi8(eq) as u32;
            while mask != 0 {
                let at = i + mask.trailing_zeros() as usize;
                if middle_matches(hay, at, needle) {
                    return Some(at);
                }
                mask &= mask - 1;
            }
            i += 32;
        }
        // SAFETY: SSE2 is part of the x86-64 baseline.
        unsafe { find_sse2_from(finder, hay, i) }
    }

    /// The SSE2 scan from position `from` on.
    ///
    /// # Safety
    ///
    /// The CPU must support SSE2.
    #[target_feature(enable = "sse2")]
    unsafe fn find_sse2_from(finder: &Finder, hay: &[u8], from: usize) -> Option<usize> {
        // SAFETY: same requirement as this function.
        unsafe { find_sse2(finder, &hay[from..]) }.map(|p| p + from)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Byte(u8),
    /// `_`
    One,
    /// `%`
    Many,
}

#[derive(Debug, Clone)]
enum Kind {
    /// Only `%`: every string matches.
    Any,
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Contains(Finder),
    /// `prefix%m1%m2%…%suffix` without `_`.
    Segments {
        prefix: Vec<u8>,
        middle: Vec<Finder>,
        suffix: Vec<u8>,
    },
    /// Any pattern with `_`.
    General(Vec<Token>),
}

/// A compiled `LIKE` pattern.
#[derive(Debug, Clone)]
pub struct LikePattern {
    kind: Kind,
}

#[inline]
fn char_len(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn parse(pattern: &[u8]) -> Result<Vec<Token>> {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut i = 0;
    while i < pattern.len() {
        let token = match pattern[i] {
            b'\\' => {
                i += 1;
                let b = *pattern.get(i).ok_or_else(|| {
                    VetaError::user(format!(
                        "LIKE pattern '{}' ends with the escape character",
                        String::from_utf8_lossy(pattern)
                    ))
                })?;
                Token::Byte(b)
            }
            b'%' => Token::Many,
            b'_' => Token::One,
            b => Token::Byte(b),
        };
        // Consecutive `%` are one.
        if !(token == Token::Many && tokens.last() == Some(&Token::Many)) {
            tokens.push(token);
        }
        i += 1;
    }
    Ok(tokens)
}

fn general_match(tokens: &[Token], s: &[u8]) -> bool {
    let (mut t, mut i) = (0usize, 0usize);
    // Last `%` seen: token index and the position it currently stops at.
    let mut star: Option<(usize, usize)> = None;
    loop {
        if t < tokens.len() {
            match tokens[t] {
                Token::Many => {
                    star = Some((t, i));
                    t += 1;
                    continue;
                }
                Token::Byte(b) if i < s.len() && s[i] == b => {
                    t += 1;
                    i += 1;
                    continue;
                }
                Token::One if i < s.len() => {
                    i += char_len(s[i]).min(s.len() - i);
                    t += 1;
                    continue;
                }
                _ => {}
            }
        } else if i == s.len() {
            return true;
        }
        match star {
            Some((st, sp)) if sp < s.len() => {
                let next = sp + char_len(s[sp]).min(s.len() - sp);
                star = Some((st, next));
                t = st + 1;
                i = next;
            }
            _ => return false,
        }
    }
}

fn segments_match(prefix: &[u8], middle: &[Finder], suffix: &[u8], s: &[u8]) -> bool {
    if s.len() < prefix.len() + suffix.len() || !s.starts_with(prefix) || !s.ends_with(suffix) {
        return false;
    }
    let mut rest = &s[prefix.len()..s.len() - suffix.len()];
    for finder in middle {
        match finder.find(rest) {
            Some(p) => rest = &rest[p + finder.needle().len()..],
            None => return false,
        }
    }
    true
}

impl LikePattern {
    /// Compiles `pattern`; a trailing `\` is an error (exit 1).
    pub fn new(pattern: &[u8]) -> Result<LikePattern> {
        let tokens = parse(pattern)?;
        if tokens.contains(&Token::One) {
            return Ok(LikePattern {
                kind: Kind::General(tokens),
            });
        }
        let leading = tokens.first() == Some(&Token::Many);
        let trailing = tokens.last() == Some(&Token::Many);
        let mut segments: Vec<Vec<u8>> = vec![Vec::new()];
        for t in &tokens {
            match t {
                Token::Many => segments.push(Vec::new()),
                Token::Byte(b) => segments.last_mut().map_or((), |s| s.push(*b)),
                Token::One => {}
            }
        }
        let many = segments.len() - 1;
        let kind = if many == 0 {
            Kind::Exact(segments.pop().unwrap_or_default())
        } else if segments.iter().all(Vec::is_empty) {
            Kind::Any
        } else if many == 1 && trailing {
            Kind::Prefix(segments.swap_remove(0))
        } else if many == 1 && leading {
            Kind::Suffix(segments.pop().unwrap_or_default())
        } else if many == 2 && leading && trailing {
            Kind::Contains(Finder::new(segments.swap_remove(1)))
        } else {
            let suffix = segments.pop().unwrap_or_default();
            let prefix = segments.remove(0);
            Kind::Segments {
                prefix,
                middle: segments.into_iter().map(Finder::new).collect(),
                suffix,
            }
        };
        Ok(LikePattern { kind })
    }

    /// Relative cost of matching one string, to order the conjuncts of a filter.
    pub fn cost(&self) -> u32 {
        match &self.kind {
            Kind::Any => 1,
            Kind::Exact(_) | Kind::Prefix(_) | Kind::Suffix(_) => 8,
            Kind::Contains(_) => 16,
            Kind::Segments { middle, .. } => 16 + 8 * middle.len() as u32,
            Kind::General(_) => 48,
        }
    }

    /// Whether `s` matches the pattern.
    pub fn matches(&self, s: &[u8]) -> bool {
        match &self.kind {
            Kind::Any => true,
            Kind::Exact(lit) => s == lit.as_slice(),
            Kind::Prefix(lit) => s.starts_with(lit),
            Kind::Suffix(lit) => s.ends_with(lit),
            Kind::Contains(f) => f.find(s).is_some(),
            Kind::Segments {
                prefix,
                middle,
                suffix,
            } => segments_match(prefix, middle, suffix, s),
            Kind::General(tokens) => general_match(tokens, s),
        }
    }

    /// Matches every string of a `Varchar` buffer (`offsets` has `n + 1` entries), with
    /// the pattern kind chosen once outside the loop. Results of NULL slots are
    /// meaningless.
    pub fn matches_all(&self, offsets: &[u32], bytes: &[u8]) -> Vec<bool> {
        let n = offsets.len().saturating_sub(1);
        let at = |i: usize| &bytes[offsets[i] as usize..offsets[i + 1] as usize];
        match &self.kind {
            Kind::Any => vec![true; n],
            Kind::Exact(lit) => (0..n).map(|i| at(i) == lit.as_slice()).collect(),
            Kind::Prefix(lit) => (0..n).map(|i| at(i).starts_with(lit)).collect(),
            Kind::Suffix(lit) => (0..n).map(|i| at(i).ends_with(lit)).collect(),
            Kind::Contains(f) => contains_all(f, offsets, bytes),
            Kind::Segments {
                prefix,
                middle,
                suffix,
            } => (0..n)
                .map(|i| segments_match(prefix, middle, suffix, at(i)))
                .collect(),
            Kind::General(tokens) => (0..n).map(|i| general_match(tokens, at(i))).collect(),
        }
    }

    /// Like `matches_all` but only for rows `rows` (indices into the buffer).
    pub fn matches_rows(&self, offsets: &[u32], bytes: &[u8], rows: &[u32]) -> Vec<bool> {
        rows.iter()
            .map(|&r| {
                let r = r as usize;
                self.matches(&bytes[offsets[r] as usize..offsets[r + 1] as usize])
            })
            .collect()
    }
}

/// `%lit%` over a whole vector: searches the concatenated buffer once and maps each hit to
/// its row, skipping hits that cross a row boundary.
fn contains_all(finder: &Finder, offsets: &[u32], bytes: &[u8]) -> Vec<bool> {
    let n = offsets.len().saturating_sub(1);
    let mut out = vec![false; n];
    let m = finder.needle().len();
    let mut row = 0usize;
    let mut pos = offsets.first().copied().unwrap_or(0) as usize;
    let end = offsets.last().copied().unwrap_or(0) as usize;
    while row < n && pos < end {
        let Some(p) = finder.find(&bytes[pos..end]) else {
            break;
        };
        let at = pos + p;
        while row < n && offsets[row + 1] as usize <= at {
            row += 1;
        }
        if row >= n {
            break;
        }
        let row_end = offsets[row + 1] as usize;
        if at + m <= row_end {
            out[row] = true;
            pos = row_end;
            row += 1;
        } else {
            pos = at + 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn like(p: &str, s: &str) -> bool {
        LikePattern::new(p.as_bytes())
            .unwrap()
            .matches(s.as_bytes())
    }

    #[test]
    fn finder_finds_first_occurrence() {
        let f = Finder::new(b"google".to_vec());
        assert_eq!(f.find(b"www.google.com"), Some(4));
        assert_eq!(f.find(b"googl"), None);
        assert_eq!(f.find(b"gooooglegoogle"), Some(8));
        assert_eq!(Finder::new(b"a".to_vec()).find(b"bba"), Some(2));
        assert_eq!(Finder::new(Vec::new()).find(b"x"), Some(0));
    }

    /// Every scan this CPU can run.
    fn scans() -> Vec<Scan> {
        let mut scans = vec![Scan::Scalar];
        #[cfg(target_arch = "x86_64")]
        {
            scans.push(Scan::Sse2);
            if std::arch::is_x86_feature_detected!("avx2") {
                scans.push(Scan::Avx2);
            }
        }
        scans
    }

    #[test]
    fn simd_search_matches_the_naive_search_on_random_text() {
        use veta_core::rand::Rng;
        let naive = |hay: &[u8], needle: &[u8]| -> Option<usize> {
            if needle.is_empty() {
                return Some(0);
            }
            hay.windows(needle.len()).position(|w| w == needle)
        };
        let mut rng = Rng::new(17);
        for round in 0..3000 {
            // A small alphabet makes partial matches of the first and last byte frequent.
            let alphabet = if round % 2 == 0 { 3 } else { 26 };
            let hay_len = rng.below(200) as usize;
            let hay: Vec<u8> = (0..hay_len)
                .map(|_| b'a' + rng.below(alphabet) as u8)
                .collect();
            let needle_len = rng.below(12) as usize;
            let needle: Vec<u8> = if needle_len > 0 && hay_len >= needle_len && rng.next_bool(0.5) {
                let at = rng.below((hay_len - needle_len + 1) as u64) as usize;
                hay[at..at + needle_len].to_vec()
            } else {
                (0..needle_len)
                    .map(|_| b'a' + rng.below(alphabet) as u8)
                    .collect()
            };
            let expected = naive(&hay, &needle);
            for scan in scans() {
                let finder = Finder::with_scan(needle.clone(), scan);
                assert_eq!(
                    finder.find(&hay),
                    expected,
                    "{scan:?} {:?} in {:?}",
                    String::from_utf8_lossy(&needle),
                    String::from_utf8_lossy(&hay)
                );
            }
        }
    }

    #[test]
    fn fast_path_shapes() {
        let kind = |p: &str| LikePattern::new(p.as_bytes()).unwrap().kind;
        assert!(matches!(kind("%google%"), Kind::Contains(_)));
        assert!(matches!(kind("PROMO%"), Kind::Prefix(_)));
        assert!(matches!(kind("%BRASS"), Kind::Suffix(_)));
        assert!(matches!(kind("abc"), Kind::Exact(_)));
        assert!(matches!(kind("%%"), Kind::Any));
        assert!(matches!(kind("%a%b%"), Kind::Segments { .. }));
        assert!(matches!(kind("a_c"), Kind::General(_)));
        assert!(matches!(kind(r"100\%"), Kind::Exact(_)));
    }

    #[test]
    fn wildcards_and_escapes() {
        assert!(like("%google%", "www.google.com"));
        assert!(!like("%google%", "www.Google.com"));
        assert!(like("PROMO%", "PROMO BRUSHED"));
        assert!(!like("PROMO%", "XPROMO"));
        assert!(like("%BRASS", "LARGE BRASS"));
        assert!(!like("%BRASS", "BRASS X"));
        assert!(like(
            "%Customer%Complaints%",
            "xx Customer yy Complaints zz"
        ));
        assert!(!like("%Customer%Complaints%", "Complaints Customer"));
        assert!(like("a%b%c", "abc"));
        assert!(like("a%b%c", "aXbYc"));
        assert!(!like("a%b%c", "ab"));
        assert!(!like("ab%ba", "aba"));
        assert!(like("a_c", "abc"));
        assert!(!like("a_c", "ac"));
        assert!(!like("a_c", "abbc"));
        assert!(like("_", "ñ"));
        assert!(like("%_%", "x"));
        assert!(!like("%_%", ""));
        assert!(like("_%_", "ab"));
        assert!(like("%a_c%", "zzabczz"));
        assert!(like(r"100\%", "100%"));
        assert!(!like(r"100\%", "1000"));
        assert!(like(r"a\_b", "a_b"));
        assert!(!like(r"a\_b", "axb"));
        assert!(like(r"%\\%", r"a\b"));
        assert!(like("", ""));
        assert!(!like("", "a"));
        assert!(like("%", ""));
        assert!(LikePattern::new(br"abc\").is_err());
    }

    #[test]
    fn whole_vector_matches_rows() {
        let strings = ["google", "", "xgoo", "gle", "a google b", "goog"];
        let mut offsets = vec![0u32];
        let mut bytes = Vec::new();
        for s in strings {
            bytes.extend_from_slice(s.as_bytes());
            offsets.push(bytes.len() as u32);
        }
        for p in ["%google%", "%goo%", "goo%", "%gle", "google", "%o_g%", "%"] {
            let pat = LikePattern::new(p.as_bytes()).unwrap();
            let expected: Vec<bool> = strings.iter().map(|s| like(p, s)).collect();
            assert_eq!(pat.matches_all(&offsets, &bytes), expected, "{p}");
            assert_eq!(
                pat.matches_rows(&offsets, &bytes, &[4, 0]),
                vec![expected[4], expected[0]]
            );
        }
        // "xgoo" + "gle" concatenated contain "google" across the boundary.
        let pat = LikePattern::new(b"%google%").unwrap();
        assert_eq!(
            pat.matches_all(&offsets, &bytes),
            vec![true, false, false, false, true, false]
        );
    }
}
