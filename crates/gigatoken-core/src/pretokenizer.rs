use unicode_general_category::{GeneralCategory, get_general_category};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CharClass {
    Upper,
    Lower,
    Caseless,
    Mark,
    Number,
    Whitespace,
    Other,
}

#[derive(Clone, Copy)]
enum CaseState {
    Upper { last_caseless_end: usize },
    Lower,
}

pub(crate) struct O200kPretokens<'a> {
    text: &'a str,
    position: usize,
}

impl<'a> O200kPretokens<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self { text, position: 0 }
    }
}

impl<'a> Iterator for O200kPretokens<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.position == self.text.len() {
            return None;
        }
        let start = self.position;
        self.position = advance(self.text, start);
        Some(&self.text.as_bytes()[start..self.position])
    }
}

fn classify(character: char) -> CharClass {
    match get_general_category(character) {
        GeneralCategory::UppercaseLetter | GeneralCategory::TitlecaseLetter => CharClass::Upper,
        GeneralCategory::LowercaseLetter => CharClass::Lower,
        GeneralCategory::ModifierLetter | GeneralCategory::OtherLetter => CharClass::Caseless,
        GeneralCategory::NonspacingMark
        | GeneralCategory::SpacingMark
        | GeneralCategory::EnclosingMark => CharClass::Mark,
        GeneralCategory::DecimalNumber
        | GeneralCategory::LetterNumber
        | GeneralCategory::OtherNumber => CharClass::Number,
        _ if character.is_whitespace() => CharClass::Whitespace,
        _ => CharClass::Other,
    }
}

fn character_at(text: &str, position: usize) -> (char, usize) {
    let character = text[position..]
        .chars()
        .next()
        .expect("position is before the end of valid UTF-8");
    (character, character.len_utf8())
}

fn ascii_state(byte: u8) -> CaseState {
    if byte.is_ascii_uppercase() {
        CaseState::Upper {
            last_caseless_end: 0,
        }
    } else {
        CaseState::Lower
    }
}

fn letter_first(text: &str, position: usize) -> Option<(usize, CaseState)> {
    let &byte = text.as_bytes().get(position)?;
    if byte.is_ascii_alphabetic() {
        return Some((position + 1, ascii_state(byte)));
    }
    if byte.is_ascii() {
        return None;
    }
    let (character, width) = character_at(text, position);
    let end = position + width;
    match classify(character) {
        CharClass::Upper => Some((
            end,
            CaseState::Upper {
                last_caseless_end: 0,
            },
        )),
        CharClass::Lower => Some((end, CaseState::Lower)),
        CharClass::Caseless | CharClass::Mark => Some((
            end,
            CaseState::Upper {
                last_caseless_end: end,
            },
        )),
        CharClass::Number | CharClass::Whitespace | CharClass::Other => None,
    }
}

fn scan_case_run(text: &str, mut position: usize, mut state: CaseState) -> usize {
    let bytes = text.as_bytes();
    while position < bytes.len() {
        let byte = bytes[position];
        if byte.is_ascii_uppercase() {
            if matches!(state, CaseState::Lower) {
                return position;
            }
            position += 1;
            continue;
        }
        if byte.is_ascii_lowercase() {
            state = CaseState::Lower;
            position += 1;
            continue;
        }
        if byte.is_ascii() {
            break;
        }
        let (character, width) = character_at(text, position);
        match classify(character) {
            CharClass::Upper => {
                if matches!(state, CaseState::Lower) {
                    return position;
                }
                position += width;
            }
            CharClass::Lower => {
                state = CaseState::Lower;
                position += width;
            }
            CharClass::Caseless | CharClass::Mark => {
                position += width;
                if let CaseState::Upper {
                    ref mut last_caseless_end,
                } = state
                {
                    *last_caseless_end = position;
                }
            }
            CharClass::Number | CharClass::Whitespace | CharClass::Other => break,
        }
    }
    match state {
        CaseState::Upper { last_caseless_end } if last_caseless_end != 0 => last_caseless_end,
        CaseState::Upper { .. } | CaseState::Lower => position,
    }
}

fn contraction_end(bytes: &[u8], end: usize) -> usize {
    if bytes.get(end) != Some(&b'\'') {
        return end;
    }
    match bytes.get(end + 1).map(u8::to_ascii_lowercase) {
        Some(b's' | b'd' | b'm' | b't') => end + 2,
        Some(b'l') if bytes.get(end + 2).map(u8::to_ascii_lowercase) == Some(b'l') => end + 3,
        Some(b'v' | b'r') if bytes.get(end + 2).map(u8::to_ascii_lowercase) == Some(b'e') => {
            end + 3
        }
        Some(0xC5) if bytes.get(end + 2) == Some(&0xBF) => end + 3,
        _ => end,
    }
}

fn scan_punctuation(text: &str, mut position: usize) -> usize {
    let bytes = text.as_bytes();
    while position < bytes.len() {
        let byte = bytes[position];
        if byte.is_ascii() {
            if byte.is_ascii_alphabetic()
                || byte.is_ascii_digit()
                || char::from(byte).is_ascii_whitespace()
            {
                break;
            }
            position += 1;
            continue;
        }
        let (character, width) = character_at(text, position);
        if matches!(classify(character), CharClass::Other | CharClass::Mark) {
            position += width;
        } else {
            break;
        }
    }
    position
}

fn scan_tail(bytes: &[u8], mut position: usize) -> usize {
    while bytes
        .get(position)
        .is_some_and(|byte| matches!(*byte, b'\r' | b'\n' | b'/'))
    {
        position += 1;
    }
    position
}

fn whitespace_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut position = start;
    let mut last_newline_end = None;
    let mut last_character_start = start;
    while position < bytes.len() {
        let (character, width) = character_at(text, position);
        if !character.is_whitespace() {
            break;
        }
        last_character_start = position;
        position += width;
        if matches!(character, '\r' | '\n') {
            last_newline_end = Some(position);
        }
    }
    if let Some(end) = last_newline_end {
        return end;
    }
    if position == bytes.len() {
        return position;
    }
    if last_character_start > start {
        return last_character_start;
    }
    position
}

fn number_end(text: &str, mut position: usize) -> usize {
    let mut count = 1;
    while position < text.len() && count < 3 {
        let (character, width) = character_at(text, position);
        if classify(character) != CharClass::Number {
            break;
        }
        position += width;
        count += 1;
    }
    position
}

fn letter_token_end(text: &str, position: usize, state: CaseState) -> usize {
    contraction_end(text.as_bytes(), scan_case_run(text, position, state))
}

fn advance(text: &str, position: usize) -> usize {
    let bytes = text.as_bytes();
    let first = bytes[position];

    if first.is_ascii_alphabetic() {
        return letter_token_end(text, position + 1, ascii_state(first));
    }

    if first == b' ' {
        let Some(&second) = bytes.get(position + 1) else {
            return position + 1;
        };
        if second.is_ascii_alphabetic() {
            return letter_token_end(text, position + 2, ascii_state(second));
        }
        if second.is_ascii() {
            if second.is_ascii_digit() {
                return position + 1;
            }
            if char::from(second).is_ascii_whitespace() {
                return whitespace_end(text, position);
            }
            return scan_tail(bytes, scan_punctuation(text, position + 2));
        }
        let (character, width) = character_at(text, position + 1);
        let after = position + 1 + width;
        return match classify(character) {
            CharClass::Upper => letter_token_end(
                text,
                after,
                CaseState::Upper {
                    last_caseless_end: 0,
                },
            ),
            CharClass::Lower => letter_token_end(text, after, CaseState::Lower),
            CharClass::Caseless | CharClass::Mark => letter_token_end(
                text,
                after,
                CaseState::Upper {
                    last_caseless_end: after,
                },
            ),
            CharClass::Whitespace => whitespace_end(text, position),
            CharClass::Number => position + 1,
            CharClass::Other => scan_tail(bytes, scan_punctuation(text, after)),
        };
    }

    if !first.is_ascii() {
        let (character, width) = character_at(text, position);
        let after = position + width;
        return match classify(character) {
            CharClass::Upper => letter_token_end(
                text,
                after,
                CaseState::Upper {
                    last_caseless_end: 0,
                },
            ),
            CharClass::Lower => letter_token_end(text, after, CaseState::Lower),
            CharClass::Caseless | CharClass::Mark => letter_token_end(
                text,
                after,
                CaseState::Upper {
                    last_caseless_end: after,
                },
            ),
            CharClass::Number => number_end(text, after),
            class => {
                if let Some((end, state)) = letter_first(text, after) {
                    letter_token_end(text, end, state)
                } else if class == CharClass::Whitespace {
                    whitespace_end(text, position)
                } else {
                    scan_tail(bytes, scan_punctuation(text, after))
                }
            }
        };
    }

    if first.is_ascii_digit() {
        return number_end(text, position + 1);
    }
    if matches!(first, b'\r' | b'\n') {
        return whitespace_end(text, position);
    }
    if char::from(first).is_ascii_whitespace() {
        if let Some((end, state)) = letter_first(text, position + 1) {
            return letter_token_end(text, end, state);
        }
        return whitespace_end(text, position);
    }
    if let Some((end, state)) = letter_first(text, position + 1) {
        return letter_token_end(text, end, state);
    }
    scan_tail(bytes, scan_punctuation(text, position + 1))
}

#[cfg(test)]
mod tests {
    use super::O200kPretokens;

    #[test]
    fn scalar_boundaries_cover_case_contractions_numbers_and_whitespace() {
        let text = "camelCase HTTPResponse don't 1234 hi!\n\n  ";
        let actual = O200kPretokens::new(text)
            .map(|bytes| std::str::from_utf8(bytes).expect("valid span"))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            [
                "camel",
                "Case",
                " HTTPResponse",
                " don't",
                " ",
                "123",
                "4",
                " hi",
                "!\n\n",
                "  "
            ]
        );
    }
}
