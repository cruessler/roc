use crate::parser::Progress::{self, *};
use crate::parser::{BadInputError, Col, EExpr, ParseResult, Parser, Row};
use crate::state::State;
use bumpalo::collections::vec::Vec;
use bumpalo::Bump;

/// The parser accepts all of these in any position where any one of them could
/// appear. This way, canonicalization can give more helpful error messages like
/// "you can't redefine this tag!" if you wrote `Foo = ...` or
/// "you can only define unqualified constants" if you wrote `Foo.bar = ...`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ident<'a> {
    /// Foo or Bar
    GlobalTag(&'a str),
    /// @Foo or @Bar
    PrivateTag(&'a str),
    /// foo or foo.bar or Foo.Bar.baz.qux
    Access {
        module_name: &'a str,
        parts: &'a [&'a str],
    },
    /// .foo { foo: 42 }
    AccessorFunction(&'a str),
    /// .Foo or foo. or something like foo.Bar
    Malformed(&'a str, BadIdent),
}

impl<'a> Ident<'a> {
    pub fn len(&self) -> usize {
        use self::Ident::*;

        match self {
            GlobalTag(string) | PrivateTag(string) => string.len(),
            Access { module_name, parts } => {
                let mut len = if module_name.is_empty() {
                    0
                } else {
                    module_name.len() + 1
                    // +1 for the dot
                };

                for part in parts.iter() {
                    len += part.len() + 1 // +1 for the dot
                }

                len - 1
            }
            AccessorFunction(string) => string.len(),
            Malformed(string, _) => string.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// This could be:
///
/// * A record field, e.g. "email" in `.email` or in `email:`
/// * A named pattern match, e.g. "foo" in `foo =` or `foo ->` or `\foo ->`
pub fn lowercase_ident<'a>() -> impl Parser<'a, &'a str, ()> {
    move |_, state: State<'a>| match chomp_lowercase_part(state.bytes()) {
        Err(progress) => Err((progress, (), state)),
        Ok(ident) => {
            if crate::keyword::KEYWORDS.iter().any(|kw| &ident == kw) {
                Err((NoProgress, (), state))
            } else {
                let width = ident.len();
                match state.advance_without_indenting_ee(width, |_, _| ()) {
                    Ok(state) => Ok((MadeProgress, ident, state)),
                    Err(bad) => Err(bad),
                }
            }
        }
    }
}

pub fn tag_name<'a>() -> impl Parser<'a, &'a str, ()> {
    move |arena, state: State<'a>| {
        if state.bytes().starts_with(b"@") {
            match chomp_private_tag(state.bytes(), state.line, state.column) {
                Err(BadIdent::Start(_, _)) => Err((NoProgress, (), state)),
                Err(_) => Err((MadeProgress, (), state)),
                Ok(ident) => {
                    let width = ident.len();
                    match state.advance_without_indenting_ee(width, |_, _| ()) {
                        Ok(state) => Ok((MadeProgress, ident, state)),
                        Err(bad) => Err(bad),
                    }
                }
            }
        } else {
            uppercase_ident().parse(arena, state)
        }
    }
}

/// This could be:
///
/// * A module name
/// * A type name
/// * A global tag
pub fn uppercase_ident<'a>() -> impl Parser<'a, &'a str, ()> {
    move |_, state: State<'a>| match chomp_uppercase_part(state.bytes()) {
        Err(progress) => Err((progress, (), state)),
        Ok(ident) => {
            let width = ident.len();
            match state.advance_without_indenting_ee(width, |_, _| ()) {
                Ok(state) => Ok((MadeProgress, ident, state)),
                Err(bad) => Err(bad),
            }
        }
    }
}

pub fn unqualified_ident<'a>() -> impl Parser<'a, &'a str, ()> {
    move |_, state: State<'a>| match chomp_part(|c| c.is_alphabetic(), state.bytes()) {
        Err(progress) => Err((progress, (), state)),
        Ok(ident) => {
            if crate::keyword::KEYWORDS.iter().any(|kw| &ident == kw) {
                Err((MadeProgress, (), state))
            } else {
                let width = ident.len();
                match state.advance_without_indenting_ee(width, |_, _| ()) {
                    Ok(state) => Ok((MadeProgress, ident, state)),
                    Err(bad) => Err(bad),
                }
            }
        }
    }
}

macro_rules! advance_state {
    ($state:expr, $n:expr) => {
        $state.advance_without_indenting_ee($n, |r, c| {
            BadIdent::Space(crate::parser::BadInputError::LineTooLong, r, c)
        })
    };
}

pub fn parse_ident<'a>(arena: &'a Bump, state: State<'a>) -> ParseResult<'a, Ident<'a>, EExpr<'a>> {
    let initial = state.clone();

    match parse_ident_help(arena, state) {
        Ok((progress, ident, state)) => {
            if let Ident::Access { module_name, parts } = ident {
                if module_name.is_empty() {
                    if let Some(first) = parts.first() {
                        for keyword in crate::keyword::KEYWORDS.iter() {
                            if first == keyword {
                                return Err((
                                    NoProgress,
                                    EExpr::Start(initial.line, initial.column),
                                    initial,
                                ));
                            }
                        }
                    }
                }
            }

            Ok((progress, ident, state))
        }
        Err((NoProgress, _, state)) => {
            Err((NoProgress, EExpr::Start(state.line, state.column), state))
        }
        Err((MadeProgress, fail, state)) => match fail {
            BadIdent::Start(r, c) => Err((NoProgress, EExpr::Start(r, c), state)),
            BadIdent::Space(e, r, c) => Err((NoProgress, EExpr::Space(e, r, c), state)),
            _ => malformed_identifier(initial.bytes(), fail, state),
        },
    }
}

fn malformed_identifier<'a>(
    initial_bytes: &'a [u8],
    problem: BadIdent,
    mut state: State<'a>,
) -> ParseResult<'a, Ident<'a>, EExpr<'a>> {
    let chomped = chomp_malformed(state.bytes());
    let delta = initial_bytes.len() - state.bytes().len();
    let parsed_str = unsafe { std::str::from_utf8_unchecked(&initial_bytes[..chomped + delta]) };

    state = state.advance_without_indenting_ee(chomped, |r, c| {
        EExpr::Space(crate::parser::BadInputError::LineTooLong, r, c)
    })?;

    Ok((MadeProgress, Ident::Malformed(parsed_str, problem), state))
}

/// skip forward to the next non-identifier character
pub fn chomp_malformed(bytes: &[u8]) -> usize {
    use encode_unicode::CharExt;
    let mut chomped = 0;
    while let Ok((ch, width)) = char::from_utf8_slice_start(&bytes[chomped..]) {
        // We can't use ch.is_alphanumeric() here because that passes for
        // things that are "numeric" but not ASCII digits, like `¾`
        if ch == '.' || ch == '_' || ch.is_alphabetic() || ch.is_ascii_digit() {
            chomped += width;
            continue;
        } else {
            break;
        }
    }

    chomped
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadIdent {
    Start(Row, Col),
    Space(BadInputError, Row, Col),

    Underscore(Row, Col),
    QualifiedTag(Row, Col),
    WeirdAccessor(Row, Col),
    WeirdDotAccess(Row, Col),
    WeirdDotQualified(Row, Col),
    StrayDot(Row, Col),
    BadPrivateTag(Row, Col),
}

fn chomp_lowercase_part(buffer: &[u8]) -> Result<&str, Progress> {
    chomp_part(|c: char| c.is_lowercase(), buffer)
}

fn chomp_uppercase_part(buffer: &[u8]) -> Result<&str, Progress> {
    chomp_part(|c: char| c.is_uppercase(), buffer)
}

#[inline(always)]
fn chomp_part<F>(leading_is_good: F, buffer: &[u8]) -> Result<&str, Progress>
where
    F: Fn(char) -> bool,
{
    use encode_unicode::CharExt;

    let mut chomped = 0;

    if let Ok((ch, width)) = char::from_utf8_slice_start(&buffer[chomped..]) {
        if leading_is_good(ch) {
            chomped += width;
        } else {
            return Err(NoProgress);
        }
    }

    while let Ok((ch, width)) = char::from_utf8_slice_start(&buffer[chomped..]) {
        if ch.is_alphabetic() || ch.is_ascii_digit() {
            chomped += width;
        } else {
            // we're done
            break;
        }
    }

    if chomped == 0 {
        Err(NoProgress)
    } else {
        let name = unsafe { std::str::from_utf8_unchecked(&buffer[..chomped]) };

        Ok(name)
    }
}

/// a `.foo` accessor function
fn chomp_accessor(buffer: &[u8], row: Row, col: Col) -> Result<&str, BadIdent> {
    // assumes the leading `.` has been chomped already
    use encode_unicode::CharExt;

    match chomp_lowercase_part(buffer) {
        Ok(name) => {
            let chomped = name.len();

            if let Ok(('.', _)) = char::from_utf8_slice_start(&buffer[chomped..]) {
                Err(BadIdent::WeirdAccessor(row, col))
            } else {
                Ok(name)
            }
        }
        Err(_) => {
            // we've already made progress with the initial `.`
            Err(BadIdent::StrayDot(row, col + 1))
        }
    }
}

/// a `@Token` private tag
fn chomp_private_tag(buffer: &[u8], row: Row, col: Col) -> Result<&str, BadIdent> {
    // assumes the leading `@` has NOT been chomped already
    debug_assert_eq!(buffer.get(0), Some(&b'@'));
    use encode_unicode::CharExt;

    match chomp_uppercase_part(&buffer[1..]) {
        Ok(name) => {
            let width = 1 + name.len();

            if let Ok(('.', _)) = char::from_utf8_slice_start(&buffer[width..]) {
                Err(BadIdent::BadPrivateTag(row, col + width as u16))
            } else {
                let value = unsafe { std::str::from_utf8_unchecked(&buffer[..width]) };
                Ok(value)
            }
        }
        Err(_) => Err(BadIdent::BadPrivateTag(row, col + 1)),
    }
}

fn chomp_identifier_chain<'a>(
    arena: &'a Bump,
    buffer: &'a [u8],
    row: Row,
    col: Col,
) -> Result<(u16, Ident<'a>), (u16, BadIdent)> {
    use encode_unicode::CharExt;

    let first_is_uppercase;
    let mut chomped = 0;

    match char::from_utf8_slice_start(&buffer[chomped..]) {
        Ok((ch, width)) => match ch {
            '.' => match chomp_accessor(&buffer[1..], row, col) {
                Ok(accessor) => {
                    let bytes_parsed = 1 + accessor.len();

                    return Ok((bytes_parsed as u16, Ident::AccessorFunction(accessor)));
                }
                Err(fail) => return Err((1, fail)),
            },
            '@' => match chomp_private_tag(buffer, row, col) {
                Ok(tagname) => {
                    let bytes_parsed = tagname.len();

                    return Ok((bytes_parsed as u16, Ident::PrivateTag(tagname)));
                }
                Err(fail) => return Err((1, fail)),
            },
            c if c.is_alphabetic() => {
                // fall through
                chomped += width;
                first_is_uppercase = c.is_uppercase();
            }
            _ => {
                return Err((0, BadIdent::Start(row, col)));
            }
        },
        Err(_) => return Err((0, BadIdent::Start(row, col))),
    }

    while let Ok((ch, width)) = char::from_utf8_slice_start(&buffer[chomped..]) {
        if ch.is_alphabetic() || ch.is_ascii_digit() {
            chomped += width;
        } else {
            // we're done
            break;
        }
    }

    if let Ok(('.', _)) = char::from_utf8_slice_start(&buffer[chomped..]) {
        let module_name = if first_is_uppercase {
            match chomp_module_chain(&buffer[chomped..]) {
                Ok(width) => {
                    chomped += width as usize;
                    unsafe { std::str::from_utf8_unchecked(&buffer[..chomped]) }
                }
                Err(MadeProgress) => todo!(),
                Err(NoProgress) => unsafe { std::str::from_utf8_unchecked(&buffer[..chomped]) },
            }
        } else {
            ""
        };

        let mut parts = Vec::with_capacity_in(4, arena);

        if !first_is_uppercase {
            let first_part = unsafe { std::str::from_utf8_unchecked(&buffer[..chomped]) };
            parts.push(first_part);
        }

        match chomp_access_chain(&buffer[chomped..], &mut parts) {
            Ok(width) => {
                chomped += width as usize;

                let ident = Ident::Access {
                    module_name,
                    parts: parts.into_bump_slice(),
                };

                Ok((chomped as u16, ident))
            }
            Err(0) if !module_name.is_empty() => Err((
                chomped as u16,
                BadIdent::QualifiedTag(row, chomped as u16 + col),
            )),
            Err(1) if parts.is_empty() => Err((
                chomped as u16 + 1,
                BadIdent::WeirdDotQualified(row, chomped as u16 + col + 1),
            )),
            Err(width) => Err((
                chomped as u16 + width,
                BadIdent::WeirdDotAccess(row, chomped as u16 + col + width),
            )),
        }
    } else if let Ok(('_', _)) = char::from_utf8_slice_start(&buffer[chomped..]) {
        // we don't allow underscores in the middle of an identifier
        // but still parse them (and generate a malformed identifier)
        // to give good error messages for this case
        Err((
            chomped as u16 + 1,
            BadIdent::Underscore(row, col + chomped as u16 + 1),
        ))
    } else if first_is_uppercase {
        // just one segment, starting with an uppercase letter; that's a global tag
        let value = unsafe { std::str::from_utf8_unchecked(&buffer[..chomped]) };
        Ok((chomped as u16, Ident::GlobalTag(value)))
    } else {
        // just one segment, starting with a lowercase letter; that's a normal identifier
        let value = unsafe { std::str::from_utf8_unchecked(&buffer[..chomped]) };
        let ident = Ident::Access {
            module_name: "",
            parts: arena.alloc([value]),
        };
        Ok((chomped as u16, ident))
    }
}

fn chomp_module_chain(buffer: &[u8]) -> Result<u16, Progress> {
    let mut chomped = 0;

    while let Some(b'.') = buffer.get(chomped) {
        match &buffer.get(chomped + 1..) {
            Some(slice) => match chomp_uppercase_part(slice) {
                Ok(name) => {
                    chomped += name.len() + 1;
                }
                Err(MadeProgress) => return Err(MadeProgress),
                Err(NoProgress) => break,
            },
            None => return Err(MadeProgress),
        }
    }

    if chomped == 0 {
        Err(NoProgress)
    } else {
        Ok(chomped as u16)
    }
}

pub fn concrete_type<'a>() -> impl Parser<'a, (&'a str, &'a str), ()> {
    move |_, state: State<'a>| match chomp_concrete_type(state.bytes()) {
        Err(progress) => Err((progress, (), state)),
        Ok((module_name, type_name, width)) => {
            match state.advance_without_indenting_ee(width, |_, _| ()) {
                Ok(state) => Ok((MadeProgress, (module_name, type_name), state)),
                Err(bad) => Err(bad),
            }
        }
    }
}

// parse a type name like `Result` or `Result.Result`
fn chomp_concrete_type(buffer: &[u8]) -> Result<(&str, &str, usize), Progress> {
    let first = crate::ident::chomp_uppercase_part(buffer)?;

    if let Some(b'.') = buffer.get(first.len()) {
        match crate::ident::chomp_module_chain(&buffer[first.len()..]) {
            Err(_) => Err(MadeProgress),
            Ok(rest) => {
                let width = first.len() + rest as usize;

                // we must explicitly check here for a trailing `.`
                if let Some(b'.') = buffer.get(width) {
                    return Err(MadeProgress);
                }

                let slice = &buffer[..width];

                match slice.iter().rev().position(|c| *c == b'.') {
                    None => Ok(("", first, first.len())),
                    Some(rev_index) => {
                        let index = slice.len() - rev_index;
                        let module_name =
                            unsafe { std::str::from_utf8_unchecked(&slice[..index - 1]) };
                        let type_name = unsafe { std::str::from_utf8_unchecked(&slice[index..]) };

                        Ok((module_name, type_name, width))
                    }
                }
            }
        }
    } else {
        Ok(("", first, first.len()))
    }
}

fn chomp_access_chain<'a>(buffer: &'a [u8], parts: &mut Vec<'a, &'a str>) -> Result<u16, u16> {
    let mut chomped = 0;

    while let Some(b'.') = buffer.get(chomped) {
        match &buffer.get(chomped + 1..) {
            Some(slice) => match chomp_lowercase_part(slice) {
                Ok(name) => {
                    let value = unsafe {
                        std::str::from_utf8_unchecked(
                            &buffer[chomped + 1..chomped + 1 + name.len()],
                        )
                    };
                    parts.push(value);

                    chomped += name.len() + 1;
                }
                Err(_) => return Err(chomped as u16 + 1),
            },
            None => return Err(chomped as u16 + 1),
        }
    }

    if chomped == 0 {
        Err(0)
    } else {
        Ok(chomped as u16)
    }
}

fn parse_ident_help<'a>(
    arena: &'a Bump,
    mut state: State<'a>,
) -> ParseResult<'a, Ident<'a>, BadIdent> {
    match chomp_identifier_chain(arena, state.bytes(), state.line, state.column) {
        Ok((width, ident)) => {
            state = advance_state!(state, width as usize)?;
            Ok((MadeProgress, ident, state))
        }
        Err((0, fail)) => Err((NoProgress, fail, state)),
        Err((width, fail)) => {
            state = advance_state!(state, width as usize)?;
            Err((MadeProgress, fail, state))
        }
    }
}
