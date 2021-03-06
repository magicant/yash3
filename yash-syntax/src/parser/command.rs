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

//! Syntax parser for command
//!
//! Note that the detail parser for each type of commands is in another
//! dedicated module.

use super::core::Parser;
use super::core::Rec;
use super::core::Result;
use super::fill::MissingHereDoc;
use crate::syntax::Command;

impl Parser<'_> {
    /// Parses a command.
    ///
    /// If there is no valid command at the current position, this function
    /// returns `Ok(Rec::Parsed(None))`.
    pub async fn command(&mut self) -> Result<Rec<Option<Command<MissingHereDoc>>>> {
        match self.simple_command().await? {
            Rec::AliasSubstituted => Ok(Rec::AliasSubstituted),
            Rec::Parsed(None) => self
                .full_compound_command()
                .await
                .map(|c| Rec::Parsed(c.map(Command::Compound))),
            Rec::Parsed(Some(c)) => self
                .short_function_definition(c)
                .await
                .map(|c| Rec::Parsed(Some(c))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fill::Fill;
    use super::super::lex::Lexer;
    use super::super::lex::TokenId::EndOfInput;
    use super::*;
    use crate::source::Source;
    use futures::executor::block_on;

    #[test]
    fn parser_command_simple() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo < bar");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.command()).unwrap().unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Simple(c) = result {
            assert_eq!(c.to_string(), "foo <bar");
        } else {
            panic!("Not a simple command: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_command_compound() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(foo) < bar");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.command()).unwrap().unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Compound(c) = result {
            assert_eq!(c.to_string(), "(foo) <bar");
        } else {
            panic!("Not a compound command: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_command_function() {
        let mut lexer = Lexer::with_source(Source::Unknown, "fun () ( echo )");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.command()).unwrap().unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Function(f) = result {
            assert_eq!(f.to_string(), "fun() (echo)");
        } else {
            panic!("Not a function definition: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_command_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let mut parser = Parser::new(&mut lexer);

        let option = block_on(parser.command()).unwrap().unwrap();
        assert_eq!(option, None);
    }
}
