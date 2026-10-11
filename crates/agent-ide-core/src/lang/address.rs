//! Symbol addresses: one canonical text per code location, `src/a.rs#Type/method:7`.
//!
//! Contract: `docs/contracts/symbol-address-v1.md` (tracked boundary `symbol-address-v1`, r2).
//! [`Address`] is the pure text form (parse and print, no outline needed). [`AddressMap`] binds
//! addresses to the lines of one file from its [`Outline`] and source text. The grammar and its
//! escapes are implemented; the map is a published stub (`todo!()` bodies) that later tasks fill
//! in without changing a signature.

use std::{fmt, ops::Range, path::PathBuf};

use super::{
    InsertSite, InsertWhere, LangError, LanguageSupport, LineRange, Outline, Symbol, SymbolKind,
    SymbolPath,
    regions::{RawKind, RawRegion, RegionRules, scan_gap},
};

/// Why address text was rejected by [`Address::parse`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressError {
    /// Empty text, or a name lookup / file part with nothing in it.
    Empty,
    /// A `/` separated two empty path segments.
    EmptySegment,
    /// The file part is absolute; files are relative to the project root.
    AbsoluteFile,
    /// The file part contains `..`.
    ParentSegment,
    /// A window or location number is zero, malformed, or its end precedes its start.
    BadWindow,
}

impl fmt::Display for AddressError {
    /// One-line reason, suitable for a refusal head.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty address",
            Self::EmptySegment => "empty address segment",
            Self::AbsoluteFile => "address file must be relative to the project root",
            Self::ParentSegment => "address file must not contain `..`",
            Self::BadWindow => "bad line window: use :N, :N-M or :N- with 1 <= N <= M",
        })
    }
}

impl std::error::Error for AddressError {}

/// A line window inside one unit, relative to the unit's first line (1 is that line).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Window {
    /// First line, 1-based.
    pub first: u32,
    /// Last line inclusive; `None` for `N-`, meaning the unit's last line.
    pub last: Option<u32>,
}

/// One path segment: a unit name plus its duplicate ordinal.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Segment {
    /// Decoded unit name (`@imports` for a region).
    pub name: String,
    /// Duplicate ordinal; 1 for the first unit with this raw path, printed as `~k` when >= 2.
    pub ordinal: u32,
}

/// A parsed address; its `Display` is the canonical text and parsing it back is the identity.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Address {
    file: Option<PathBuf>,
    segments: Vec<Segment>,
    window: Option<Window>,
}

/// What [`Address::parse`] understood.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Parsed {
    /// `file#path[:window]` or a file-less name lookup.
    Address(Address),
    /// `file:line[:column]`, what grep and compilers print; resolve it with
    /// [`AddressMap::at_line`]. The column is accepted and ignored.
    Location {
        /// Relative or absolute file as written.
        file: PathBuf,
        /// 1-based line.
        line: u32,
        /// 1-based column when present.
        column: Option<u32>,
    },
}

/// Applies the contract's escapes to one name: `/` always, `%` before `25|2F|7E|3A`, a trailing
/// `~digits` and a trailing `:digits[-digits]`.
fn encode_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let rest: String = chars[i + 1..].iter().collect();
        match c {
            '/' => out.push_str("%2F"),
            '%' if matches!(
                rest.get(..2).map(str::to_ascii_uppercase).as_deref(),
                Some("25" | "2F" | "7E" | "3A")
            ) =>
            {
                out.push_str("%25")
            }
            '~' if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) => {
                out.push_str("%7E")
            }
            ':' if is_window_text(&rest) => out.push_str("%3A"),
            _ => out.push(c),
        }
    }
    out
}

/// Maps the four escapes anywhere in `text`; every other `%` stays literal.
fn decode_name(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let decoded = tail
            .get(1..3)
            .and_then(|h| match h.to_ascii_uppercase().as_str() {
                "25" => Some('%'),
                "2F" => Some('/'),
                "7E" => Some('~'),
                "3A" => Some(':'),
                _ => None,
            });
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[3..];
            }
            None => {
                out.push('%');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether `text` is exactly `digits` or `digits-` or `digits-digits`.
fn is_window_text(text: &str) -> bool {
    parse_window(text).is_ok()
}

/// Parses `N`, `N-M` or `N-` (no colon).
fn parse_window(text: &str) -> Result<Window, AddressError> {
    let num = |s: &str| -> Result<u32, AddressError> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AddressError::BadWindow);
        }
        match s.parse::<u32>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => Err(AddressError::BadWindow),
        }
    };
    match text.split_once('-') {
        None => {
            let n = num(text)?;
            Ok(Window {
                first: n,
                last: Some(n),
            })
        }
        Some((a, "")) => Ok(Window {
            first: num(a)?,
            last: None,
        }),
        Some((a, b)) => {
            let (first, last) = (num(a)?, num(b)?);
            if last < first {
                return Err(AddressError::BadWindow);
            }
            Ok(Window {
                first,
                last: Some(last),
            })
        }
    }
}

impl Address {
    /// Parses any form of contract section 2. Text without `#` that ends in `:N` or `:N:C` and
    /// whose prefix contains `.` or `/` is a location; any other text without `#` is a file-less
    /// name lookup. Errors: see [`AddressError`].
    pub fn parse(text: &str) -> Result<Parsed, AddressError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(AddressError::Empty);
        }
        let (file, rest) = match text.split_once('#') {
            Some((file, rest)) => (Some(file), rest),
            None => {
                if let Some(loc) = parse_location(text) {
                    return loc;
                }
                (None, text)
            }
        };
        let file = match file {
            Some("") | None => None,
            Some(f) => {
                let p = PathBuf::from(f);
                if p.is_absolute() {
                    return Err(AddressError::AbsoluteFile);
                }
                if p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(AddressError::ParentSegment);
                }
                Some(p)
            }
        };
        // A trailing `:window` is the last colon followed only by window text.
        let (path, window) = match rest.rfind(':') {
            Some(i) if is_window_text(&rest[i + 1..]) => {
                (&rest[..i], Some(parse_window(&rest[i + 1..])?))
            }
            _ => (rest, None),
        };
        let segments = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/')
                .map(parse_segment)
                .collect::<Result<Vec<_>, _>>()?
        };
        if file.is_none() && segments.is_empty() {
            return Err(AddressError::Empty);
        }
        Ok(Parsed::Address(Address {
            file,
            segments,
            window,
        }))
    }

    /// Builds an address from parts already known to be valid.
    pub fn new(file: Option<PathBuf>, segments: Vec<Segment>, window: Option<Window>) -> Self {
        Self {
            file,
            segments,
            window,
        }
    }

    /// Relative file, absent for a name lookup.
    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    /// Decoded segments, outermost first; empty when the address names a whole file.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The typed line window, if the text carried one.
    pub fn window(&self) -> Option<Window> {
        self.window
    }

    /// The same address without its window.
    pub fn without_window(&self) -> Self {
        Self {
            window: None,
            ..self.clone()
        }
    }

    /// The same address with `window`.
    pub fn with_window(&self, window: Window) -> Self {
        Self {
            window: Some(window),
            ..self.clone()
        }
    }

    /// The unit part as a [`SymbolPath`] (ordinals printed as `~k`), for the legacy lookups.
    pub fn symbol_path(&self) -> SymbolPath {
        SymbolPath::new(
            self.file.clone(),
            self.segments.iter().map(Segment::to_string).collect(),
        )
    }
}

/// Splits `name~k` (k >= 2, digits only) and decodes the name.
fn parse_segment(raw: &str) -> Result<Segment, AddressError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AddressError::EmptySegment);
    }
    if let Some((name, k)) = raw.rsplit_once('~')
        && let Ok(ordinal) = k.parse::<u32>()
        && ordinal >= 2
        && !name.is_empty()
        && k.bytes().all(|b| b.is_ascii_digit())
    {
        return Ok(Segment {
            name: decode_name(name),
            ordinal,
        });
    }
    Ok(Segment {
        name: decode_name(raw),
        ordinal: 1,
    })
}

/// `file:line[:col]` when `text` has that shape and its prefix looks like a path.
fn parse_location(text: &str) -> Option<Result<Parsed, AddressError>> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (head, last) = text.rsplit_once(':')?;
    if !digits(last) {
        return None;
    }
    let (file, line, column) = match head.rsplit_once(':') {
        Some((f, l)) if digits(l) => (f, l, Some(last)),
        _ => (head, last, None),
    };
    if !(file.contains('.') || file.contains('/')) {
        return None;
    }
    let num = |s: &str| s.parse::<u32>().ok().filter(|n| *n >= 1);
    Some(match (num(line), column.map(num)) {
        (Some(line), None) => Ok(Parsed::Location {
            file: file.into(),
            line,
            column: None,
        }),
        (Some(line), Some(Some(c))) => Ok(Parsed::Location {
            file: file.into(),
            line,
            column: Some(c),
        }),
        _ => Err(AddressError::BadWindow),
    })
}

impl fmt::Display for Segment {
    /// `name` or `name~k`, escaped per the contract.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&encode_name(&self.name))?;
        if self.ordinal >= 2 {
            write!(f, "~{}", self.ordinal)?;
        }
        Ok(())
    }
}

impl fmt::Display for Window {
    /// `N`, `N-M` or `N-`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.last {
            Some(l) if l == self.first => write!(f, "{}", self.first),
            Some(l) => write!(f, "{}-{}", self.first, l),
            None => write!(f, "{}-", self.first),
        }
    }
}

impl fmt::Display for Address {
    /// Canonical text `file#a/b~2:3-5` (file-less: `a/b`). Parsing it back yields `self`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}#", file.display())?;
        }
        for (i, s) in self.segments.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            write!(f, "{s}")?;
        }
        if let Some(w) = self.window {
            write!(f, ":{w}")?;
        }
        Ok(())
    }
}

impl Window {
    /// Maps this window onto a unit `span` (absolute lines) and clamps its end to the span.
    /// Returns the absolute lines and whether the end was clamped. Errors:
    /// [`ResolveError::LineOutOfRange`] when `first` lies beyond the span.
    pub fn to_lines(self, span: LineRange) -> Result<(LineRange, bool), ResolveError> {
        let length = span.len();
        if self.first > length {
            return Err(ResolveError::LineOutOfRange { length });
        }
        let last = self.last.unwrap_or(length);
        let clamped = last > length;
        let last = last.min(length);
        Ok((
            LineRange::new(span.start + self.first - 1, span.start + last - 1),
            clamped,
        ))
    }
}

/// Region vocabulary (contract section 5).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RegionKind {
    /// `@header`: leading comments, attributes and header statements.
    Header,
    /// `@imports`: a run of import statements.
    Imports,
    /// `@main`: Python `if __name__ == "__main__":` block.
    Main,
    /// `@code/<leader>`: a top-level statement block that is neither symbol nor import.
    Code,
    /// `@after/<prev>`: trivia between two units.
    After,
}

/// What a [`Unit`] is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnitKind {
    /// A symbol of the language outline.
    Symbol(SymbolKind),
    /// A file-level region.
    Region(RegionKind),
}

/// One addressable unit of a file: a symbol or a region.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unit {
    /// Canonical address, never with a window.
    pub address: Address,
    /// Symbol or region.
    pub kind: UnitKind,
    /// Absolute 1-based inclusive lines, doc comments and attributes included.
    pub span: LineRange,
    /// 0 for file level.
    pub depth: u8,
    /// Pre-order index into the outline's symbols; `None` for regions.
    pub symbol: Option<usize>,
    /// One-line signature; empty for regions.
    pub signature: String,
    /// First doc paragraph.
    pub doc: Option<String>,
    /// A region whose lines are all blank (such rows are not printed in outlines).
    pub blank: bool,
}

/// An address resolved to lines and bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolved {
    /// Index into [`AddressMap::units`].
    pub unit: usize,
    /// The unit's span.
    pub span: LineRange,
    /// The window in absolute lines, when the address had one.
    pub window: Option<LineRange>,
    /// Byte range of the window, or of the span without one; line terminators included.
    pub bytes: Range<usize>,
    /// The window's end was beyond the unit and was clamped.
    pub clamped: bool,
}

/// A file line mapped to its position address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Located {
    /// Index of the innermost unit.
    pub unit: usize,
    /// `file#unit:N`, window `{N, Some(N)}`.
    pub address: Address,
}

/// Why [`AddressMap::resolve`] failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveError {
    /// No unit has this address; `near` lists up to five unit indices.
    UnknownUnit {
        /// Candidate units.
        near: Vec<usize>,
    },
    /// The window starts beyond the unit, which has `length` lines.
    LineOutOfRange {
        /// Lines in the unit.
        length: u32,
    },
    /// The address names no file and the map cannot choose one.
    NoFile,
}

/// Every unit of one file and the total line partition (contract sections 3 to 5).
///
/// Immutable; a pure function of the outline and the exact source text it was built from.
#[derive(Clone, Debug)]
pub struct AddressMap {
    file: PathBuf,
    units: Vec<Unit>,
    line_count: u32,
    /// The outline the map was built from, kept for symbol anchors of `insert_site`.
    outline: Option<Outline>,
    /// Symbol paths by pre-order outline index.
    symbol_paths: Vec<SymbolPath>,
    /// Innermost unit index of each line (`owner[line - 1]`).
    owner: Vec<usize>,
    /// Byte offset of the start of each line, plus the source length as the last entry.
    line_starts: Vec<usize>,
    /// Canonical segments to unit index.
    index: std::collections::HashMap<Vec<Segment>, usize>,
}

/// Disambiguation class of a symbol inside a group of equal raw paths: declarations, impl blocks,
/// then fields (contract section 4).
fn rank(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Impl => 1,
        SymbolKind::Field => 2,
        _ => 0,
    }
}

impl AddressMap {
    /// Builds the map: outline symbols plus regions so every line `1..=line_count` has exactly one
    /// innermost unit. Reads the language's region rules from `outline.language`. Never fails.
    pub fn build(outline: &Outline, source: &str) -> AddressMap {
        let rules = outline.language.support().region_rules();
        let mut map = Self::build_parts(&outline.file, &outline.symbols, source, rules);
        map.outline = Some(outline.clone());
        map
    }

    /// [`Self::build`] on loose parts: the file, its top-level symbols and explicit region rules.
    /// `insert_site` on a symbol anchor needs the outline and answers `UnknownSymbol` for a map
    /// built this way.
    pub fn build_parts(
        file: &std::path::Path,
        symbols: &[Symbol],
        source: &str,
        rules: &RegionRules,
    ) -> AddressMap {
        // Pre-order flattening with ancestry names and depth.
        struct Flat<'a> {
            symbol: &'a Symbol,
            names: Vec<String>,
            depth: u8,
        }
        fn flatten<'a>(level: &'a [Symbol], names: &[String], depth: u8, out: &mut Vec<Flat<'a>>) {
            for symbol in level {
                let mut own = names.to_vec();
                own.push(symbol.name.clone());
                out.push(Flat {
                    symbol,
                    names: own.clone(),
                    depth,
                });
                flatten(&symbol.children, &own, depth.saturating_add(1), out);
            }
        }
        let mut flat = Vec::new();
        flatten(symbols, &[], 0, &mut flat);
        // Ordinals: position inside the group of equal raw paths by (rank, file order).
        let mut ordinals = vec![1u32; flat.len()];
        let mut groups: std::collections::HashMap<&[String], Vec<usize>> = Default::default();
        for (i, f) in flat.iter().enumerate() {
            groups.entry(f.names.as_slice()).or_default().push(i);
        }
        for members in groups.values() {
            let mut sorted = members.clone();
            sorted.sort_by_key(|&i| (rank(flat[i].symbol.kind), i));
            for (k, &i) in sorted.iter().enumerate() {
                ordinals[i] = k as u32 + 1;
            }
        }
        let file = file.to_path_buf();
        let seg_of = |i: usize| {
            let f = &flat[i];
            let mut segments: Vec<Segment> = f.names[..f.names.len() - 1]
                .iter()
                .map(|name| Segment {
                    name: name.clone(),
                    ordinal: 1,
                })
                .collect();
            segments.push(Segment {
                name: f.names[f.names.len() - 1].clone(),
                ordinal: ordinals[i],
            });
            segments
        };
        // Top-level items in file order: symbol subtrees and regions.
        let line_total = super::line_count(source);
        let lines: Vec<&str> = source.lines().collect();
        let mut tops: Vec<usize> = (0..flat.len()).filter(|&i| flat[i].depth == 0).collect();
        tops.sort_by_key(|&i| (flat[i].symbol.range.start, i));
        enum Item {
            Symbol(usize),
            Region(RawRegion),
        }
        let mut items: Vec<(u32, Item)> = Vec::new();
        let mut cursor = 1u32;
        for &i in &tops {
            let range = flat[i].symbol.range;
            let (start, end) = (range.start.max(1), range.end.min(line_total.max(1)));
            if start > cursor {
                for r in scan_gap(&lines, cursor, start - 1, cursor == 1, rules) {
                    items.push((r.span.start, Item::Region(r)));
                }
            }
            items.push((start, Item::Symbol(i)));
            cursor = cursor.max(end + 1);
        }
        if cursor <= line_total {
            for r in scan_gap(&lines, cursor, line_total, cursor == 1, rules) {
                items.push((r.span.start, Item::Region(r)));
            }
        }
        // Units in pre-order, naming regions as they appear.
        let mut units: Vec<Unit> = Vec::new();
        let mut counters: std::collections::HashMap<Vec<String>, u32> = Default::default();
        let mut prev_text: Option<String> = None;
        let paths: Vec<SymbolPath> = flat
            .iter()
            .map(|f| SymbolPath::new(Some(file.clone()), f.names.clone()))
            .collect();
        for (_, item) in items {
            match item {
                Item::Region(r) => {
                    let (names, kind): (Vec<String>, RegionKind) = match r.kind {
                        RawKind::Header => (vec!["@header".into()], RegionKind::Header),
                        RawKind::Imports => (vec!["@imports".into()], RegionKind::Imports),
                        RawKind::Main => (vec!["@main".into()], RegionKind::Main),
                        RawKind::Code => (vec!["@code".into(), r.leader.clone()], RegionKind::Code),
                        RawKind::After => (
                            vec!["@after".into(), prev_text.clone().unwrap_or_default()],
                            RegionKind::After,
                        ),
                    };
                    let count = counters.entry(names.clone()).or_insert(0);
                    *count += 1;
                    let ordinal = *count;
                    let mut segments: Vec<Segment> = names[..names.len() - 1]
                        .iter()
                        .map(|name| Segment {
                            name: name.clone(),
                            ordinal: 1,
                        })
                        .collect();
                    segments.push(Segment {
                        name: names[names.len() - 1].clone(),
                        ordinal,
                    });
                    let address = Address::new(Some(file.clone()), segments, None);
                    if r.kind != RawKind::After {
                        let parts: Vec<String> =
                            address.segments().iter().map(Segment::to_string).collect();
                        prev_text = Some(parts.join("/"));
                    }
                    units.push(Unit {
                        address,
                        kind: UnitKind::Region(kind),
                        span: r.span,
                        depth: 0,
                        symbol: None,
                        signature: String::new(),
                        doc: None,
                        blank: (r.span.start..=r.span.end).all(|line| {
                            lines
                                .get(line as usize - 1)
                                .is_none_or(|text| text.trim().is_empty())
                        }),
                    });
                }
                Item::Symbol(top) => {
                    // The subtree of `top` is the following flat entries with greater depth.
                    let mut i = top;
                    loop {
                        let f = &flat[i];
                        let segments = seg_of(i);
                        units.push(Unit {
                            address: Address::new(Some(file.clone()), segments, None),
                            kind: UnitKind::Symbol(f.symbol.kind),
                            span: f.symbol.range,
                            depth: f.depth,
                            symbol: Some(i),
                            signature: f.symbol.signature.clone(),
                            doc: f.symbol.doc.clone(),
                            blank: false,
                        });
                        i += 1;
                        if i >= flat.len() || flat[i].depth == 0 {
                            break;
                        }
                    }
                    let last = Segment::to_string(&seg_of(top).pop().unwrap());
                    prev_text = Some(last);
                }
            }
        }
        // Paint lines: deeper wins, then later start, then shorter span.
        let mut order: Vec<usize> = (0..units.len()).collect();
        order.sort_by_key(|&u| {
            let s = units[u].span;
            (units[u].depth, s.start, std::cmp::Reverse(s.len()))
        });
        let mut owner = vec![usize::MAX; line_total as usize];
        for u in order {
            let s = units[u].span;
            for line in s.start..=s.end.min(line_total) {
                owner[line as usize - 1] = u;
            }
        }
        let mut line_starts = vec![0usize];
        for (at, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(at + 1);
            }
        }
        if *line_starts.last().unwrap() != source.len() || line_starts.len() <= line_total as usize
        {
            line_starts.push(source.len());
        }
        let mut index = std::collections::HashMap::new();
        for (u, unit) in units.iter().enumerate() {
            index.entry(unit.address.segments().to_vec()).or_insert(u);
        }
        AddressMap {
            file,
            units,
            line_count: line_total,
            outline: None,
            symbol_paths: paths,
            owner,
            line_starts,
            index,
        }
    }

    /// Checks the contract invariants of section 3 and 4 against `source`: every line has an
    /// address whose resolution contains it, all canonical addresses differ, and each unit
    /// resolves to itself. Returns one message per violation (empty when sound). Meant for the
    /// per-language conformance tests.
    pub fn violations(&self, source: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (i, unit) in self.units.iter().enumerate() {
            if !seen.insert(unit.address.to_string()) {
                out.push(format!("duplicate address {}", unit.address));
            }
            match self.resolve(&unit.address) {
                Ok(r) if r.unit == i => {}
                other => out.push(format!("{} resolves to {other:?}", unit.address)),
            }
        }
        for line in 1..=super::line_count(source) {
            match self.at_line(line) {
                None => out.push(format!("line {line} has no address")),
                Some(located) => match self.resolve(&located.address) {
                    Ok(r) if r.window.is_some_and(|w| w.start == line) => {}
                    other => out.push(format!("line {line} -> {} -> {other:?}", located.address)),
                },
            }
        }
        out
    }

    /// Units in file order, pre-order.
    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    /// File this map describes.
    pub fn file(&self) -> &std::path::Path {
        &self.file
    }

    /// The outline symbol behind symbol unit `unit`; `None` for a region or a map built without
    /// an outline ([`Self::build_parts`]).
    pub fn symbol_of(&self, unit: usize) -> Option<&Symbol> {
        let index = self.units.get(unit)?.symbol?;
        let mut order = 0usize;
        let mut found = None;
        for top in &self.outline.as_ref()?.symbols {
            top.walk(&mut |symbol| {
                if order == index {
                    found = Some(symbol);
                }
                order += 1;
            });
        }
        found
    }

    /// The canonical address of outline symbol `symbol` (matched by path, kind and lines), as the
    /// text results print it; `None` for a map built without an outline or a foreign symbol.
    pub fn address_of(&self, symbol: &Symbol) -> Option<String> {
        let mut order = 0usize;
        let mut index = None;
        for top in &self.outline.as_ref()?.symbols {
            top.walk(&mut |candidate| {
                if index.is_none() && candidate == symbol {
                    index = Some(order);
                }
                order += 1;
            });
        }
        let index = index?;
        self.units
            .iter()
            .find(|unit| unit.symbol == Some(index))
            .map(|unit| unit.address.to_string())
    }

    /// The unit named by `path` (symbols and regions; a region is the one-segment `@imports`).
    pub fn unit(&self, path: &SymbolPath) -> Option<&Unit> {
        let segments: Vec<Segment> = path
            .segments()
            .iter()
            .map(|s| parse_segment(s))
            .collect::<Result<_, _>>()
            .ok()?;
        self.index.get(&segments).map(|&u| &self.units[u])
    }

    /// Address (with window) to absolute lines and byte range. A file part, when present, must be
    /// this map's file; an address without segments names no unit.
    pub fn resolve(&self, address: &Address) -> Result<Resolved, ResolveError> {
        if address.file().is_some_and(|f| f != self.file) {
            return Err(ResolveError::UnknownUnit { near: Vec::new() });
        }
        let Some(&unit) = self.index.get(address.segments()) else {
            return Err(ResolveError::UnknownUnit {
                near: self.near(address),
            });
        };
        let span = self.units[unit].span;
        let (window, clamped) = match address.window() {
            Some(w) => {
                let (lines, clamped) = w.to_lines(span)?;
                (Some(lines), clamped)
            }
            None => (None, false),
        };
        let lines = window.unwrap_or(span);
        let from = self.line_starts[lines.start as usize - 1];
        let to = self.line_starts[(lines.end as usize).min(self.line_starts.len() - 1)];
        Ok(Resolved {
            unit,
            span,
            window,
            bytes: from..to,
            clamped,
        })
    }

    /// The position address of `line`; `None` only beyond the last line.
    pub fn at_line(&self, line: u32) -> Option<Located> {
        if line == 0 || line > self.line_count {
            return None;
        }
        let unit = *self.owner.get(line as usize - 1)?;
        let n = line - self.units[unit].span.start + 1;
        Some(Located {
            unit,
            address: self.units[unit].address.with_window(Window {
                first: n,
                last: Some(n),
            }),
        })
    }

    /// `file#unit:N` for `line`, without the absolute line.
    pub fn address_at(&self, file: &std::path::Path, line: u32) -> Option<String> {
        let located = self.at_line(line)?;
        Some(
            Address::new(
                Some(file.to_path_buf()),
                located.address.segments().to_vec(),
                located.address.window(),
            )
            .to_string(),
        )
    }

    /// Units whose last segment equals `name`, file order, with the declaration-and-impl collapse.
    pub fn named(&self, name: &str) -> Vec<usize> {
        let matches: Vec<usize> = (0..self.units.len())
            .filter(|&u| {
                matches!(self.units[u].kind, UnitKind::Symbol(_))
                    && self.units[u]
                        .address
                        .segments()
                        .last()
                        .is_some_and(|s| s.name == name)
            })
            .collect();
        let raw = |u: usize| -> Vec<&str> {
            self.units[u]
                .address
                .segments()
                .iter()
                .map(|s| s.name.as_str())
                .collect()
        };
        let is_impl = |u: usize| self.units[u].kind == UnitKind::Symbol(SymbolKind::Impl);
        matches
            .iter()
            .copied()
            .filter(|&u| !is_impl(u) || !matches.iter().any(|&v| !is_impl(v) && raw(v) == raw(u)))
            .collect()
    }

    /// At most five units near an unresolvable `address`: same last name elsewhere first, then
    /// siblings under the same owner.
    pub fn near(&self, address: &Address) -> Vec<usize> {
        let Some(last) = address.segments().last() else {
            return Vec::new();
        };
        let owner: Vec<&str> = address.segments()[..address.segments().len() - 1]
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let symbols =
            (0..self.units.len()).filter(|&u| matches!(self.units[u].kind, UnitKind::Symbol(_)));
        let mut out: Vec<usize> = symbols
            .clone()
            .filter(|&u| {
                self.units[u]
                    .address
                    .segments()
                    .last()
                    .is_some_and(|s| s.name == last.name)
            })
            .collect();
        if !owner.is_empty() {
            for u in symbols {
                let segs = self.units[u].address.segments();
                let same_owner = segs.len() == owner.len() + 1
                    && segs[..owner.len()]
                        .iter()
                        .map(|s| s.name.as_str())
                        .eq(owner.iter().copied());
                if same_owner && !out.contains(&u) {
                    out.push(u);
                }
            }
        }
        out.truncate(5);
        out
    }

    /// Line where a new import belongs: after the last import, else after `@header`, else 1.
    pub fn imports_insertion_line(&self) -> u32 {
        let end_of = |kind: RegionKind| {
            self.units
                .iter()
                .filter(|u| u.kind == UnitKind::Region(kind))
                .map(|u| u.span.end)
                .max()
        };
        end_of(RegionKind::Imports)
            .or_else(|| end_of(RegionKind::Header))
            .map_or(1, |end| end + 1)
    }

    /// Insertion point relative to `anchor`. A region anchor is placed here (`After` at its last
    /// line + 1, `Before` at its first line); a symbol anchor delegates to
    /// [`LanguageSupport::insert_site`]. A missing `@imports` anchor falls back to
    /// [`Self::imports_insertion_line`]. Errors: `UnknownSymbol` for an unresolvable anchor,
    /// `NotAContainer` for `First`/`Last` on a region.
    pub fn insert_site(
        &self,
        source: &str,
        lang: &dyn LanguageSupport,
        anchor: &Address,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        let site = |line, blank_before, blank_after| InsertSite {
            line,
            indent: String::new(),
            blank_before,
            blank_after,
        };
        let unit = self.index.get(anchor.segments()).copied();
        let Some(unit) = unit else {
            let is_imports =
                anchor.segments().len() == 1 && anchor.segments()[0].name == "@imports";
            return if is_imports && matches!(where_, InsertWhere::After | InsertWhere::Last) {
                let line = self.imports_insertion_line();
                Ok(site(line, u8::from(line > 1), 1))
            } else {
                Err(LangError::UnknownSymbol(anchor.symbol_path()))
            };
        };
        let u = &self.units[unit];
        match (u.kind, where_) {
            (UnitKind::Region(_), InsertWhere::First | InsertWhere::Last) => {
                Err(LangError::NotAContainer(anchor.symbol_path()))
            }
            (UnitKind::Region(kind), InsertWhere::After) => {
                let adjacent = kind == RegionKind::Imports;
                Ok(site(u.span.end + 1, u8::from(!adjacent), 0))
            }
            (UnitKind::Region(_), InsertWhere::Before) => Ok(site(u.span.start, 0, 1)),
            (UnitKind::Symbol(_), _) => {
                let (Some(outline), Some(i)) = (&self.outline, u.symbol) else {
                    return Err(LangError::UnknownSymbol(anchor.symbol_path()));
                };
                lang.insert_site(source, outline, &self.symbol_paths[i], where_)
            }
        }
    }
}

/// Conformance sweep for a language's tests: builds the [`AddressMap`] of every file under `root`
/// whose extension the language owns (skipping `target`, `node_modules`, `.git` and virtual
/// environments), using the language's source outline, and returns the number of files checked
/// plus every [`AddressMap::violations`] message prefixed with the file.
pub fn conformance_sweep(
    root: &std::path::Path,
    language: super::Language,
) -> (usize, Vec<String>) {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !matches!(
                    &*name,
                    "target" | "node_modules" | ".git" | ".venv" | "venv"
                ) {
                    walk(&path, out);
                }
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    files.sort();
    let (mut checked, mut failures) = (0, Vec::new());
    for path in files {
        let owned = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| language.descriptor().extensions.contains(&e));
        let Some(source) = owned.then(|| std::fs::read_to_string(&path).ok()).flatten() else {
            continue;
        };
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let Some(outline) = language.support().outline_from_source(rel, &source) else {
            continue;
        };
        checked += 1;
        let map = AddressMap::build(&outline, &source);
        for message in map.violations(&source) {
            failures.push(format!("{}: {message}", rel.display()));
        }
    }
    (checked, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(first: u32, last: Option<u32>) -> Window {
        Window { first, last }
    }

    fn addr(text: &str) -> Address {
        match Address::parse(text).unwrap() {
            Parsed::Address(a) => a,
            other => panic!("not an address: {other:?}"),
        }
    }

    #[test]
    fn address_grammar_round_trips() {
        for text in [
            "src/a.rs#Store/get",
            "src/a.rs#Store/get:7",
            "src/a.rs#Store/get:3-5",
            "src/a.rs#Store/get:3-",
            "src/a.rs#@imports",
            "src/a.rs#@imports~2",
            "src/a.rs#Store~2/new~3:2",
            "src/a.rs#",
            "src/a.rs#impl Display for Foo/fmt",
            "a.rs#x%2Fy/z",
            "a.rs#100%2525",
            "a.rs#a%7E2",
            "a.rs#a%3A3:5",
            "Store/get",
        ] {
            assert_eq!(addr(text).to_string(), text, "{text}");
        }
        let seg = |name: &str, ordinal| Segment {
            name: name.into(),
            ordinal,
        };
        let a = Address::new(
            Some("a.rs".into()),
            vec![
                seg("a/b", 1),
                seg("x~2", 2),
                seg("p:3", 1),
                seg("50%25", 1),
                seg("plain%zz", 1),
            ],
            Some(win(2, None)),
        );
        assert_eq!(addr(&a.to_string()), a);
    }

    #[test]
    fn address_grammar_windows_and_locations() {
        assert_eq!(addr("a.rs#f:3-5").window(), Some(win(3, Some(5))));
        assert_eq!(addr("a.rs#f:3-").window(), Some(win(3, None)));
        assert_eq!(addr("a.rs#f:3").window(), Some(win(3, Some(3))));
        // `:0` is not window text, so the colon stays part of the name.
        assert_eq!(addr("a.rs#f:0").segments()[0].name, "f:0");
        assert_eq!(addr("a.rs#f:0").window(), None);
        assert_eq!(
            Address::parse("src/a.rs:0").unwrap_err(),
            AddressError::BadWindow
        );
        assert_eq!(
            Address::parse("src/a.rs:120:15").unwrap(),
            Parsed::Location {
                file: "src/a.rs".into(),
                line: 120,
                column: Some(15)
            }
        );
        assert!(matches!(
            Address::parse("main:3").unwrap(),
            Parsed::Address(_)
        ));
        assert_eq!(
            Address::parse("/x.rs#f").unwrap_err(),
            AddressError::AbsoluteFile
        );
    }

    #[test]
    fn window_clamps_to_span() {
        let span = LineRange::new(10, 14);
        let w = win;
        assert_eq!(
            w(2, Some(3)).to_lines(span).unwrap(),
            (LineRange::new(11, 12), false)
        );
        assert_eq!(
            w(2, None).to_lines(span).unwrap(),
            (LineRange::new(11, 14), false)
        );
        assert_eq!(
            w(4, Some(99)).to_lines(span).unwrap(),
            (LineRange::new(13, 14), true)
        );
        assert_eq!(
            w(6, None).to_lines(span).unwrap_err(),
            ResolveError::LineOutOfRange { length: 5 }
        );
    }

    fn symbol(
        names: &[&str],
        kind: SymbolKind,
        lines: (u32, u32),
        children: Vec<Symbol>,
    ) -> Symbol {
        let range = LineRange::new(lines.0, lines.1);
        Symbol {
            path: SymbolPath::new(
                Some("f.rs".into()),
                names.iter().map(|n| n.to_string()).collect(),
            ),
            kind,
            name: names.last().unwrap().to_string(),
            range,
            body: range,
            signature: format!("sig {}", names.last().unwrap()),
            doc: None,
            children,
        }
    }

    /// A file with a header, a type, an impl with two same-named methods and a tail comment.
    fn sample() -> (String, AddressMap) {
        let src = "// header\n\nstruct Foo;\n\nimpl Foo {\n    fn new() {}\n    // note\n    fn new() {}\n}\n\n// tail\n";
        let new = |n| symbol(&["Foo", "new"], SymbolKind::Method, (n, n), Vec::new());
        let symbols = vec![
            symbol(&["Foo"], SymbolKind::Struct, (3, 3), Vec::new()),
            symbol(&["Foo"], SymbolKind::Impl, (5, 9), vec![new(6), new(8)]),
        ];
        let map = AddressMap::build_parts(
            std::path::Path::new("f.rs"),
            &symbols,
            src,
            &crate::lang::regions::GENERIC_RULES,
        );
        (src.to_owned(), map)
    }

    #[test]
    fn address_totality_and_uniqueness() {
        let (src, map) = sample();
        let names: Vec<String> = map.units().iter().map(|u| u.address.to_string()).collect();
        assert_eq!(
            names,
            [
                "f.rs#@header",
                "f.rs#Foo",
                "f.rs#@after/Foo",
                "f.rs#Foo~2",
                "f.rs#Foo/new",
                "f.rs#Foo/new~2",
                "f.rs#@after/Foo%7E2",
            ]
        );
        for line in 1..=crate::lang::line_count(&src) {
            let located = map.at_line(line).expect("every line has an address");
            let resolved = map.resolve(&located.address).unwrap();
            assert!(resolved.window.unwrap().start == line, "line {line}");
        }
        assert!(map.at_line(12).is_none());
        for (i, unit) in map.units().iter().enumerate() {
            assert_eq!(map.resolve(&unit.address).unwrap().unit, i);
        }
    }

    #[test]
    fn address_positions_windows_and_bytes() {
        let (src, map) = sample();
        let at = |n| map.address_at(std::path::Path::new("f.rs"), n).unwrap();
        assert_eq!(at(1), "f.rs#@header:1");
        assert_eq!(at(5), "f.rs#Foo~2:1");
        assert_eq!(at(7), "f.rs#Foo~2:3");
        assert_eq!(at(8), "f.rs#Foo/new~2:1");
        assert_eq!(at(9), "f.rs#Foo~2:5");
        let r = map.resolve(&addr("f.rs#Foo~2:2-3")).unwrap();
        assert_eq!(&src[r.bytes], "    fn new() {}\n    // note\n");
        let r = map.resolve(&addr("f.rs#Foo~2:4-99")).unwrap();
        assert!(r.clamped);
        assert_eq!(
            map.resolve(&addr("f.rs#Foo:2")).unwrap_err(),
            ResolveError::LineOutOfRange { length: 1 }
        );
        assert!(matches!(
            map.resolve(&addr("f.rs#Missing/new")),
            Err(ResolveError::UnknownUnit { .. })
        ));
        assert_eq!(map.imports_insertion_line(), 3);
    }

    #[test]
    fn address_names_collapse_impl_into_declaration() {
        let (_, map) = sample();
        let foo: Vec<String> = map
            .named("Foo")
            .into_iter()
            .map(|u| map.units()[u].address.to_string())
            .collect();
        assert_eq!(foo, ["f.rs#Foo"]);
        assert_eq!(map.named("new").len(), 2);
        let near = map.near(&addr("f.rs#Foo/nwe"));
        assert_eq!(near.len(), 2);
    }
}
