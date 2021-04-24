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

//! Helper for parsing operator tokens.

use std::fmt;

/// Operator token identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operator {
    /// Newline
    Newline,
    /// `&`
    And,
    /// `&&`
    AndAnd,
    /// `(`
    OpenParen,
    /// `)`
    CloseParen,
    /// `;`
    Semicolon,
    /// `;;`
    SemicolonSemicolon,
    /// `<`
    Less,
    /// `<&`
    LessAnd,
    /// `<(`
    LessOpenParen,
    /// `<<`
    LessLess,
    /// `<<-`
    LessLessDash,
    /// `<<<`
    LessLessLess,
    /// `<>`
    LessGreater,
    /// `>`
    Greater,
    /// `>&`
    GreaterAnd,
    /// `>(`
    GreaterOpenParen,
    /// `>>`
    GreaterGreater,
    /// `>>|`
    GreaterGreaterBar,
    /// `>|`
    GreaterBar,
    /// `|`
    Bar,
    /// `||`
    BarBar,
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Operator::*;
        match self {
            Newline => f.write_str("\n"),
            And => f.write_str("&"),
            AndAnd => f.write_str("&&"),
            OpenParen => f.write_str("("),
            CloseParen => f.write_str(")"),
            Semicolon => f.write_str(";"),
            SemicolonSemicolon => f.write_str(";;"),
            Less => f.write_str("<"),
            LessAnd => f.write_str("<&"),
            LessOpenParen => f.write_str("<("),
            LessLess => f.write_str("<<"),
            LessLessDash => f.write_str("<<-"),
            LessLessLess => f.write_str("<<<"),
            LessGreater => f.write_str("<>"),
            Greater => f.write_str(">"),
            GreaterAnd => f.write_str(">&"),
            GreaterOpenParen => f.write_str(">("),
            GreaterGreater => f.write_str(">>"),
            GreaterGreaterBar => f.write_str(">>|"),
            GreaterBar => f.write_str(">|"),
            Bar => f.write_str("|"),
            BarBar => f.write_str("||"),
        }
    }
}

/// Trie data structure that defines a set of operator tokens.
///
/// This struct represents a node of the trie. A node is a sorted array of [`Edge`]s.
#[derive(Copy, Clone, Debug)]
pub struct Trie(&'static [Edge]);

/// Edge of a [`Trie`].
#[derive(Copy, Clone, Debug)]
pub struct Edge {
    /// Character value of this edge.
    pub key: char,
    /// Final operator token that is delimited after taking this edge if there are no longer
    /// matches.
    pub value: Option<Operator>,
    /// Sub-trie containing values for keys that have the common prefix.
    pub next: Trie,
}

impl Trie {
    /// Tests if this trie is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Finds an edge for the given key.
    pub fn edge(&self, key: char) -> Option<&Edge> {
        self.0
            .binary_search_by_key(&key, |edge| edge.key)
            .ok()
            .map(|i| &self.0[i])
    }
}

/// Trie containing all the operators.
pub const OPERATORS: Trie = Trie(&[
    Edge {
        key: '\n',
        value: Some(Operator::Newline),
        next: NONE,
    },
    Edge {
        key: '&',
        value: Some(Operator::And),
        next: AND,
    },
    Edge {
        key: '(',
        value: Some(Operator::OpenParen),
        next: NONE,
    },
    Edge {
        key: ')',
        value: Some(Operator::CloseParen),
        next: NONE,
    },
    Edge {
        key: ';',
        value: Some(Operator::Semicolon),
        next: SEMICOLON,
    },
    Edge {
        key: '<',
        value: Some(Operator::Less),
        next: LESS,
    },
    Edge {
        key: '>',
        value: Some(Operator::Greater),
        next: GREATER,
    },
    Edge {
        key: '|',
        value: Some(Operator::Bar),
        next: BAR,
    },
]);

/// Trie of the operators that start with `&`.
const AND: Trie = Trie(&[Edge {
    key: '&',
    value: Some(Operator::AndAnd),
    next: NONE,
}]);

/// Trie of the operators that start with `;`.
const SEMICOLON: Trie = Trie(&[Edge {
    key: ';',
    value: Some(Operator::SemicolonSemicolon),
    next: NONE,
}]);

/// Trie of the operators that start with `<`.
const LESS: Trie = Trie(&[
    Edge {
        key: '&',
        value: Some(Operator::LessAnd),
        next: NONE,
    },
    Edge {
        key: '(',
        value: Some(Operator::LessOpenParen),
        next: NONE,
    },
    Edge {
        key: '<',
        value: Some(Operator::LessLess),
        next: LESS_LESS,
    },
    Edge {
        key: '>',
        value: Some(Operator::LessGreater),
        next: NONE,
    },
]);

/// Trie of the operators that start with `<<`.
const LESS_LESS: Trie = Trie(&[
    Edge {
        key: '-',
        value: Some(Operator::LessLessDash),
        next: NONE,
    },
    Edge {
        key: '<',
        value: Some(Operator::LessLessLess),
        next: NONE,
    },
]);

/// Trie of the operators that start with `>`.
const GREATER: Trie = Trie(&[
    Edge {
        key: '&',
        value: Some(Operator::GreaterAnd),
        next: NONE,
    },
    Edge {
        key: '(',
        value: Some(Operator::GreaterOpenParen),
        next: NONE,
    },
    Edge {
        key: '>',
        value: Some(Operator::GreaterGreater),
        next: GREATER_GREATER,
    },
    Edge {
        key: '|',
        value: Some(Operator::GreaterBar),
        next: NONE,
    },
]);

/// Trie of the operators that start with `>>`.
const GREATER_GREATER: Trie = Trie(&[Edge {
    key: '|',
    value: Some(Operator::GreaterGreaterBar),
    next: NONE,
}]);

/// Trie of the operators that start with `|`.
const BAR: Trie = Trie(&[Edge {
    key: '|',
    value: Some(Operator::BarBar),
    next: NONE,
}]);

/// Trie containing nothing.
const NONE: Trie = Trie(&[]);

/// Tests whether the given character is the first character of an operator.
pub fn is_operator_char(c: char) -> bool {
    OPERATORS.edge(c).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_sorted(trie: &Trie) {
        assert!(
            trie.0.windows(2).all(|pair| pair[0].key < pair[1].key),
            "The trie should be sorted: {:?}",
            trie
        );

        for edge in trie.0 {
            ensure_sorted(&edge.next);
        }
    }

    #[test]
    fn tries_are_sorted() {
        ensure_sorted(&OPERATORS);
    }
}