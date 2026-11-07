//! Own regex engine for `REGEXP_REPLACE` (contract §4, §10.3): a parser of the subset of
//! §10.3 and a Pike machine with captures that runs in time linear in the input.
//!
//! The engine works on bytes: `.` and classes match one byte, and `.` also matches `\n`
//! (ClickHouse sets `dot_nl`). `^` and `$` only match at the start and the end of the whole
//! string. Repetitions are greedy and the leftmost match wins, with the priorities of
//! re2/Perl (leftmost-first). Any syntax outside §10.3 is a compile error.
//!
//! Before the machine runs, two prefilters discard texts that cannot match: an anchored
//! pattern needs its literal prefix at the start, and every match contains the longest
//! literal run the pattern requires. The machine keeps its thread lists and capture slots in
//! a [`Cache`] reused across rows, so a search allocates nothing once the cache is warm.

use veta_core::{Result, VetaError};

use crate::like::Finder;

/// Largest bound accepted in `{n}`, `{n,}` and `{n,m}`.
const MAX_REPEAT: u32 = 1000;
/// Largest compiled program, in instructions.
const MAX_PROGRAM: usize = 100_000;

/// Set of bytes, one bit per byte value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteSet([u64; 4]);

impl ByteSet {
    const EMPTY: ByteSet = ByteSet([0; 4]);

    fn insert(&mut self, b: u8) {
        self.0[(b >> 6) as usize] |= 1 << (b & 63);
    }

    fn insert_range(&mut self, lo: u8, hi: u8) {
        for b in lo..=hi {
            self.insert(b);
        }
    }

    fn union(&mut self, other: &ByteSet) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a |= b;
        }
    }

    fn negate(&mut self) {
        for a in self.0.iter_mut() {
            *a = !*a;
        }
    }

    #[inline]
    fn contains(&self, b: u8) -> bool {
        self.0[(b >> 6) as usize] >> (b & 63) & 1 == 1
    }

    fn digit() -> ByteSet {
        let mut s = ByteSet::EMPTY;
        s.insert_range(b'0', b'9');
        s
    }

    fn word() -> ByteSet {
        let mut s = ByteSet::digit();
        s.insert_range(b'a', b'z');
        s.insert_range(b'A', b'Z');
        s.insert(b'_');
        s
    }

    fn space() -> ByteSet {
        let mut s = ByteSet::EMPTY;
        for b in [b'\t', b'\n', 0x0C, b'\r', b' '] {
            s.insert(b);
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Ast {
    Empty,
    Byte(u8),
    Class(ByteSet),
    Any,
    Start,
    End,
    /// A group; `Some(i)` is capture group `i` (1-based).
    Group(Box<Ast>, Option<usize>),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        ast: Box<Ast>,
        min: u32,
        max: Option<u32>,
    },
}

fn syntax(pattern: &[u8], what: impl std::fmt::Display) -> VetaError {
    VetaError::user(format!(
        "invalid regular expression '{}': {what}",
        String::from_utf8_lossy(pattern)
    ))
}

struct Parser<'a> {
    pattern: &'a [u8],
    pos: usize,
    groups: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, what: impl std::fmt::Display) -> VetaError {
        syntax(self.pattern, what)
    }

    fn peek(&self) -> Option<u8> {
        self.pattern.get(self.pos).copied()
    }

    fn parse_alt(&mut self) -> Result<Ast> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some(b'|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Ast::Empty)
        } else {
            Ast::Alt(branches)
        })
    }

    fn parse_concat(&mut self) -> Result<Ast> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == b'|' || c == b')' {
                break;
            }
            let atom = self.parse_atom()?;
            let atom = self.parse_quantifiers(atom)?;
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Ast::Empty,
            1 => items.pop().unwrap_or(Ast::Empty),
            _ => Ast::Concat(items),
        })
    }

    fn parse_quantifiers(&mut self, atom: Ast) -> Result<Ast> {
        let (min, max) = match self.peek() {
            Some(b'*') => {
                self.pos += 1;
                (0, None)
            }
            Some(b'+') => {
                self.pos += 1;
                (1, None)
            }
            Some(b'?') => {
                self.pos += 1;
                (0, Some(1))
            }
            Some(b'{') => self.parse_braces()?,
            _ => return Ok(atom),
        };
        if matches!(atom, Ast::Start | Ast::End) {
            return Err(self.err("repetition of an anchor"));
        }
        if matches!(self.peek(), Some(b'*' | b'+' | b'?' | b'{')) {
            return Err(self.err("nested or non-greedy repetition is not supported"));
        }
        Ok(Ast::Repeat {
            ast: Box::new(atom),
            min,
            max,
        })
    }

    fn parse_number(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut value: u32 = 0;
        while let Some(c @ b'0'..=b'9') = self.peek() {
            value = value.saturating_mul(10).saturating_add((c - b'0') as u32);
            self.pos += 1;
        }
        (self.pos > start).then_some(value)
    }

    /// `{n}`, `{n,}` or `{n,m}`; `self.pos` is at `{`.
    fn parse_braces(&mut self) -> Result<(u32, Option<u32>)> {
        self.pos += 1;
        let min = self
            .parse_number()
            .ok_or_else(|| self.err("invalid repetition"))?;
        let max = if self.peek() == Some(b',') {
            self.pos += 1;
            if self.peek() == Some(b'}') {
                None
            } else {
                Some(
                    self.parse_number()
                        .ok_or_else(|| self.err("invalid repetition"))?,
                )
            }
        } else {
            Some(min)
        };
        if self.peek() != Some(b'}') {
            return Err(self.err("invalid repetition"));
        }
        self.pos += 1;
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT) {
            return Err(self.err(format!("repetition bound above {MAX_REPEAT}")));
        }
        if max.is_some_and(|m| m < min) {
            return Err(self.err("repetition with max below min"));
        }
        Ok((min, max))
    }

    fn parse_atom(&mut self) -> Result<Ast> {
        let c = self.peek().ok_or_else(|| self.err("unexpected end"))?;
        self.pos += 1;
        Ok(match c {
            b'.' => Ast::Any,
            b'^' => Ast::Start,
            b'$' => Ast::End,
            b'[' => Ast::Class(self.parse_class()?),
            b'(' => {
                let index = if self.pattern[self.pos..].starts_with(b"?:") {
                    self.pos += 2;
                    None
                } else if self.peek() == Some(b'?') {
                    return Err(self.err("unsupported group syntax"));
                } else {
                    self.groups += 1;
                    Some(self.groups)
                };
                let inner = self.parse_alt()?;
                if self.peek() != Some(b')') {
                    return Err(self.err("missing )"));
                }
                self.pos += 1;
                Ast::Group(Box::new(inner), index)
            }
            b'*' | b'+' | b'?' | b'{' => return Err(self.err("repetition without operand")),
            b'\\' => match self.parse_escape()? {
                Escape::Byte(b) => Ast::Byte(b),
                Escape::Set(s) => Ast::Class(s),
            },
            b']' | b'}' => return Err(self.err(format!("unescaped {}", c as char))),
            _ => Ast::Byte(c),
        })
    }

    /// The escape after a `\`: `\d \w \s` or an escaped punctuation character.
    fn parse_escape(&mut self) -> Result<Escape> {
        let c = self.peek().ok_or_else(|| self.err("trailing \\"))?;
        self.pos += 1;
        match c {
            b'd' => Ok(Escape::Set(ByteSet::digit())),
            b'w' => Ok(Escape::Set(ByteSet::word())),
            b's' => Ok(Escape::Set(ByteSet::space())),
            c if c.is_ascii_punctuation() => Ok(Escape::Byte(c)),
            c => Err(self.err(format!("unsupported escape \\{}", c as char))),
        }
    }

    /// `[...]` after the `[`.
    fn parse_class(&mut self) -> Result<ByteSet> {
        let mut set = ByteSet::EMPTY;
        let negated = self.peek() == Some(b'^');
        if negated {
            self.pos += 1;
        }
        let mut first = true;
        loop {
            let c = self
                .peek()
                .ok_or_else(|| self.err("missing ] in character class"))?;
            if c == b']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            self.pos += 1;
            let lo = match c {
                b'\\' => match self.parse_escape()? {
                    Escape::Byte(b) => b,
                    Escape::Set(s) => {
                        set.union(&s);
                        continue;
                    }
                },
                b'[' if self.peek() == Some(b':') => {
                    return Err(self.err("POSIX classes are not supported"));
                }
                c => c,
            };
            // A range `lo-hi`, unless the `-` is the last character of the class.
            if self.peek() == Some(b'-')
                && self.pattern.get(self.pos + 1).is_some_and(|&n| n != b']')
            {
                self.pos += 1;
                let h = self.peek().ok_or_else(|| self.err("missing ]"))?;
                self.pos += 1;
                let hi = if h == b'\\' {
                    match self.parse_escape()? {
                        Escape::Byte(b) => b,
                        Escape::Set(_) => return Err(self.err("class escape in a range")),
                    }
                } else {
                    h
                };
                if hi < lo {
                    return Err(self.err("invalid range in character class"));
                }
                set.insert_range(lo, hi);
            } else {
                set.insert(lo);
            }
        }
        if negated {
            set.negate();
        }
        Ok(set)
    }
}

enum Escape {
    Byte(u8),
    Set(ByteSet),
}

#[derive(Debug, Clone, PartialEq)]
enum Inst {
    Byte(u8),
    Class(ByteSet),
    Any,
    /// Try `.0` first, then `.1`.
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    AssertStart,
    AssertEnd,
    Match,
}

struct Compiler<'a> {
    pattern: &'a [u8],
    prog: Vec<Inst>,
}

impl Compiler<'_> {
    fn push(&mut self, inst: Inst) -> Result<usize> {
        if self.prog.len() >= MAX_PROGRAM {
            return Err(syntax(self.pattern, "pattern is too large"));
        }
        self.prog.push(inst);
        Ok(self.prog.len() - 1)
    }

    fn patch_split_second(&mut self, at: usize, target: usize) {
        if let Inst::Split(_, second) = &mut self.prog[at] {
            *second = target;
        }
    }

    fn emit(&mut self, ast: &Ast) -> Result<()> {
        match ast {
            Ast::Empty => {}
            Ast::Byte(b) => {
                self.push(Inst::Byte(*b))?;
            }
            Ast::Class(s) => {
                self.push(Inst::Class(*s))?;
            }
            Ast::Any => {
                self.push(Inst::Any)?;
            }
            Ast::Start => {
                self.push(Inst::AssertStart)?;
            }
            Ast::End => {
                self.push(Inst::AssertEnd)?;
            }
            Ast::Group(inner, None) => self.emit(inner)?,
            Ast::Group(inner, Some(i)) => {
                self.push(Inst::Save(2 * i))?;
                self.emit(inner)?;
                self.push(Inst::Save(2 * i + 1))?;
            }
            Ast::Concat(items) => {
                for item in items {
                    self.emit(item)?;
                }
            }
            Ast::Alt(branches) => {
                let mut jumps = Vec::new();
                for (k, branch) in branches.iter().enumerate() {
                    if k + 1 < branches.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.prog[split] = Inst::Split(split + 1, 0);
                        self.emit(branch)?;
                        jumps.push(self.push(Inst::Jmp(0))?);
                        let next = self.prog.len();
                        self.patch_split_second(split, next);
                    } else {
                        self.emit(branch)?;
                    }
                }
                let end = self.prog.len();
                for j in jumps {
                    self.prog[j] = Inst::Jmp(end);
                }
            }
            Ast::Repeat { ast, min, max } => {
                for _ in 0..*min {
                    self.emit(ast)?;
                }
                match max {
                    None => {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.prog[split] = Inst::Split(split + 1, 0);
                        self.emit(ast)?;
                        self.push(Inst::Jmp(split))?;
                        let end = self.prog.len();
                        self.patch_split_second(split, end);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            let split = self.push(Inst::Split(0, 0))?;
                            self.prog[split] = Inst::Split(split + 1, 0);
                            splits.push(split);
                            self.emit(ast)?;
                        }
                        let end = self.prog.len();
                        for s in splits {
                            self.patch_split_second(s, end);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Longest run of bytes every match must contain, if any.
fn required_literal(ast: &Ast) -> Option<Vec<u8>> {
    fn runs(ast: &Ast, current: &mut Vec<u8>, best: &mut Vec<u8>) {
        let flush = |current: &mut Vec<u8>, best: &mut Vec<u8>| {
            if current.len() > best.len() {
                *best = std::mem::take(current);
            } else {
                current.clear();
            }
        };
        match ast {
            Ast::Byte(b) => current.push(*b),
            Ast::Empty | Ast::Start | Ast::End => {}
            Ast::Group(inner, _) => runs(inner, current, best),
            Ast::Concat(items) => items.iter().for_each(|i| runs(i, current, best)),
            Ast::Repeat { ast, min, max } if *min >= 1 => {
                // The first copy is required and contiguous with what precedes it; the
                // rest may vary, so the run ends after it.
                runs(ast, current, best);
                if *max != Some(1) || *min != 1 {
                    flush(current, best);
                }
            }
            _ => flush(current, best),
        }
    }
    let mut current = Vec::new();
    let mut best = Vec::new();
    runs(ast, &mut current, &mut best);
    if current.len() > best.len() {
        best = current;
    }
    (!best.is_empty()).then_some(best)
}

/// Bytes an anchored pattern must start with: the literal bytes right after the leading
/// `^`, up to the first item that is not a single required byte.
fn anchored_prefix(ast: &Ast) -> Vec<u8> {
    let mut prefix = Vec::new();
    let Ast::Concat(items) = ast else {
        return prefix;
    };
    if items.first() != Some(&Ast::Start) {
        return prefix;
    }
    for item in &items[1..] {
        match item {
            Ast::Byte(b) => prefix.push(*b),
            Ast::Repeat { ast, min, .. } if *min >= 1 && matches!(**ast, Ast::Byte(_)) => {
                if let Ast::Byte(b) = **ast {
                    prefix.push(b);
                }
                break;
            }
            _ => break,
        }
    }
    prefix
}

/// A compiled pattern.
#[derive(Debug)]
pub struct Regex {
    prog: Vec<Inst>,
    /// Capture groups, without the implicit group 0.
    groups: usize,
    /// The pattern starts with `^`: it can only match at position 0.
    anchored: bool,
    /// Bytes every match contains (prefilter).
    required: Option<Finder>,
    /// Bytes an anchored pattern starts with (prefilter); empty if none.
    prefix: Vec<u8>,
}

/// Reusable scratch space of the Pike machine; one per thread.
#[derive(Debug, Default)]
pub struct Cache {
    clist: Threads,
    nlist: Threads,
    stack: Vec<Frame>,
    scratch: Vec<Option<usize>>,
    /// Capture slots of the last successful search.
    matched: Vec<Option<usize>>,
}

impl Cache {
    /// Capture slots of the last successful [`Regex::search`] (`2i`, `2i+1` = start and end
    /// of group `i`; group 0 is the whole match).
    pub fn captures(&self) -> &[Option<usize>] {
        &self.matched
    }
}

#[derive(Debug, Default)]
struct Threads {
    dense: Vec<usize>,
    sparse: Vec<usize>,
    /// `slots` capture positions per instruction.
    caps: Vec<Option<usize>>,
}

impl Threads {
    fn reset(&mut self, prog_len: usize, slots: usize) {
        self.dense.clear();
        if self.sparse.len() != prog_len {
            self.sparse = vec![0; prog_len];
        }
        if self.caps.len() != prog_len * slots {
            self.caps = vec![None; prog_len * slots];
        }
    }

    #[inline]
    fn contains(&self, pc: usize) -> bool {
        let i = self.sparse[pc];
        i < self.dense.len() && self.dense[i] == pc
    }

    #[inline]
    fn insert(&mut self, pc: usize) {
        self.sparse[pc] = self.dense.len();
        self.dense.push(pc);
    }
}

#[derive(Debug, Clone, Copy)]
enum Frame {
    Explore(usize),
    Restore(usize, Option<usize>),
}

fn starts_anchored(ast: &Ast) -> bool {
    match ast {
        Ast::Start => true,
        Ast::Concat(items) => items.first().is_some_and(starts_anchored),
        Ast::Group(inner, _) => starts_anchored(inner),
        _ => false,
    }
}

impl Regex {
    /// Compiles `pattern`; syntax outside §10.3 is an error (exit 1).
    pub fn new(pattern: &[u8]) -> Result<Regex> {
        let mut parser = Parser {
            pattern,
            pos: 0,
            groups: 0,
        };
        let ast = parser.parse_alt()?;
        if parser.pos != pattern.len() {
            return Err(parser.err("unmatched )"));
        }
        let mut compiler = Compiler {
            pattern,
            prog: Vec::new(),
        };
        compiler.push(Inst::Save(0))?;
        compiler.emit(&ast)?;
        compiler.push(Inst::Save(1))?;
        compiler.push(Inst::Match)?;
        Ok(Regex {
            prog: compiler.prog,
            groups: parser.groups,
            anchored: starts_anchored(&ast),
            required: required_literal(&ast).map(Finder::new),
            prefix: anchored_prefix(&ast),
        })
    }

    /// Number of capture groups, without group 0.
    pub fn groups(&self) -> usize {
        self.groups
    }

    fn slots(&self) -> usize {
        2 * (self.groups + 1)
    }

    /// Whether `text` contains a match.
    pub fn is_match(&self, text: &[u8], cache: &mut Cache) -> bool {
        self.search(text, 0, cache)
    }

    /// Leftmost-first match starting the search at `start`; on a match the capture slots
    /// are in [`Cache::captures`].
    pub fn search(&self, text: &[u8], start: usize, cache: &mut Cache) -> bool {
        self.search_with(text, start, cache, true)
    }

    /// [`Regex::search`] with the prefilters on or off; both give the same answer.
    fn search_with(&self, text: &[u8], start: usize, cache: &mut Cache, prefilter: bool) -> bool {
        if self.anchored && start > 0 {
            return false;
        }
        if prefilter {
            if !text.starts_with(&self.prefix) {
                return false;
            }
            if let Some(required) = &self.required
                && required.find(&text[start..]).is_none()
            {
                return false;
            }
        }
        self.pike(text, start, cache)
    }

    fn add(
        &self,
        list: &mut Threads,
        stack: &mut Vec<Frame>,
        caps: &mut [Option<usize>],
        pc: usize,
        pos: usize,
        len: usize,
    ) {
        let slots = caps.len();
        stack.push(Frame::Explore(pc));
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Restore(slot, old) => caps[slot] = old,
                Frame::Explore(pc) => {
                    if list.contains(pc) {
                        continue;
                    }
                    list.insert(pc);
                    match &self.prog[pc] {
                        Inst::Jmp(x) => stack.push(Frame::Explore(*x)),
                        Inst::Split(x, y) => {
                            stack.push(Frame::Explore(*y));
                            stack.push(Frame::Explore(*x));
                        }
                        Inst::Save(slot) => {
                            stack.push(Frame::Restore(*slot, caps[*slot]));
                            caps[*slot] = Some(pos);
                            stack.push(Frame::Explore(pc + 1));
                        }
                        Inst::AssertStart => {
                            if pos == 0 {
                                stack.push(Frame::Explore(pc + 1));
                            }
                        }
                        Inst::AssertEnd => {
                            if pos == len {
                                stack.push(Frame::Explore(pc + 1));
                            }
                        }
                        Inst::Byte(_) | Inst::Class(_) | Inst::Any | Inst::Match => {
                            list.caps[pc * slots..(pc + 1) * slots].copy_from_slice(caps);
                        }
                    }
                }
            }
        }
    }

    /// The Pike machine; the captures of a match go to `cache.matched`.
    fn pike(&self, text: &[u8], start: usize, cache: &mut Cache) -> bool {
        let slots = self.slots();
        let n = self.prog.len();
        let Cache {
            clist,
            nlist,
            stack,
            scratch,
            matched: best,
        } = cache;
        clist.reset(n, slots);
        nlist.reset(n, slots);
        scratch.clear();
        scratch.resize(slots, None);
        best.clear();
        best.resize(slots, None);
        let mut matched = false;
        let len = text.len();
        let mut pos = start;
        loop {
            if !matched && (!self.anchored || pos == 0) {
                scratch.iter_mut().for_each(|s| *s = None);
                self.add(clist, stack, scratch, 0, pos, len);
            }
            if clist.dense.is_empty() {
                break;
            }
            let byte = text.get(pos).copied();
            for &pc in &clist.dense {
                let step = match (&self.prog[pc], byte) {
                    (Inst::Match, _) => {
                        best.copy_from_slice(&clist.caps[pc * slots..(pc + 1) * slots]);
                        matched = true;
                        // Lower-priority threads are cut.
                        break;
                    }
                    (Inst::Byte(b), Some(c)) => *b == c,
                    (Inst::Class(s), Some(c)) => s.contains(c),
                    (Inst::Any, Some(_)) => true,
                    _ => false,
                };
                if step {
                    scratch.copy_from_slice(&clist.caps[pc * slots..(pc + 1) * slots]);
                    self.add(nlist, stack, scratch, pc + 1, pos + 1, len);
                }
            }
            std::mem::swap(clist, nlist);
            nlist.dense.clear();
            if pos >= len {
                break;
            }
            pos += 1;
        }
        // Threads left in the list after the end of the text only matter if they reach
        // `Match` without consuming, which `add` already resolved at `pos == len`.
        if !matched {
            for &pc in &clist.dense {
                if self.prog[pc] == Inst::Match {
                    best.copy_from_slice(&clist.caps[pc * slots..(pc + 1) * slots]);
                    matched = true;
                    break;
                }
            }
        }
        matched
    }

    /// Replaces every non-overlapping match of `text` with `replacement`, appending the
    /// result to `out` (ClickHouse `replaceRegexpAll`: after an empty match one byte is
    /// copied, and the search stops at the end of the string).
    pub fn replace_all_into(
        &self,
        text: &[u8],
        replacement: &Replacement,
        out: &mut Vec<u8>,
        cache: &mut Cache,
    ) {
        let mut pos = 0;
        while pos < text.len() {
            if !self.search(text, pos, cache) {
                break;
            }
            let caps = cache.captures();
            let (Some(s), Some(e)) = (caps[0], caps[1]) else {
                break;
            };
            out.extend_from_slice(&text[pos..s]);
            replacement.write(text, caps, out);
            pos = e;
            if s == e {
                if pos < text.len() {
                    out.push(text[pos]);
                }
                pos += 1;
            }
        }
        if pos < text.len() {
            out.extend_from_slice(&text[pos..]);
        }
    }

    /// `replace_all_into` returning a new buffer.
    pub fn replace_all(&self, text: &[u8], replacement: &Replacement) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len());
        self.replace_all_into(text, replacement, &mut out, &mut Cache::default());
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(Vec<u8>),
    Group(usize),
}

/// Replacement string of `REGEXP_REPLACE`: `\0`–`\9` are groups, `\\` is a backslash and
/// `\` before any other byte keeps that byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pieces: Vec<Piece>,
}

impl Replacement {
    /// Parses `text` for `regex`; a group the pattern does not have is an error.
    pub fn new(text: &[u8], regex: &Regex) -> Result<Replacement> {
        let mut pieces = Vec::new();
        let mut literal = Vec::new();
        let mut i = 0;
        while i < text.len() {
            let c = text[i];
            if c == b'\\' && i + 1 < text.len() {
                let next = text[i + 1];
                if next.is_ascii_digit() {
                    let group = (next - b'0') as usize;
                    if group > regex.groups() {
                        return Err(VetaError::user(format!(
                            "replacement uses \\{group} but the pattern has {} group(s)",
                            regex.groups()
                        )));
                    }
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                    }
                    pieces.push(Piece::Group(group));
                } else {
                    literal.push(next);
                }
                i += 2;
            } else {
                literal.push(c);
                i += 1;
            }
        }
        if !literal.is_empty() {
            pieces.push(Piece::Literal(literal));
        }
        Ok(Replacement { pieces })
    }

    fn write(&self, text: &[u8], caps: &[Option<usize>], out: &mut Vec<u8>) {
        for piece in &self.pieces {
            match piece {
                Piece::Literal(bytes) => out.extend_from_slice(bytes),
                Piece::Group(g) => {
                    if let (Some(Some(s)), Some(Some(e))) = (caps.get(2 * g), caps.get(2 * g + 1)) {
                        out.extend_from_slice(&text[*s..*e]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace(pattern: &str, text: &str, rep: &str) -> String {
        let re = Regex::new(pattern.as_bytes()).unwrap();
        let rep = Replacement::new(rep.as_bytes(), &re).unwrap();
        String::from_utf8(re.replace_all(text.as_bytes(), &rep)).unwrap()
    }

    fn is_match(pattern: &str, text: &str) -> bool {
        Regex::new(pattern.as_bytes())
            .unwrap()
            .is_match(text.as_bytes(), &mut Cache::default())
    }

    const Q29: &str = r"^https?://(?:www\.)?([^/]+)/.*$";

    #[test]
    fn clickbench_q29_pattern() {
        assert_eq!(
            replace(Q29, "https://www.example.com/path?q=1", r"\1"),
            "example.com"
        );
        assert_eq!(replace(Q29, "http://example.org/", r"\1"), "example.org");
        assert_eq!(
            replace(Q29, "http://wwwx.example.org/a/b", r"\1"),
            "wwwx.example.org"
        );
        // `.` matches `\n`.
        assert_eq!(
            replace(Q29, "https://www.site.ru/line1\nline2", r"\1"),
            "site.ru"
        );
        // No match: the string is unchanged.
        assert_eq!(
            replace(Q29, "ftp://example.com/x", r"\1"),
            "ftp://example.com/x"
        );
        assert_eq!(
            replace(Q29, "https://example.com", r"\1"),
            "https://example.com"
        );
        assert_eq!(replace(Q29, "", r"\1"), "");
    }

    #[test]
    fn replaces_all_non_overlapping_matches() {
        assert_eq!(replace("a", "banana", "o"), "bonono");
        assert_eq!(replace("ana", "banana", "<\\0>"), "b<ana>na");
        assert_eq!(
            replace(r"(\d+)-(\d+)", "1-2 and 30-40", r"\2-\1"),
            "2-1 and 40-30"
        );
        assert_eq!(replace("x*", "abc", "-"), "-a-b-c");
        assert_eq!(replace("b", "abc", r"\\"), r"a\c");
    }

    #[test]
    fn alternation_classes_and_counted_repetition() {
        assert_eq!(replace("cat|dog", "cat dog cow", "pet"), "pet pet cow");
        assert!(is_match("^(?:ab|cd)+$", "abcdab"));
        assert!(!is_match("^(?:ab|cd)+$", "abce"));
        assert_eq!(replace("[^a-c]", "abcdef", ""), "abc");
        assert_eq!(replace(r"[\d.]+", "v1.25 x", "N"), "vN x");
        assert!(is_match("^a{2,3}$", "aa"));
        assert!(is_match("^a{2,3}$", "aaa"));
        assert!(!is_match("^a{2,3}$", "a"));
        assert!(!is_match("^a{2,3}$", "aaaa"));
        assert!(is_match("^a{2}$", "aa"));
        assert!(is_match("^a{2,}$", "aaaaa"));
        assert!(!is_match("^a{2,}$", "a"));
        assert_eq!(replace(r"\w+", "hi there_1 !", "w"), "w w !");
        assert_eq!(replace(r"\s", "a b\tc", ""), "abc");
        // Leftmost-first priority: the first alternative wins.
        assert_eq!(replace("(a|ab)(c|bcd)", "abcd", r"[\1,\2]"), "[a,bcd]");
        // Greedy repetition.
        assert_eq!(replace("<(.*)>", "<a><b>", r"\1"), "a><b");
    }

    #[test]
    fn anchors_only_match_string_edges() {
        assert_eq!(replace("^a", "aaa", "b"), "baa");
        assert_eq!(replace("a$", "aaa", "b"), "aab");
        assert!(!is_match("^b", "ab"));
        assert!(!is_match("a$", "a\n"));
    }

    #[test]
    fn invalid_patterns_are_errors() {
        for p in [
            "(", ")", "[a", "a**", "a*?", "*a", "a{3,1}", "(?i)a", r"\p", r"\b", "a{x}", "]",
            "[z-a]", "a{2000}",
        ] {
            let err = Regex::new(p.as_bytes()).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{p}");
        }
        let re = Regex::new(b"(a)").unwrap();
        assert!(Replacement::new(br"\2", &re).is_err());
    }

    #[test]
    fn prefilters_never_change_the_answer() {
        use veta_core::rand::Rng;
        let patterns = [
            Q29,
            r"^ab+c",
            r"^a",
            r"x(abc)+y",
            r"(\d+)-(\d+)",
            r"cat|dog",
            r"^(?:ab|cd)+$",
            r"a.c$",
            r"[^a-c]b",
            r"bb*a",
        ];
        let mut rng = Rng::new(29);
        let alphabet = b"abcdxy-1/:htps.w";
        let mut texts: Vec<Vec<u8>> = vec![
            b"https://www.example.com/path".to_vec(),
            b"http://example.org/".to_vec(),
            b"ftp://x/".to_vec(),
            Vec::new(),
        ];
        for _ in 0..400 {
            let len = rng.below(24) as usize;
            texts.push(
                (0..len)
                    .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
                    .collect(),
            );
        }
        let mut with = Cache::default();
        let mut without = Cache::default();
        for pattern in patterns {
            let re = Regex::new(pattern.as_bytes()).unwrap();
            for text in &texts {
                for start in 0..=text.len().min(3) {
                    let a = re.search_with(text, start, &mut with, true);
                    let b = re.search_with(text, start, &mut without, false);
                    assert_eq!(a, b, "{pattern} on {:?} from {start}", text);
                    if a {
                        assert_eq!(with.captures(), without.captures(), "{pattern}");
                    }
                }
            }
        }
    }

    #[test]
    fn anchored_prefix_of_patterns() {
        assert_eq!(Regex::new(Q29.as_bytes()).unwrap().prefix, b"http".to_vec());
        assert_eq!(Regex::new(b"^ab+c").unwrap().prefix, b"ab".to_vec());
        assert!(Regex::new(b"ab").unwrap().prefix.is_empty());
        assert!(Regex::new(b"^(a)").unwrap().prefix.is_empty());
    }

    #[test]
    fn required_literal_prefilter() {
        let re = Regex::new(Q29.as_bytes()).unwrap();
        // `http` (4 bytes) is longer than `://` (3 bytes).
        assert_eq!(
            re.required.as_ref().map(|f| f.needle().to_vec()),
            Some(b"http".to_vec())
        );
        assert!(re.anchored);
        let re = Regex::new(b"a|b").unwrap();
        assert!(re.required.is_none());
        let re = Regex::new(b"x(abc)+y").unwrap();
        assert_eq!(
            re.required.as_ref().map(|f| f.needle().to_vec()),
            Some(b"xabc".to_vec())
        );
    }
}
