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

//! Syntax parser for the shell language.
//!
//! TODO Elaborate

mod core;
mod fill;

pub mod lex;

use self::lex::Operator::*;
use self::lex::PartialHereDoc;
use self::lex::Token;
use self::lex::TokenId::*;
use super::syntax::*;
use std::rc::Rc;

pub use self::core::AsyncFnMut;
pub use self::core::Error;
pub use self::core::ErrorCause;
pub use self::core::Parser;
pub use self::core::Rec;
pub use self::core::Result;
pub use self::fill::Fill;
pub use self::fill::MissingHereDoc;

impl Parser<'_> {
    /// Consumes the current token with possible alias substitution fully applied.
    ///
    /// This function calls
    /// [`self.take_token_aliased(false)`](Parser::take_token_aliased) repeatedly
    /// until it returns `Ok(Rec::Parsed(_))` or `Err(_)` and then returns it.
    ///
    /// This function should be used only in contexts where no backtrack is
    /// needed after alias substitution.
    pub async fn take_token_aliased_fully(&mut self) -> Result<Token> {
        loop {
            if let Rec::Parsed(t) = self.take_token_aliased(false).await? {
                return Ok(t);
            }
        }
    }

    /// Parses a redirection.
    ///
    /// If the current token is not a redirection operator, `Ok(None)` is returned. If a word token
    /// is missing after the operator, `Err(Error{...})` is returned with a cause of
    /// [`MissingHereDocDelimiter`](ErrorCause::MissingHereDocDelimiter).
    pub async fn redirection(&mut self) -> Result<Option<Redir<MissingHereDoc>>> {
        // TODO IO_NUMBER
        let operator = match self.peek_token().await?.id {
            // TODO <, <>, >, >>, >|, <&, >&, >>|, <<<
            Operator(op) if op == LessLess || op == LessLessDash => {
                self.take_token().await.unwrap()
            }
            _ => return Ok(None),
        };

        let operand = self.take_token_aliased_fully().await?;
        match operand.id {
            Token => (),
            Operator(_) | EndOfInput => {
                return Err(Error {
                    cause: ErrorCause::MissingHereDocDelimiter,
                    location: operator.word.location,
                })
            } // TODO IoNumber => reject if posixly-correct,
        }

        let remove_tabs = match operator.id {
            Operator(LessLess) => false,
            Operator(LessLessDash) => true,
            _ => unreachable!("unhandled redirection operator type"),
        };
        self.memorize_unread_here_doc(PartialHereDoc {
            delimiter: operand.word,
            remove_tabs,
        });

        Ok(Some(Redir {
            fd: None,
            body: RedirBody::HereDoc(MissingHereDoc),
        }))
    }

    /// Parses a simple command.
    pub async fn simple_command(&mut self) -> Result<Rec<SimpleCommand<MissingHereDoc>>> {
        // TODO Return Option::None if the first token is not a normal word token.
        // TODO Support assignments.
        let mut words = vec![];
        let mut redirs = vec![];
        loop {
            if let Some(redir) = self.redirection().await? {
                redirs.push(redir);
                continue;
            }

            let token = self.peek_token().await?;
            if token.id != Token {
                break;
            }

            match self.take_token_aliased(words.is_empty()).await? {
                // TODO Also consider assignments.is_empty
                Rec::AliasSubstituted => {
                    if words.is_empty() && redirs.is_empty() {
                        return Ok(Rec::AliasSubstituted);
                    }
                }
                Rec::Parsed(token) => words.push(token.word),
            }
        }
        Ok(Rec::Parsed(SimpleCommand { words, redirs }))
    }

    /// Parses a command.
    pub async fn command(&mut self) -> Result<Rec<Command<MissingHereDoc>>> {
        // TODO compound command
        // TODO Function definition
        match self.simple_command().await? {
            Rec::AliasSubstituted => Ok(Rec::AliasSubstituted),
            Rec::Parsed(c) => Ok(Rec::Parsed(Command::SimpleCommand(c))),
        }
    }

    /// Parses a pipeline.
    pub async fn pipeline(&mut self) -> Result<Rec<Pipeline<MissingHereDoc>>> {
        // TODO Parse `!`
        // TODO Parse `|` and more commands
        let negation = false;
        match self.command().await? {
            Rec::AliasSubstituted => Ok(Rec::AliasSubstituted),
            Rec::Parsed(c) => Ok(Rec::Parsed(Pipeline {
                commands: vec![Rc::new(c)],
                negation,
            })),
        }
    }

    /// Parses an and-or list.
    pub async fn and_or_list(&mut self) -> Result<Rec<AndOrList<MissingHereDoc>>> {
        let first = match self.pipeline().await? {
            Rec::AliasSubstituted => return Ok(Rec::AliasSubstituted),
            Rec::Parsed(p) => p,
        };

        // TODO Parse `&&` and `||` and more pipelines
        Ok(Rec::Parsed(AndOrList {
            first,
            rest: vec![],
        }))
    }

    /// Parses an optional newline token and here-document contents.
    ///
    /// If the current token is a newline, it is consumed and any pending here-document contents
    /// are read starting from the next line. Otherwise, this function returns `Ok(false)` without
    /// any side effect.
    pub async fn newline_and_here_doc_contents(&mut self) -> Result<bool> {
        if self.peek_token().await?.id != Operator(Newline) {
            return Ok(false);
        }

        self.take_token().await?;
        self.here_doc_contents().await?;
        Ok(true)
    }

    // TODO Should return a vector of and-or lists
    /// Parses a complete command optionally delimited by a newline.
    ///
    /// A complete command is a minimal sequence of and-or lists that can be executed in the shell
    /// environment. This function reads as many lines as needed to compose the complete command.
    ///
    /// If the current line is empty (or containing only whitespaces and comments), the result is
    /// an empty vector. If the first token of the current line is the end of input, the result is
    /// `Ok(None)`.
    pub async fn command_line(&mut self) -> Result<Option<AndOrList>> {
        if self.peek_token().await?.id == EndOfInput {
            return Ok(None);
        }

        let cmd = loop {
            if let Rec::Parsed(cmd) = self.and_or_list().await? {
                break cmd;
            }
        };

        if !self.newline_and_here_doc_contents().await? {
            let next = self.peek_token().await?;
            if next.id != EndOfInput {
                // TODO Return a better error depending on the token id of the peeked token
                return Err(Error {
                    cause: ErrorCause::UnexpectedToken,
                    location: next.word.location.clone(),
                });
            }
        }

        self.ensure_no_unread_here_doc()?;
        Ok(Some(cmd.fill(&mut self.take_read_here_docs().into_iter())?))
    }

    // TODO Should return a vector of and-or lists
    /// Parses an optional compound list.
    ///
    /// A compound list is a sequence of one or more and-or lists that are
    /// separated by newlines and optionally preceded and/or followed by
    /// newlines.
    ///
    /// This function stops parsing on encountering an unexpected token that
    /// cannot be parsed as the beginning of an and-or list. The caller should
    /// check that the next token is an expected one.
    pub async fn maybe_compound_list(&mut self) -> Result<Vec<SimpleCommand<MissingHereDoc>>> {
        // TODO Parse leading and trailing newlines
        let cmd = loop {
            if let Rec::Parsed(cmd) = self.simple_command().await? {
                break cmd;
            }
        };
        self.newline_and_here_doc_contents().await?;
        Ok(vec![cmd])
    }
}

#[cfg(test)]
mod tests {
    use super::lex::Lexer;
    use super::*;
    use crate::source::Source;
    use futures::executor::block_on;

    #[test]
    fn parser_redirection_lessless() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<<end \nend\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        assert_eq!(redir.body, RedirBody::HereDoc(MissingHereDoc));

        block_on(parser.newline_and_here_doc_contents()).unwrap();
        let here_docs = parser.take_read_here_docs();
        assert_eq!(here_docs.len(), 1);
        assert_eq!(here_docs[0].delimiter.to_string(), "end");
        assert_eq!(here_docs[0].remove_tabs, false);
        assert_eq!(here_docs[0].content.to_string(), "");
    }

    #[test]
    fn parser_redirection_lesslessdash() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<<-end \nend\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        assert_eq!(redir.body, RedirBody::HereDoc(MissingHereDoc));

        block_on(parser.newline_and_here_doc_contents()).unwrap();
        let here_docs = parser.take_read_here_docs();
        assert_eq!(here_docs.len(), 1);
        assert_eq!(here_docs[0].delimiter.to_string(), "end");
        assert_eq!(here_docs[0].remove_tabs, true);
        assert_eq!(here_docs[0].content.to_string(), "");
    }

    #[test]
    fn parser_redirection_not_operator() {
        let mut lexer = Lexer::with_source(Source::Unknown, "x");
        let mut parser = Parser::new(&mut lexer);

        assert!(block_on(parser.redirection()).unwrap().is_none());
    }

    #[test]
    fn parser_redirection_not_heredoc_delimiter() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<< <<");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.redirection()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::MissingHereDocDelimiter);
        assert_eq!(e.location.line.value, "<< <<");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_redirection_eof_heredoc_delimiter() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<<");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.redirection()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::MissingHereDocDelimiter);
        assert_eq!(e.location.line.value, "<<");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    // TODO test simple_command
    // TODO test command
    // TODO test pipeline
    // TODO test and_or_list

    #[test]
    fn parser_command_line_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.command_line()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parser_command_line_command_and_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<<END\nfoo\nEND\n");
        let mut parser = Parser::new(&mut lexer);

        let AndOrList { first, rest } = block_on(parser.command_line()).unwrap().unwrap();
        assert!(rest.is_empty(), "expected empty rest: {:?}", rest);
        let Pipeline { commands, negation } = first;
        assert_eq!(negation, false);
        assert_eq!(commands.len(), 1);
        let cmd = match *commands[0] {
            Command::SimpleCommand(ref c) => c,
        };
        assert_eq!(cmd.words.len(), 0);
        assert_eq!(cmd.redirs.len(), 1);
        assert_eq!(cmd.redirs[0].fd, None);
        let RedirBody::HereDoc(ref here_doc) = cmd.redirs[0].body;
        //if let RedirBody::HereDoc(ref here_doc) = cmd.redirs[0].body {
        let HereDoc {
            delimiter,
            remove_tabs,
            content,
        } = here_doc;
        assert_eq!(delimiter.to_string(), "END");
        assert_eq!(*remove_tabs, false);
        assert_eq!(content.to_string(), "foo\n");
        //} else {
        //panic!("Expected here-document, but got {:?}", cmd.redirs[0].body);
        //}
    }

    #[test]
    fn parser_command_line_command_without_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo");
        let mut parser = Parser::new(&mut lexer);

        let cmd = block_on(parser.command_line()).unwrap().unwrap();
        assert_eq!(cmd.to_string(), "foo");
    }

    #[test]
    fn parser_command_line_newline_only() {
        let mut lexer = Lexer::with_source(Source::Unknown, "\n");
        let mut parser = Parser::new(&mut lexer);

        let AndOrList { first, rest } = block_on(parser.command_line()).unwrap().unwrap();
        assert!(rest.is_empty(), "expected empty rest: {:?}", rest);
        let Pipeline { commands, negation } = first;
        assert_eq!(negation, false);
        assert_eq!(commands.len(), 1);
        let cmd = match *commands[0] {
            Command::SimpleCommand(ref c) => c,
        };
        assert_eq!(cmd.words.len(), 0);
        assert_eq!(cmd.redirs.len(), 0);
    }

    #[test]
    fn parser_command_line_here_doc_without_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<<END");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.command_line()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::MissingHereDocContent);
        assert_eq!(e.location.line.value, "<<END");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 3);
    }

    #[test]
    fn parser_command_line_wrong_delimiter() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo)");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.command_line()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::UnexpectedToken);
        assert_eq!(e.location.line.value, "foo)");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 4);
    }
}
