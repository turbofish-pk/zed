//! Rendering of control characters that would otherwise be invisible.

use std::borrow::Cow;

/// The stand-in used for a line feed (`\n`).
///
/// This is the symbol Zed has always used for line feeds in single-line labels,
/// so text containing `\n` keeps rendering the way it used to.
const LINE_FEED_SYMBOL: char = '⏎';

/// Start of the Unicode "Control Pictures" block (`U+2400`), which holds one
/// printable stand-in per C0 control character, in code point order. So the
/// stand-in for a control character is `U+2400 + <its code point>`.
const CONTROL_PICTURES_START: u32 = 0x2400;

/// The stand-in for `DEL` (`U+007F`), which sits outside the contiguous range
/// covered by [`CONTROL_PICTURES_START`].
const DELETE_SYMBOL: char = '␡';

/// Returns the printable stand-in for a control character, or `None` if the
/// character is already printable.
///
/// C1 controls (`U+0080..=U+009F`) are deliberately left alone: they have no
/// Control Pictures equivalent to map onto.
fn printable_substitute(character: char) -> Option<char> {
    match character {
        '\n' => Some(LINE_FEED_SYMBOL),
        '\x7f' => Some(DELETE_SYMBOL),
        character if character.is_ascii_control() => {
            char::from_u32(CONTROL_PICTURES_START + character as u32)
        }
        _ => None,
    }
}

/// Replaces control characters with printable stand-ins, so that text
/// containing them stays legible instead of collapsing the line it is rendered
/// on.
///
/// File names are allowed to contain characters like `\n`, `\r` and `\t` on
/// most platforms, and rendering those verbatim breaks the layout of whatever
/// is displaying them. Every stand-in is a single character, so byte offsets
/// change but character offsets are preserved.
///
/// The input is returned untouched when it holds no control characters, which
/// is the overwhelmingly common case.
///
/// # Examples
///
/// ```
/// use ui::utils::replace_control_characters;
///
/// assert_eq!(replace_control_characters("main.rs"), "main.rs");
/// assert_eq!(replace_control_characters("two\nlines.rs"), "two⏎lines.rs");
/// assert_eq!(replace_control_characters("tab\there.rs"), "tab␉here.rs");
/// ```
pub fn replace_control_characters(text: &str) -> Cow<'_, str> {
    let Some(first_control_character) = text.find(|character: char| character.is_ascii_control())
    else {
        return Cow::Borrowed(text);
    };

    let (printable_prefix, rest) = text.split_at(first_control_character);
    let mut replaced = String::with_capacity(text.len());
    replaced.push_str(printable_prefix);
    for character in rest.chars() {
        match printable_substitute(character) {
            Some(substitute) => replaced.push(substitute),
            None => replaced.push(character),
        }
    }

    Cow::Owned(replaced)
}

/// Like [`replace_control_characters`], but also moves the given byte offsets
/// into the replaced string, and returns `None` when nothing was replaced.
///
/// Callers that carry byte offsets into the text — highlight positions, for
/// instance — cannot use [`replace_control_characters`] on its own: every
/// stand-in is wider in bytes than the character it replaces, so offsets past
/// the first replacement would land inside a character. Indexing a `str` at
/// such an offset panics in release builds too, so remapping is not optional.
///
/// Each offset must sit on a character boundary. Offsets past the end of the
/// text are clamped to the end of the replaced string.
///
/// # Examples
///
/// ```
/// use ui::utils::replace_control_characters_remapping_offsets;
///
/// // The offset of "b" moves from 2 to 4, the stand-in being three bytes wide.
/// let mut offsets = vec![0, 2];
/// let replaced = replace_control_characters_remapping_offsets("a\tb", &mut offsets);
/// assert_eq!(replaced.as_deref(), Some("a␉b"));
/// assert_eq!(offsets, vec![0, 4]);
/// ```
pub fn replace_control_characters_remapping_offsets(
    text: &str,
    offsets: &mut [usize],
) -> Option<String> {
    // Control characters are single bytes below 0x80, so scanning raw bytes
    // cannot mistake a UTF-8 continuation byte for one.
    if !text.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }

    // Maps every byte offset in `text` to its offset in the replaced string.
    // Only entries on character boundaries are filled in, which is all callers
    // are allowed to ask about.
    let mut moved = vec![0; text.len() + 1];
    let mut replaced = String::with_capacity(text.len());
    for (offset, character) in text.char_indices() {
        moved[offset] = replaced.len();
        match printable_substitute(character) {
            Some(substitute) => replaced.push(substitute),
            None => replaced.push(character),
        }
    }
    moved[text.len()] = replaced.len();

    for offset in offsets {
        debug_assert!(
            *offset > text.len() || text.is_char_boundary(*offset),
            "offset {offset} is not on a character boundary of {text:?}",
        );
        *offset = moved.get(*offset).copied().unwrap_or(replaced.len());
    }

    Some(replaced)
}

