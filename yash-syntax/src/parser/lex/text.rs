// This file is part of yash, an extended POSIX shell.
// Copyright (C) 2020 WATANABE Yuki
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Part of the lexer that parses texts.

use super::core::Lexer;
use crate::parser::core::Error;
use crate::parser::core::Result;
use crate::parser::core::SyntaxError;
use crate::syntax::Backslashed;
use crate::syntax::Literal;
use crate::syntax::Text;
use crate::syntax::TextUnit;

impl Lexer {
    /// Parses a [`TextUnit`].
    ///
    /// This function parses a literal character, backslash-escaped character,
    /// [dollar unit](Self::dollar_unit), or [backquote](Self::backquote),
    /// optionally preceded by line continuations.
    ///
    /// `is_delimiter` is a function that decides if a character is a delimiter.
    /// An unquoted character is parsed only if `is_delimiter` returns false for
    /// it.
    ///
    /// `is_escapable` decides if a character can be escaped by a backslash. When
    /// `is_escapable` returns false, the preceding backslash is considered
    /// literal.
    ///
    /// If the text unit is a backquote, treatment of `\"` inside the backquote
    /// depends on `is_escapable('_')`. If it is false, the text unit is in a
    /// double-quoted context and `\"` is an escaped double-quote. Otherwise, the
    /// text unit is in an unquoted context and `\"` is treated literally.
    pub async fn text_unit<F, G>(
        &mut self,
        is_delimiter: F,
        is_escapable: G,
    ) -> Result<Option<TextUnit>>
    where
        F: FnOnce(char) -> bool,
        G: FnOnce(char) -> bool,
    {
        self.line_continuations().await?;

        if self.skip_if(|c| c == '\\').await? {
            if let Some(c) = self.consume_char_if(is_escapable).await? {
                return Ok(Some(Backslashed(c.value)));
            } else {
                return Ok(Some(Literal('\\')));
            }
        }

        if let Some(u) = self.dollar_unit().await? {
            return Ok(Some(u));
        }

        if let Some(u) = self.backquote(!is_escapable('_')).await? {
            return Ok(Some(u));
        }

        if let Some(sc) = self.consume_char_if(|c| !is_delimiter(c)).await? {
            return Ok(Some(Literal(sc.value)));
        }

        Ok(None)
    }

    /// Parses a text, i.e., a (possibly empty) sequence of [`TextUnit`]s.
    ///
    /// `is_delimiter` tests if an unquoted character is a delimiter. When
    /// `is_delimiter` returns true, the parser ends parsing and returns the text
    /// up to the character as a result.
    ///
    /// `is_escapable` tests if a backslash can escape a character. When the
    /// parser founds an unquoted backslash, the next character is passed to
    /// `is_escapable`. If `is_escapable` returns true, the backslash is treated
    /// as a valid escape (`TextUnit::Backslashed`). Otherwise, it ia a
    /// literal (`TextUnit::Literal`).
    ///
    /// `is_escapable` also affects escaping of double-quotes inside backquotes.
    /// See [`text_unit`](Self::text_unit) for details.
    pub async fn text<F, G>(&mut self, mut is_delimiter: F, mut is_escapable: G) -> Result<Text>
    where
        F: FnMut(char) -> bool,
        G: FnMut(char) -> bool,
    {
        let mut units = vec![];

        while let Some(unit) = self.text_unit(&mut is_delimiter, &mut is_escapable).await? {
            units.push(unit);
        }

        Ok(Text(units))
    }

    /// Parses a text that may contain nested parentheses.
    ///
    /// This function works similarly to [`text`](Self::text). However, if an
    /// unquoted `(` is found in the text, all text units are parsed up to the
    /// next matching unquoted `)`. Inside the parentheses, the `is_delimiter`
    /// function is ignored and all non-special characters are parsed as literal
    /// word units. After finding the `)`, this function continues parsing to
    /// find a delimiter (as per `is_delimiter`) or another parentheses.
    ///
    /// Nested parentheses are supported: the number of `(`s and `)`s must
    /// match. In other words, the final delimiter is recognized only outside
    /// outermost parentheses.
    pub async fn text_with_parentheses<F, G>(
        &mut self,
        mut is_delimiter: F,
        mut is_escapable: G,
    ) -> Result<Text>
    where
        F: FnMut(char) -> bool,
        G: FnMut(char) -> bool,
    {
        let mut units = Vec::new();
        let mut open_paren_locations = Vec::new();
        loop {
            let is_delimiter_or_paren = |c| {
                if c == '(' {
                    return true;
                }
                if open_paren_locations.is_empty() {
                    is_delimiter(c)
                } else {
                    c == ')'
                }
            };
            let next_units = self.text(is_delimiter_or_paren, &mut is_escapable).await?.0;
            units.extend(next_units);
            if let Some(sc) = self.consume_char_if(|c| c == '(').await? {
                units.push(Literal('('));
                open_paren_locations.push(sc.location.clone());
            } else if let Some(opening_location) = open_paren_locations.pop() {
                if self.skip_if(|c| c == ')').await? {
                    units.push(Literal(')'));
                } else {
                    let cause = SyntaxError::UnclosedParen { opening_location }.into();
                    let location = self.location().await?.clone();
                    return Err(Error { cause, location });
                }
            } else {
                break;
            }
        }
        Ok(Text(units))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::core::ErrorCause;
    use crate::source::Source;
    use crate::syntax::Backquote;
    use crate::syntax::BackquoteUnit;
    use crate::syntax::CommandSubst;
    use futures::executor::block_on;

    #[test]
    fn lexer_text_unit_literal_accepted() {
        let mut lexer = Lexer::with_source(Source::Unknown, "X");
        let mut called = false;
        let result = block_on(lexer.text_unit(
            |c| {
                called = true;
                assert_eq!(c, 'X');
                false
            },
            |c| {
                assert_eq!(c, '_');
                true
            },
        ))
        .unwrap()
        .unwrap();
        assert!(called);
        if let Literal(c) = result {
            assert_eq!(c, 'X');
        } else {
            panic!("unexpected result {:?}", result);
        }

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_unit_literal_rejected() {
        let mut lexer = Lexer::with_source(Source::Unknown, ";");
        let mut called = false;
        let result = block_on(lexer.text_unit(
            |c| {
                called = true;
                assert_eq!(c, ';');
                true
            },
            |c| {
                assert_eq!(c, '_');
                true
            },
        ))
        .unwrap();
        assert!(called);
        assert_eq!(result, None);

        assert_eq!(block_on(lexer.peek_char()).unwrap().unwrap().value, ';');
    }

    #[test]
    fn lexer_text_unit_backslash_accepted() {
        let mut lexer = Lexer::with_source(Source::Unknown, r"\#");
        let mut called = false;
        let result = block_on(lexer.text_unit(
            |c| panic!("unexpected call to is_delimiter({:?})", c),
            |c| {
                called = true;
                assert_eq!(c, '#');
                true
            },
        ))
        .unwrap()
        .unwrap();
        assert!(called);
        assert_eq!(result, Backslashed('#'));

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_unit_backslash_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, r"\");
        let result = block_on(lexer.text_unit(
            |c| panic!("unexpected call to is_delimiter({:?})", c),
            |c| panic!("unexpected call to is_escapable({:?})", c),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(result, Literal('\\'));

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_unit_dollar() {
        let mut lexer = Lexer::with_source(Source::Unknown, "$()");
        let result = block_on(lexer.text_unit(
            |c| panic!("unexpected call to is_delimiter({:?})", c),
            |c| panic!("unexpected call to is_escapable({:?})", c),
        ))
        .unwrap()
        .unwrap();
        if let CommandSubst { content, location } = result {
            assert_eq!(content, "");
            assert_eq!(location.column.get(), 1);
        } else {
            panic!("unexpected result {:?}", result);
        }

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_unit_backquote_double_quote_escapable() {
        let mut lexer = Lexer::with_source(Source::Unknown, r#"`\"`"#);
        let result = block_on(lexer.text_unit(
            |c| panic!("unexpected call to is_delimiter({:?})", c),
            |c| {
                assert_eq!(c, '_');
                false
            },
        ))
        .unwrap()
        .unwrap();
        if let Backquote { content, location } = result {
            assert_eq!(content, [BackquoteUnit::Backslashed('"')]);
            assert_eq!(location.column.get(), 1);
        } else {
            panic!("Not a backquote: {:?}", result);
        }

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_unit_backquote_double_quote_not_escapable() {
        let mut lexer = Lexer::with_source(Source::Unknown, r#"`\"`"#);
        let result = block_on(lexer.text_unit(
            |c| panic!("unexpected call to is_delimiter({:?})", c),
            |c| {
                assert_eq!(c, '_');
                true
            },
        ))
        .unwrap()
        .unwrap();
        if let Backquote { content, location } = result {
            assert_eq!(
                content,
                [BackquoteUnit::Literal('\\'), BackquoteUnit::Literal('"')]
            );
            assert_eq!(location.column.get(), 1);
        } else {
            panic!("Not a backquote: {:?}", result);
        }

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_unit_line_continuations() {
        let mut lexer = Lexer::with_source(Source::Unknown, "\\\n\\\nX");
        let result = block_on(lexer.text_unit(
            |_| false,
            |c| {
                assert_eq!(c, '_');
                true
            },
        ))
        .unwrap()
        .unwrap();
        if let Literal(c) = result {
            assert_eq!(c, 'X');
        } else {
            panic!("unexpected result {:?}", result);
        }

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_empty() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let Text(units) = block_on(lexer.text(
            |c| panic!("unexpected call to is_delimiter({:?})", c),
            |c| {
                assert_eq!(c, '_');
                true
            },
        ))
        .unwrap();
        assert_eq!(units, &[]);
    }

    #[test]
    fn lexer_text_nonempty() {
        let mut lexer = Lexer::with_source(Source::Unknown, "abc");
        let mut called = 0;
        let Text(units) = block_on(lexer.text(
            |c| {
                assert!(
                    matches!(c, 'a' | 'b' | 'c'),
                    "unexpected call to is_delimiter({:?}), called={}",
                    c,
                    called
                );
                called += 1;
                false
            },
            |c| {
                assert_eq!(c, '_');
                true
            },
        ))
        .unwrap();
        assert_eq!(units, &[Literal('a'), Literal('b'), Literal('c')]);
        assert_eq!(called, 3);

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_delimiter() {
        let mut lexer = Lexer::with_source(Source::Unknown, "abc");
        let mut called = 0;
        let Text(units) = block_on(lexer.text(
            |c| {
                assert!(
                    matches!(c, 'a' | 'b' | 'c'),
                    "unexpected call to is_delimiter({:?}), called={}",
                    c,
                    called
                );
                called += 1;
                c == 'c'
            },
            |c| {
                assert_eq!(c, '_');
                true
            },
        ))
        .unwrap();
        assert_eq!(units, &[Literal('a'), Literal('b')]);
        assert_eq!(called, 3);

        assert_eq!(block_on(lexer.peek_char()).unwrap().unwrap().value, 'c');
    }

    #[test]
    fn lexer_text_escaping() {
        let mut lexer = Lexer::with_source(Source::Unknown, r"a\b\c");
        let mut tested_chars = String::new();
        let Text(units) = block_on(lexer.text(
            |_| false,
            |c| {
                tested_chars.push(c);
                c == 'b'
            },
        ))
        .unwrap();
        assert_eq!(
            units,
            &[Literal('a'), Backslashed('b'), Literal('\\'), Literal('c')]
        );
        assert_eq!(tested_chars, "_bc__");

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_with_parentheses_no_parentheses() {
        let mut lexer = Lexer::with_source(Source::Unknown, "abc");
        let Text(units) = block_on(lexer.text_with_parentheses(|_| false, |_| false)).unwrap();
        assert_eq!(units, &[Literal('a'), Literal('b'), Literal('c')]);

        assert_eq!(block_on(lexer.peek_char()), Ok(None));
    }

    #[test]
    fn lexer_text_with_parentheses_nest_1() {
        let mut lexer = Lexer::with_source(Source::Unknown, "a(b)c)");
        let Text(units) =
            block_on(lexer.text_with_parentheses(|c| c == 'b' || c == ')', |_| false)).unwrap();
        assert_eq!(
            units,
            &[
                Literal('a'),
                Literal('('),
                Literal('b'),
                Literal(')'),
                Literal('c'),
            ]
        );

        assert_eq!(block_on(lexer.peek_char()).unwrap().unwrap().value, ')');
    }

    #[test]
    fn lexer_text_with_parentheses_nest_1_1() {
        let mut lexer = Lexer::with_source(Source::Unknown, "ab(CD)ef(GH)ij;");
        let Text(units) = block_on(
            lexer.text_with_parentheses(|c| c.is_ascii_uppercase() || c == ';', |_| false),
        )
        .unwrap();
        assert_eq!(
            units,
            &[
                Literal('a'),
                Literal('b'),
                Literal('('),
                Literal('C'),
                Literal('D'),
                Literal(')'),
                Literal('e'),
                Literal('f'),
                Literal('('),
                Literal('G'),
                Literal('H'),
                Literal(')'),
                Literal('i'),
                Literal('j'),
            ]
        );

        assert_eq!(block_on(lexer.peek_char()).unwrap().unwrap().value, ';');
    }

    #[test]
    fn lexer_text_with_parentheses_nest_3() {
        let mut lexer = Lexer::with_source(Source::Unknown, "a(B((C)D))e;");
        let Text(units) = block_on(
            lexer.text_with_parentheses(|c| c.is_ascii_uppercase() || c == ';', |_| false),
        )
        .unwrap();
        assert_eq!(
            units,
            &[
                Literal('a'),
                Literal('('),
                Literal('B'),
                Literal('('),
                Literal('('),
                Literal('C'),
                Literal(')'),
                Literal('D'),
                Literal(')'),
                Literal(')'),
                Literal('e'),
            ]
        );

        assert_eq!(block_on(lexer.peek_char()).unwrap().unwrap().value, ';');
    }

    #[test]
    fn lexer_text_with_parentheses_unclosed() {
        let mut lexer = Lexer::with_source(Source::Unknown, "x(()");
        let e = block_on(lexer.text_with_parentheses(|_| false, |_| false)).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedParen { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, "x(()");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 2);
        } else {
            panic!("unexpected error cause {:?}", e);
        }
        assert_eq!(e.location.line.value, "x(()");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 5);
    }
}