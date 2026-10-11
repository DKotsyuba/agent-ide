//! Symbol addresses: one canonical text per code location, `src/a.rs#Type/method:7`.
//!
//! Contract: `docs/contracts/symbol-address-v1.md` (tracked boundary `symbol-address-v1`, r2).
//! [`Address`] is the pure text form (parse and print, no outline needed). [`AddressMap`] binds
//! addresses to the lines of one file from its [`Outline`] and source text. The grammar and its
//! escapes are implemented; the map is a published stub (`todo!()` bodies) that later tasks fill
//! in without changing a signature.

use std::{fmt, ops::Range, path::PathBuf};

use super::{
    InsertSite, InsertWhere, LangError, LanguageSupport, LineRange, Outline, SymbolKind, SymbolPath,
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
    if let Some((name, k)) = raw.rsplit_once('~') {
        if let Ok(ordinal) = k.parse::<u32>() {
            if ordinal >= 2 && !name.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) {
                return Ok(Segment {
                    name: decode_name(name),
                    ordinal,
                });
            }
        }
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
        match &self.file {
            Some(file) => write!(f, "{}#", file.display())?,
            None => {}
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
    #[allow(dead_code)] // read by the T-002 builder and `at_line`
    line_count: u32,
}

impl AddressMap {
    /// Builds the map: outline symbols plus regions so every line `1..=line_count` has exactly one
    /// innermost unit. Reads the language's region rules from `outline.language`. Never fails.
    pub fn build(outline: &Outline, source: &str) -> AddressMap {
        let _ = (outline, source);
        todo!("T-002")
    }

    /// Units in file order, pre-order.
    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    /// File this map describes.
    pub fn file(&self) -> &std::path::Path {
        &self.file
    }

    /// The unit named by `path` (symbols and regions; a region is the one-segment `@imports`).
    pub fn unit(&self, path: &SymbolPath) -> Option<&Unit> {
        let _ = path;
        todo!("T-003")
    }

    /// Address (with window) to absolute lines and byte range.
    pub fn resolve(&self, address: &Address) -> Result<Resolved, ResolveError> {
        let _ = address;
        todo!("T-003")
    }

    /// The position address of `line`; `None` only beyond the last line.
    pub fn at_line(&self, line: u32) -> Option<Located> {
        let _ = line;
        todo!("T-003")
    }

    /// `file#unit:N` for `line`, without the absolute line.
    pub fn address_at(&self, file: &std::path::Path, line: u32) -> Option<String> {
        let _ = (file, line);
        todo!("T-003")
    }

    /// Units whose last segment equals `name`, file order, with the declaration-and-impl collapse.
    pub fn named(&self, name: &str) -> Vec<usize> {
        let _ = name;
        todo!("T-003")
    }

    /// At most five units near an unresolvable `address`.
    pub fn near(&self, address: &Address) -> Vec<usize> {
        let _ = address;
        todo!("T-003")
    }

    /// Line where a new import belongs: after the last import, else after `@header`, else 1.
    pub fn imports_insertion_line(&self) -> u32 {
        todo!("T-003")
    }

    /// Insertion point relative to `anchor`. A region anchor is placed here (`After` at its last
    /// line + 1, `Before` at its first line); a symbol anchor delegates to
    /// [`LanguageSupport::insert_site`].
    pub fn insert_site(
        &self,
        source: &str,
        lang: &dyn LanguageSupport,
        anchor: &Address,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        let _ = (source, lang, anchor, where_);
        todo!("T-003")
    }
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

    #[test]
    #[ignore = "T-002/T-003: AddressMap bodies land after the foundation"]
    fn address_map_stub_is_pending() {
        todo!()
    }
}
