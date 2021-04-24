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
mod fromstr;

pub mod lex;

use self::lex::keyword::Keyword::*;
use self::lex::Operator::*;
use self::lex::PartialHereDoc;
use self::lex::TokenId::*;
use super::syntax::*;
use crate::source::Location;
use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;

pub use self::core::AsyncFnMut;
pub use self::core::Error;
pub use self::core::Parser;
pub use self::core::Rec;
pub use self::core::Result;
pub use self::core::SyntaxError;
pub use self::fill::Fill;
pub use self::fill::MissingHereDoc;

impl Parser<'_> {
    /// Parses the value of an array assignment.
    ///
    /// This function first consumes a `(` token, then any number of words
    /// separated by blanks and/or newlines, and finally a `)`.
    /// If the first token is not `(`, the result is `Ok(None)`.
    /// If the last `)` is missing, the result is
    /// `Err(ErrorCause::Syntax(SyntaxError::UnclosedArrayValue(_)))`.
    pub async fn array_values(&mut self) -> Result<Option<Vec<Word>>> {
        if self.peek_token().await?.id != Operator(OpenParen) {
            return Ok(None);
        }

        let opening_location = self.take_token_raw().await?.word.location;
        let mut words = vec![];

        loop {
            let next = self.take_token_auto(&[]).await?;
            match next.id {
                Operator(Newline) => continue,
                Operator(CloseParen) => break,
                Token(_keyword) => words.push(next.word),
                _ => {
                    return Err(Error {
                        cause: SyntaxError::UnclosedArrayValue { opening_location }.into(),
                        location: next.word.location,
                    })
                }
            }
        }

        Ok(Some(words))
    }

    /// Parses the operand of a redirection operator.
    async fn redirection_operand(&mut self) -> Result<Option<Word>> {
        let operand = self.take_token_auto(&[]).await?;
        match operand.id {
            Token(_) => (),
            Operator(_) | EndOfInput => return Ok(None),
            IoNumber => (), // TODO reject if POSIXly-correct
        }
        Ok(Some(operand.word))
    }

    /// Parses a normal redirection body.
    async fn normal_redirection_body(
        &mut self,
        operator: RedirOp,
    ) -> Result<RedirBody<MissingHereDoc>> {
        // TODO reject >>| and <<< if POSIXly-correct
        let operator_location = self.take_token_raw().await?.word.location;
        let operand = self.redirection_operand().await?.ok_or(Error {
            cause: SyntaxError::MissingRedirOperand.into(),
            location: operator_location,
        })?;
        return Ok(RedirBody::Normal { operator, operand });
    }

    /// Parses the redirection body for a here-document.
    async fn here_doc_redirection_body(
        &mut self,
        remove_tabs: bool,
    ) -> Result<RedirBody<MissingHereDoc>> {
        let operator_location = self.take_token_raw().await?.word.location;
        let delimiter = self.redirection_operand().await?.ok_or(Error {
            cause: SyntaxError::MissingHereDocDelimiter.into(),
            location: operator_location,
        })?;

        self.memorize_unread_here_doc(PartialHereDoc {
            delimiter,
            remove_tabs,
        });

        Ok(RedirBody::HereDoc(MissingHereDoc))
    }

    /// Parses the redirection body.
    async fn redirection_body(&mut self) -> Result<Option<RedirBody<MissingHereDoc>>> {
        let operator = match self.peek_token().await?.id {
            Operator(operator) => operator,
            _ => return Ok(None),
        };

        if let Ok(operator) = RedirOp::try_from(operator) {
            return Ok(Some(self.normal_redirection_body(operator).await?));
        }
        match operator {
            LessLess => Ok(Some(self.here_doc_redirection_body(false).await?)),
            LessLessDash => Ok(Some(self.here_doc_redirection_body(true).await?)),
            // TODO <() >()
            _ => Ok(None),
        }
    }

    /// Parses a redirection.
    ///
    /// If the current token is not a redirection operator, `Ok(None)` is returned. If a word token
    /// is missing after the operator, `Err(Error{...})` is returned with a cause of
    /// [`MissingRedirOperand`](SyntaxError::MissingRedirOperand) or
    /// [`MissingHereDocDelimiter`](SyntaxError::MissingHereDocDelimiter).
    pub async fn redirection(&mut self) -> Result<Option<Redir<MissingHereDoc>>> {
        let fd = if self.peek_token().await?.id == IoNumber {
            let token = self.take_token_raw().await?;
            if let Ok(fd) = token.word.to_string().parse() {
                Some(fd)
            } else {
                return Err(Error {
                    cause: SyntaxError::FdOutOfRange.into(),
                    location: token.word.location,
                });
            }
        } else {
            None
        };

        Ok(self
            .redirection_body()
            .await?
            .map(|body| Redir { fd, body }))
    }

    /// Parses a (possibly empty) sequence of redirections.
    pub async fn redirections(&mut self) -> Result<Vec<Redir<MissingHereDoc>>> {
        // TODO substitute global aliases
        let mut redirs = vec![];
        while let Some(redir) = self.redirection().await? {
            redirs.push(redir);
        }
        Ok(redirs)
    }

    /// Parses a simple command.
    ///
    /// If there is no valid command at the current position, this function
    /// returns `Ok(Rec::Parsed(None))`.
    pub async fn simple_command(&mut self) -> Result<Rec<Option<SimpleCommand<MissingHereDoc>>>> {
        let mut result = SimpleCommand {
            assigns: vec![],
            words: vec![],
            redirs: vec![],
        };

        loop {
            // Parse redirection
            if let Some(redir) = self.redirection().await? {
                result.redirs.push(redir);
                continue;
            }

            // Filter token type
            match self.peek_token().await?.id {
                Token(Some(_keyword)) if result.is_empty() => break,
                Token(_) => (),
                _ => break,
            }

            // Apply alias substitution
            let token = match self.take_token_manual(result.words.is_empty()).await? {
                Rec::AliasSubstituted => {
                    if result.is_empty() {
                        return Ok(Rec::AliasSubstituted);
                    } else {
                        continue;
                    }
                }
                Rec::Parsed(token) => token,
            };

            // Tell assignment from word
            if !result.words.is_empty() {
                result.words.push(token.word);
                continue;
            }
            let mut assign = match Assign::try_from(token.word) {
                Ok(assign) => assign,
                Err(word) => {
                    result.words.push(word);
                    continue;
                }
            };

            let units = match &assign.value {
                Scalar(Word { units, .. }) => units,
                _ => panic!(
                    "Assign::try_from produced a non-scalar value {:?}",
                    assign.value
                ),
            };

            // Tell array assignment from scalar assignment
            // TODO no array assignment in POSIXly-correct mode
            if units.is_empty() && !self.has_blank().await? {
                if let Some(words) = self.array_values().await? {
                    assign.value = Array(words);
                }
            }

            result.assigns.push(assign);
        }

        Ok(Rec::Parsed(if result.is_empty() {
            None
        } else {
            Some(result)
        }))
    }

    /// Parses a normal grouping.
    ///
    /// The next token must be a `{`.
    ///
    /// # Panics
    ///
    /// If the first token is not a `{`.
    async fn grouping(&mut self) -> Result<CompoundCommand<MissingHereDoc>> {
        let open = self.take_token_raw().await?;
        assert_eq!(open.id, Token(Some(OpenBrace)));

        let list = self.maybe_compound_list_boxed().await?;

        let close = self.take_token_raw().await?;
        if close.id != Token(Some(CloseBrace)) {
            return Err(Error {
                cause: SyntaxError::UnclosedGrouping {
                    opening_location: open.word.location,
                }
                .into(),
                location: close.word.location,
            });
        }

        // TODO allow empty subshell if not POSIXly-correct
        if list.0.is_empty() {
            return Err(Error {
                cause: SyntaxError::EmptyGrouping.into(),
                location: open.word.location,
            });
        }

        Ok(CompoundCommand::Grouping(list))
    }

    /// Parses a subshell.
    ///
    /// The next token must be a `(`.
    ///
    /// # Panics
    ///
    /// If the first token is not a `(`.
    async fn subshell(&mut self) -> Result<CompoundCommand<MissingHereDoc>> {
        let open = self.take_token_raw().await?;
        assert_eq!(open.id, Operator(OpenParen));

        let list = self.maybe_compound_list_boxed().await?;

        let close = self.take_token_raw().await?;
        if close.id != Operator(CloseParen) {
            return Err(Error {
                cause: SyntaxError::UnclosedSubshell {
                    opening_location: open.word.location,
                }
                .into(),
                location: close.word.location,
            });
        }

        // TODO allow empty subshell if not POSIXly-correct
        if list.0.is_empty() {
            return Err(Error {
                cause: SyntaxError::EmptySubshell.into(),
                location: open.word.location,
            });
        }

        Ok(CompoundCommand::Subshell(list))
    }

    /// Parses a `do` clause, i.e., a compound list surrounded in `do ... done`.
    ///
    /// Returns `Ok(None)` if the first token is not `do`.
    async fn do_clause(&mut self) -> Result<Option<List<MissingHereDoc>>> {
        if self.peek_token().await?.id != Token(Some(Do)) {
            return Ok(None);
        }

        let open = self.take_token_raw().await?;

        let list = self.maybe_compound_list_boxed().await?;

        let close = self.take_token_raw().await?;
        if close.id != Token(Some(Done)) {
            return Err(Error {
                cause: SyntaxError::UnclosedDoClause {
                    opening_location: open.word.location,
                }
                .into(),
                location: close.word.location,
            });
        }

        // TODO allow empty do clause if not POSIXly-correct
        if list.0.is_empty() {
            return Err(Error {
                cause: SyntaxError::EmptyDoClause.into(),
                location: open.word.location,
            });
        }

        Ok(Some(list))
    }

    /// Parses the name of a for loop.
    async fn for_loop_name(&mut self) -> Result<Word> {
        let name = self.take_token_auto(&[]).await?;

        // Validate the token type
        match name.id {
            EndOfInput | Operator(Newline) | Operator(Semicolon) => {
                let cause = SyntaxError::MissingForName.into();
                let location = name.word.location;
                return Err(Error { cause, location });
            }
            Operator(_) => {
                let cause = SyntaxError::InvalidForName.into();
                let location = name.word.location;
                return Err(Error { cause, location });
            }
            Token(_) | IoNumber => (),
        }

        // TODO reject non-portable names in POSIXly-correct mode

        Ok(name.word)
    }

    /// Parses the values of a for loop.
    ///
    /// For the values to be parsed, the first token needs to be `in`. Otherwise,
    /// the result will be `None`.
    ///
    /// If successful, `opening_location` is returned intact as the second value
    /// of the tuple.
    async fn for_loop_values(
        &mut self,
        opening_location: Location,
    ) -> Result<(Option<Vec<Word>>, Location)> {
        // Parse the `in`
        let mut first_line = true;
        loop {
            match self.peek_token().await?.id {
                Operator(Semicolon) if first_line => {
                    self.take_token_raw().await?;
                    return Ok((None, opening_location));
                }
                Token(Some(Do)) => {
                    return Ok((None, opening_location));
                }
                Operator(Newline) => {
                    assert!(self.newline_and_here_doc_contents().await?);
                    first_line = false;
                }
                Token(Some(In)) => {
                    self.take_token_raw().await?;
                    break;
                }
                _ => match self.take_token_manual(false).await? {
                    Rec::AliasSubstituted => (),
                    Rec::Parsed(token) => {
                        let cause = SyntaxError::MissingForBody { opening_location }.into();
                        let location = token.word.location;
                        return Err(Error { cause, location });
                    }
                },
            }
        }

        // Parse values until a delimiter is found
        let mut values = Vec::new();
        loop {
            let next = self.take_token_auto(&[]).await?;
            match next.id {
                Token(_) | IoNumber => {
                    values.push(next.word);
                }
                Operator(Semicolon) | Operator(Newline) => {
                    return Ok((Some(values), opening_location));
                }
                _ => {
                    let cause = SyntaxError::InvalidForValue.into();
                    let location = next.word.location;
                    return Err(Error { cause, location });
                }
            }
        }
    }

    /// Parses the body of a for loop, possibly preceded by newlines.
    async fn for_loop_body(&mut self, opening_location: Location) -> Result<List<MissingHereDoc>> {
        loop {
            while self.newline_and_here_doc_contents().await? {}

            if let Some(body) = self.do_clause().await? {
                return Ok(body);
            }

            match self.take_token_manual(false).await? {
                Rec::AliasSubstituted => (),
                Rec::Parsed(token) => {
                    let cause = SyntaxError::MissingForBody { opening_location }.into();
                    let location = token.word.location;
                    return Err(Error { cause, location });
                }
            }
        }
    }

    /// Parses a for loop.
    ///
    /// The next token must be the `for` reserved word.
    ///
    /// # Panics
    ///
    /// If the first token is not `for`.
    async fn for_loop(&mut self) -> Result<CompoundCommand<MissingHereDoc>> {
        let open = self.take_token_raw().await?;
        assert_eq!(open.id, Token(Some(For)));
        let opening_location = open.word.location;

        let name = self.for_loop_name().await?;
        let (values, opening_location) = self.for_loop_values(opening_location).await?;
        let body = self.for_loop_body(opening_location).await?;
        Ok(CompoundCommand::For { name, values, body })
    }

    /// Parses a while loop.
    ///
    /// The next token must be the `while` reserved word.
    ///
    /// # Panics
    ///
    /// If the first token is not `while`.
    async fn while_loop(&mut self) -> Result<CompoundCommand<MissingHereDoc>> {
        let open = self.take_token_raw().await?;
        assert_eq!(open.id, Token(Some(While)));

        let condition = self.maybe_compound_list_boxed().await?;

        let body = match self.do_clause().await? {
            Some(body) => body,
            None => {
                return Err(Error {
                    cause: SyntaxError::UnclosedWhileClause {
                        opening_location: open.word.location,
                    }
                    .into(),
                    location: self.take_token_raw().await?.word.location,
                })
            }
        };

        // TODO allow empty condition if not POSIXly-correct
        if condition.0.is_empty() {
            return Err(Error {
                cause: SyntaxError::EmptyWhileCondition.into(),
                location: open.word.location,
            });
        }

        Ok(CompoundCommand::While { condition, body })
    }

    /// Parses an until loop.
    ///
    /// The next token must be the `until` reserved word.
    ///
    /// # Panics
    ///
    /// If the first token is not `until`.
    async fn until_loop(&mut self) -> Result<CompoundCommand<MissingHereDoc>> {
        let open = self.take_token_raw().await?;
        assert_eq!(open.id, Token(Some(Until)));

        let condition = self.maybe_compound_list_boxed().await?;

        let body = match self.do_clause().await? {
            Some(body) => body,
            None => {
                return Err(Error {
                    cause: SyntaxError::UnclosedUntilClause {
                        opening_location: open.word.location,
                    }
                    .into(),
                    location: self.take_token_raw().await?.word.location,
                })
            }
        };

        // TODO allow empty condition if not POSIXly-correct
        if condition.0.is_empty() {
            return Err(Error {
                cause: SyntaxError::EmptyUntilCondition.into(),
                location: open.word.location,
            });
        }

        Ok(CompoundCommand::Until { condition, body })
    }

    /// Parses a compound command.
    pub async fn compound_command(&mut self) -> Result<Option<CompoundCommand<MissingHereDoc>>> {
        match self.peek_token().await?.id {
            Token(Some(OpenBrace)) => self.grouping().await.map(Some),
            Operator(OpenParen) => self.subshell().await.map(Some),
            Token(Some(For)) => self.for_loop().await.map(Some),
            Token(Some(While)) => self.while_loop().await.map(Some),
            Token(Some(Until)) => self.until_loop().await.map(Some),
            _ => Ok(None),
        }
    }

    /// Parses a compound command with optional redirections.
    pub async fn full_compound_command(
        &mut self,
    ) -> Result<Option<FullCompoundCommand<MissingHereDoc>>> {
        let command = match self.compound_command().await? {
            Some(command) => command,
            None => return Ok(None),
        };
        let redirs = self.redirections().await?;
        // TODO Reject `{ { :; } >foo }` and `{ ( : ) }` if POSIXly-correct
        // (The last `}` is not regarded as a keyword in these cases.)
        Ok(Some(FullCompoundCommand { command, redirs }))
    }

    /// Parses a function definition command that does not start with the
    /// `function` reserved word.
    ///
    /// This function must be called just after a [simple
    /// command](Self::simple_command) has been parsed.
    /// The simple command must be passed as an argument.
    /// If the simple command has only one word and the next token is `(`, it is
    /// parsed as a function definition command.
    /// Otherwise, the simple command is returned intact.
    pub async fn short_function_definition(
        &mut self,
        mut intro: SimpleCommand<MissingHereDoc>,
    ) -> Result<Command<MissingHereDoc>> {
        if !intro.is_one_word() || self.peek_token().await?.id != Operator(OpenParen) {
            return Ok(Command::Simple(intro));
        }

        let open = self.take_token_raw().await?;
        debug_assert_eq!(open.id, Operator(OpenParen));

        let close = self.take_token_auto(&[]).await?;
        if close.id != Operator(CloseParen) {
            return Err(Error {
                cause: SyntaxError::UnmatchedParenthesis.into(),
                location: close.word.location,
            });
        }

        let name = intro.words.pop().unwrap();
        debug_assert!(intro.is_empty());
        // TODO reject invalid name if POSIXly-correct

        loop {
            while self.newline_and_here_doc_contents().await? {}

            return match self.full_compound_command().await? {
                Some(body) => Ok(Command::Function(FunctionDefinition {
                    has_keyword: false,
                    name,
                    body,
                })),
                None => {
                    let next = match self.take_token_manual(false).await? {
                        Rec::AliasSubstituted => continue,
                        Rec::Parsed(next) => next,
                    };
                    let cause = if let Token(_) = next.id {
                        SyntaxError::InvalidFunctionBody.into()
                    } else {
                        SyntaxError::MissingFunctionBody.into()
                    };
                    let location = next.word.location;
                    Err(Error { cause, location })
                }
            };
        }
    }

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

    /// Parses a pipeline.
    ///
    /// If there is no valid pipeline at the current position, this function
    /// returns `Ok(Rec::Parsed(None))`.
    pub async fn pipeline(&mut self) -> Result<Rec<Option<Pipeline<MissingHereDoc>>>> {
        // Parse the first command
        let (first, negation) = match self.command().await? {
            Rec::AliasSubstituted => return Ok(Rec::AliasSubstituted),
            Rec::Parsed(Some(first)) => (first, false),
            Rec::Parsed(None) => {
                // Parse the `!` reserved word
                if let Token(Some(Bang)) = self.peek_token().await?.id {
                    let location = self.take_token_raw().await?.word.location;
                    loop {
                        // Parse the command after the `!`
                        if let Rec::Parsed(option) = self.command().await? {
                            if let Some(first) = option {
                                break (first, true);
                            }

                            // Error: the command is missing
                            let next = self.peek_token().await?;
                            let cause = if next.id == Token(Some(Bang)) {
                                SyntaxError::DoubleNegation.into()
                            } else {
                                SyntaxError::MissingCommandAfterBang.into()
                            };
                            return Err(Error { cause, location });
                        }
                    }
                } else {
                    return Ok(Rec::Parsed(None));
                }
            }
        };

        // Parse `|`
        let mut commands = vec![first];
        while self.peek_token().await?.id == Operator(Bar) {
            let bar_location = self.take_token_raw().await?.word.location;

            while self.newline_and_here_doc_contents().await? {}

            commands.push(loop {
                // Parse the next command
                if let Rec::Parsed(option) = self.command().await? {
                    if let Some(next) = option {
                        break next;
                    }

                    // Error: the command is missing
                    let next = self.peek_token().await?;
                    return if next.id == Token(Some(Bang)) {
                        Err(Error {
                            cause: SyntaxError::BangAfterBar.into(),
                            location: next.word.location.clone(),
                        })
                    } else {
                        Err(Error {
                            cause: SyntaxError::MissingCommandAfterBar.into(),
                            location: bar_location,
                        })
                    };
                }
            });
        }

        Ok(Rec::Parsed(Some(Pipeline { commands, negation })))
    }

    /// Parses an and-or list.
    ///
    /// If there is no valid and-or list at the current position, this function
    /// returns `Ok(Rec::Parsed(None))`.
    pub async fn and_or_list(&mut self) -> Result<Rec<Option<AndOrList<MissingHereDoc>>>> {
        let first = match self.pipeline().await? {
            Rec::AliasSubstituted => return Ok(Rec::AliasSubstituted),
            Rec::Parsed(None) => return Ok(Rec::Parsed(None)),
            Rec::Parsed(Some(p)) => p,
        };

        let mut rest = vec![];
        loop {
            let condition = match self.peek_token().await?.id {
                Operator(AndAnd) => AndOr::AndThen,
                Operator(BarBar) => AndOr::OrElse,
                _ => break,
            };
            self.take_token_raw().await?;

            while self.newline_and_here_doc_contents().await? {}

            let maybe_pipeline = loop {
                if let Rec::Parsed(maybe_pipeline) = self.pipeline().await? {
                    break maybe_pipeline;
                }
            };
            let pipeline = match maybe_pipeline {
                None => {
                    let cause = SyntaxError::MissingPipeline(condition).into();
                    let location = self.peek_token().await?.word.location.clone();
                    return Err(Error { cause, location });
                }
                Some(pipeline) => pipeline,
            };

            rest.push((condition, pipeline));
        }

        Ok(Rec::Parsed(Some(AndOrList { first, rest })))
    }

    // There is no function that parses a single item because it would not be
    // very useful for parsing a list. An item requires a separator operator
    // ('&' or ';') for it to be followed by another item. You cannot tell from
    // the resultant item whether there was a separator operator.
    // pub async fn item(&mut self) -> Result<Rec<Item<MissingHereDoc>>> { }

    /// Parses a list.
    ///
    /// This function parses a sequence of and-or lists that are separated by `;`
    /// or `&`. A newline token that delimits the list is not parsed.
    ///
    /// If there is no valid command at the current position, this function
    /// returns a list with no items.
    pub async fn list(&mut self) -> Result<Rec<List<MissingHereDoc>>> {
        let mut items = vec![];

        let mut result = match self.and_or_list().await? {
            Rec::AliasSubstituted => return Ok(Rec::AliasSubstituted),
            Rec::Parsed(result) => result,
        };

        while let Some(and_or) = result {
            let (is_async, next) = match self.peek_token().await?.id {
                Operator(Semicolon) => (false, true),
                Operator(And) => (true, true),
                _ => (false, false),
            };

            items.push(Item { and_or, is_async });

            if !next {
                break;
            }
            self.take_token_raw().await?;

            result = loop {
                if let Rec::Parsed(result) = self.and_or_list().await? {
                    break result;
                }
            };
        }

        Ok(Rec::Parsed(List(items)))
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

        self.take_token_raw().await?;
        self.here_doc_contents().await?;
        Ok(true)
    }

    /// Parses a complete command optionally delimited by a newline.
    ///
    /// A complete command is a minimal sequence of and-or lists that can be executed in the shell
    /// environment. This function reads as many lines as needed to compose the complete command.
    ///
    /// If the current line is empty (or containing only whitespaces and comments), the result is
    /// an empty list. If the first token of the current line is the end of input, the result is
    /// `Ok(None)`.
    pub async fn command_line(&mut self) -> Result<Option<List>> {
        let list = loop {
            if let Rec::Parsed(list) = self.list().await? {
                break list;
            }
        };

        if !self.newline_and_here_doc_contents().await? {
            let next = self.peek_token().await?;
            if next.id != EndOfInput {
                // TODO Return a better error depending on the token id of the peeked token
                return Err(Error {
                    cause: SyntaxError::UnexpectedToken.into(),
                    location: next.word.location.clone(),
                });
            }
            if list.0.is_empty() {
                return Ok(None);
            }
        }

        self.ensure_no_unread_here_doc()?;
        let mut here_docs = self.take_read_here_docs().into_iter();
        let list = list.fill(&mut here_docs)?;
        Ok(Some(list))
    }

    /// Parses an optional compound list.
    ///
    /// A compound list is a sequence of one or more and-or lists that are
    /// separated by newlines and optionally preceded and/or followed by
    /// newlines.
    ///
    /// This function stops parsing on encountering an unexpected token that
    /// cannot be parsed as the beginning of an and-or list. The caller should
    /// check that the next token is an expected one.
    pub async fn maybe_compound_list(&mut self) -> Result<List<MissingHereDoc>> {
        let mut items = vec![];

        loop {
            let list = loop {
                if let Rec::Parsed(list) = self.list().await? {
                    break list;
                }
            };
            items.extend(list.0);

            if !self.newline_and_here_doc_contents().await? {
                break;
            }
        }

        Ok(List(items))
    }

    /// Like [`maybe_compound_list`](Self::maybe_compound_list), but returns the future in a pinned box.
    pub fn maybe_compound_list_boxed(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<List<MissingHereDoc>>> + '_>> {
        Box::pin(self.maybe_compound_list())
    }
}

#[cfg(test)]
mod tests {
    use super::core::ErrorCause;
    use super::lex::Lexer;
    use super::*;
    use crate::alias::{AliasSet, HashEntry};
    use crate::source::{Location, Source};
    use futures::executor::block_on;

    #[test]
    fn parser_array_values_no_open_parenthesis() {
        let mut lexer = Lexer::with_source(Source::Unknown, ")");
        let mut parser = Parser::new(&mut lexer);
        let result = block_on(parser.array_values()).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parser_array_values_empty() {
        let mut lexer = Lexer::with_source(Source::Unknown, "()");
        let mut parser = Parser::new(&mut lexer);
        let words = block_on(parser.array_values()).unwrap().unwrap();
        assert_eq!(words, []);

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_array_values_many() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(a b c)");
        let mut parser = Parser::new(&mut lexer);
        let words = block_on(parser.array_values()).unwrap().unwrap();
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].to_string(), "a");
        assert_eq!(words[1].to_string(), "b");
        assert_eq!(words[2].to_string(), "c");
    }

    #[test]
    fn parser_array_values_newlines_and_comments() {
        let mut lexer = Lexer::with_source(
            Source::Unknown,
            "(
            a # b
            c d
        )",
        );
        let mut parser = Parser::new(&mut lexer);
        let words = block_on(parser.array_values()).unwrap().unwrap();
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].to_string(), "a");
        assert_eq!(words[1].to_string(), "c");
        assert_eq!(words[2].to_string(), "d");
    }

    #[test]
    fn parser_array_values_unclosed() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(a b");
        let mut parser = Parser::new(&mut lexer);
        let e = block_on(parser.array_values()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedArrayValue { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, "(a b");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 1);
        } else {
            panic!("Unexpected cause {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, "(a b");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 5);
    }

    #[test]
    fn parser_array_values_invalid_word() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(a;b)");
        let mut parser = Parser::new(&mut lexer);
        let e = block_on(parser.array_values()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedArrayValue { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, "(a;b)");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 1);
        } else {
            panic!("Unexpected cause {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, "(a;b)");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 3);
    }

    #[test]
    fn parser_redirection_less() {
        let mut lexer = Lexer::with_source(Source::Unknown, "</dev/null\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FileIn);
            assert_eq!(operand.to_string(), "/dev/null")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, Operator(Newline));
    }

    #[test]
    fn parser_redirection_less_greater() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<> /dev/null\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FileInOut);
            assert_eq!(operand.to_string(), "/dev/null")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_greater() {
        let mut lexer = Lexer::with_source(Source::Unknown, ">/dev/null\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FileOut);
            assert_eq!(operand.to_string(), "/dev/null")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_greater_greater() {
        let mut lexer = Lexer::with_source(Source::Unknown, " >> /dev/null\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FileAppend);
            assert_eq!(operand.to_string(), "/dev/null")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_greater_bar() {
        let mut lexer = Lexer::with_source(Source::Unknown, ">| /dev/null\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FileClobber);
            assert_eq!(operand.to_string(), "/dev/null")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_less_and() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<& -\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FdIn);
            assert_eq!(operand.to_string(), "-")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_greater_and() {
        let mut lexer = Lexer::with_source(Source::Unknown, ">& 3\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FdOut);
            assert_eq!(operand.to_string(), "3")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_greater_greater_bar() {
        let mut lexer = Lexer::with_source(Source::Unknown, ">>| 3\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::Pipe);
            assert_eq!(operand.to_string(), "3")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_less_less_less() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<<< foo\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, None);
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::String);
            assert_eq!(operand.to_string(), "foo")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }
    }

    #[test]
    fn parser_redirection_less_less() {
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
    fn parser_redirection_less_less_dash() {
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
    fn parser_redirection_with_io_number() {
        let mut lexer = Lexer::with_source(Source::Unknown, "12< /dev/null\n");
        let mut parser = Parser::new(&mut lexer);

        let redir = block_on(parser.redirection()).unwrap().unwrap();
        assert_eq!(redir.fd, Some(12));
        if let RedirBody::Normal { operator, operand } = redir.body {
            assert_eq!(operator, RedirOp::FileIn);
            assert_eq!(operand.to_string(), "/dev/null")
        } else {
            panic!("Unexpected redirection body {:?}", redir.body);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, Operator(Newline));
    }

    #[test]
    fn parser_redirection_fd_out_of_range() {
        let mut lexer = Lexer::with_source(
            Source::Unknown,
            "9999999999999999999999999999999999999999< x",
        );
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.redirection()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::FdOutOfRange));
        assert_eq!(
            e.location.line.value,
            "9999999999999999999999999999999999999999< x"
        );
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_redirection_not_operator() {
        let mut lexer = Lexer::with_source(Source::Unknown, "x");
        let mut parser = Parser::new(&mut lexer);

        assert!(block_on(parser.redirection()).unwrap().is_none());
    }

    #[test]
    fn parser_redirection_non_word_operand() {
        let mut lexer = Lexer::with_source(Source::Unknown, " < >");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.redirection()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingRedirOperand)
        );
        assert_eq!(e.location.line.value, " < >");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 2);
    }

    #[test]
    fn parser_redirection_eof_operand() {
        let mut lexer = Lexer::with_source(Source::Unknown, "  < ");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.redirection()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingRedirOperand)
        );
        assert_eq!(e.location.line.value, "  < ");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 3);
    }

    #[test]
    fn parser_redirection_not_heredoc_delimiter() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<< <<");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.redirection()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingHereDocDelimiter)
        );
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
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingHereDocDelimiter)
        );
        assert_eq!(e.location.line.value, "<<");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_simple_command_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let mut parser = Parser::new(&mut lexer);

        let option = block_on(parser.simple_command()).unwrap().unwrap();
        assert_eq!(option, None);
    }

    #[test]
    fn parser_simple_command_keyword() {
        let mut lexer = Lexer::with_source(Source::Unknown, "then");
        let mut parser = Parser::new(&mut lexer);

        let option = block_on(parser.simple_command()).unwrap().unwrap();
        assert_eq!(option, None);
    }

    #[test]
    fn parser_simple_command_one_assignment() {
        let mut lexer = Lexer::with_source(Source::Unknown, "my=assignment");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.words, []);
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.assigns.len(), 1);
        assert_eq!(sc.assigns[0].name, "my");
        assert_eq!(sc.assigns[0].value.to_string(), "assignment");
        assert_eq!(sc.assigns[0].location.line.value, "my=assignment");
        assert_eq!(sc.assigns[0].location.line.number.get(), 1);
        assert_eq!(sc.assigns[0].location.line.source, Source::Unknown);
        assert_eq!(sc.assigns[0].location.column.get(), 1);
    }

    #[test]
    fn parser_simple_command_many_assignments() {
        let mut lexer = Lexer::with_source(Source::Unknown, "a= b=! c=X");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.words, []);
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.assigns.len(), 3);
        assert_eq!(sc.assigns[0].name, "a");
        assert_eq!(sc.assigns[0].value.to_string(), "");
        assert_eq!(sc.assigns[0].location.line.value, "a= b=! c=X");
        assert_eq!(sc.assigns[0].location.line.number.get(), 1);
        assert_eq!(sc.assigns[0].location.line.source, Source::Unknown);
        assert_eq!(sc.assigns[0].location.column.get(), 1);
        assert_eq!(sc.assigns[1].name, "b");
        assert_eq!(sc.assigns[1].value.to_string(), "!");
        assert_eq!(sc.assigns[1].location.line.value, "a= b=! c=X");
        assert_eq!(sc.assigns[1].location.line.number.get(), 1);
        assert_eq!(sc.assigns[1].location.line.source, Source::Unknown);
        assert_eq!(sc.assigns[1].location.column.get(), 4);
        assert_eq!(sc.assigns[2].name, "c");
        assert_eq!(sc.assigns[2].value.to_string(), "X");
        assert_eq!(sc.assigns[2].location.line.value, "a= b=! c=X");
        assert_eq!(sc.assigns[2].location.line.number.get(), 1);
        assert_eq!(sc.assigns[2].location.line.source, Source::Unknown);
        assert_eq!(sc.assigns[2].location.column.get(), 8);
    }

    #[test]
    fn parser_simple_command_one_word() {
        let mut lexer = Lexer::with_source(Source::Unknown, "word");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns, []);
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.words.len(), 1);
        assert_eq!(sc.words[0].to_string(), "word");
    }

    #[test]
    fn parser_simple_command_many_words() {
        let mut lexer = Lexer::with_source(Source::Unknown, ": if then");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns, []);
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.words.len(), 3);
        assert_eq!(sc.words[0].to_string(), ":");
        assert_eq!(sc.words[1].to_string(), "if");
        assert_eq!(sc.words[2].to_string(), "then");
    }

    #[test]
    fn parser_simple_command_one_redirection() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<foo");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns, []);
        assert_eq!(sc.words, []);
        assert_eq!(sc.redirs.len(), 1);
        assert_eq!(sc.redirs[0].fd, None);
        if let RedirBody::Normal {
            ref operator,
            ref operand,
        } = sc.redirs[0].body
        {
            assert_eq!(operator, &RedirOp::FileIn);
            assert_eq!(operand.to_string(), "foo")
        } else {
            panic!("Unexpected redirection body {:?}", sc.redirs[0].body);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_simple_command_many_redirections() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<one >two >>three");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns, []);
        assert_eq!(sc.words, []);
        assert_eq!(sc.redirs.len(), 3);
        assert_eq!(sc.redirs[0].fd, None);
        if let RedirBody::Normal {
            ref operator,
            ref operand,
        } = sc.redirs[0].body
        {
            assert_eq!(operator, &RedirOp::FileIn);
            assert_eq!(operand.to_string(), "one")
        } else {
            panic!("Unexpected redirection body {:?}", sc.redirs[0].body);
        }
        assert_eq!(sc.redirs[1].fd, None);
        if let RedirBody::Normal {
            ref operator,
            ref operand,
        } = sc.redirs[1].body
        {
            assert_eq!(operator, &RedirOp::FileOut);
            assert_eq!(operand.to_string(), "two")
        } else {
            panic!("Unexpected redirection body {:?}", sc.redirs[1].body);
        }
        assert_eq!(sc.redirs[2].fd, None);
        if let RedirBody::Normal {
            ref operator,
            ref operand,
        } = sc.redirs[2].body
        {
            assert_eq!(operator, &RedirOp::FileAppend);
            assert_eq!(operand.to_string(), "three")
        } else {
            panic!("Unexpected redirection body {:?}", sc.redirs[2].body);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_simple_command_assignment_word() {
        let mut lexer = Lexer::with_source(Source::Unknown, "if=then else");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.assigns.len(), 1);
        assert_eq!(sc.words.len(), 1);
        assert_eq!(sc.assigns[0].name, "if");
        assert_eq!(sc.assigns[0].value.to_string(), "then");
        assert_eq!(sc.words[0].to_string(), "else");
    }

    #[test]
    fn parser_simple_command_word_redirection() {
        let mut lexer = Lexer::with_source(Source::Unknown, "word <redirection");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns, []);
        assert_eq!(sc.words.len(), 1);
        assert_eq!(sc.redirs.len(), 1);
        assert_eq!(sc.words[0].to_string(), "word");
        assert_eq!(sc.redirs[0].fd, None);
        if let RedirBody::Normal {
            ref operator,
            ref operand,
        } = sc.redirs[0].body
        {
            assert_eq!(operator, &RedirOp::FileIn);
            assert_eq!(operand.to_string(), "redirection")
        } else {
            panic!("Unexpected redirection body {:?}", sc.redirs[0].body);
        }
    }

    #[test]
    fn parser_simple_command_redirection_assignment() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<foo a=b");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.words, []);
        assert_eq!(sc.assigns.len(), 1);
        assert_eq!(sc.redirs.len(), 1);
        assert_eq!(sc.assigns[0].name, "a");
        assert_eq!(sc.assigns[0].value.to_string(), "b");
        assert_eq!(sc.redirs[0].fd, None);
        if let RedirBody::Normal {
            ref operator,
            ref operand,
        } = sc.redirs[0].body
        {
            assert_eq!(operator, &RedirOp::FileIn);
            assert_eq!(operand.to_string(), "foo")
        } else {
            panic!("Unexpected redirection body {:?}", sc.redirs[0].body);
        }
    }

    #[test]
    fn parser_simple_command_assignment_redirection_word() {
        let mut lexer = Lexer::with_source(Source::Unknown, "if=then <foo else");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns.len(), 1);
        assert_eq!(sc.words.len(), 1);
        assert_eq!(sc.redirs.len(), 1);
        assert_eq!(sc.assigns[0].name, "if");
        assert_eq!(sc.assigns[0].value.to_string(), "then");
        assert_eq!(sc.words[0].to_string(), "else");
        assert_eq!(sc.redirs[0].fd, None);
        if let RedirBody::Normal {
            ref operator,
            ref operand,
        } = sc.redirs[0].body
        {
            assert_eq!(operator, &RedirOp::FileIn);
            assert_eq!(operand.to_string(), "foo")
        } else {
            panic!("Unexpected redirection body {:?}", sc.redirs[0].body);
        }
    }

    #[test]
    fn parser_simple_command_array_assignment() {
        let mut lexer = Lexer::with_source(Source::Unknown, "a=()");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns.len(), 1);
        assert_eq!(sc.words, []);
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.assigns[0].name, "a");
        if let Array(words) = &sc.assigns[0].value {
            assert_eq!(words, &[]);
        } else {
            panic!("Non-array assignment value {:?}", sc.assigns[0].value);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_simple_command_empty_assignment_followed_by_blank_and_parenthesis() {
        let mut lexer = Lexer::with_source(Source::Unknown, "a= ()");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns.len(), 1);
        assert_eq!(sc.words, []);
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.assigns[0].name, "a");
        assert_eq!(sc.assigns[0].value.to_string(), "");
        assert_eq!(sc.assigns[0].location.line.value, "a= ()");
        assert_eq!(sc.assigns[0].location.line.number.get(), 1);
        assert_eq!(sc.assigns[0].location.line.source, Source::Unknown);
        assert_eq!(sc.assigns[0].location.column.get(), 1);

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, Operator(OpenParen));
    }

    #[test]
    fn parser_simple_command_non_empty_assignment_followed_by_parenthesis() {
        let mut lexer = Lexer::with_source(Source::Unknown, "a=b()");
        let mut parser = Parser::new(&mut lexer);

        let sc = block_on(parser.simple_command()).unwrap().unwrap().unwrap();
        assert_eq!(sc.assigns.len(), 1);
        assert_eq!(sc.words, []);
        assert_eq!(sc.redirs, []);
        assert_eq!(sc.assigns[0].name, "a");
        assert_eq!(sc.assigns[0].value.to_string(), "b");
        assert_eq!(sc.assigns[0].location.line.value, "a=b()");
        assert_eq!(sc.assigns[0].location.line.number.get(), 1);
        assert_eq!(sc.assigns[0].location.line.source, Source::Unknown);
        assert_eq!(sc.assigns[0].location.column.get(), 1);

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, Operator(OpenParen));
    }

    #[test]
    fn parser_grouping_short() {
        let mut lexer = Lexer::with_source(Source::Unknown, "{ :; }");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::Grouping(list) = result {
            assert_eq!(list.to_string(), ":");
        } else {
            panic!("Not a grouping: {:?}", result);
        }
    }

    #[test]
    fn parser_grouping_long() {
        let mut lexer = Lexer::with_source(Source::Unknown, "{ foo; bar& }");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::Grouping(list) = result {
            assert_eq!(list.to_string(), "foo; bar&");
        } else {
            panic!("Not a grouping: {:?}", result);
        }
    }

    #[test]
    fn parser_grouping_unclosed() {
        let mut lexer = Lexer::with_source(Source::Unknown, " { oh no ");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedGrouping { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, " { oh no ");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 2);
        } else {
            panic!("Wrong error cause: {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, " { oh no ");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 10);
    }

    #[test]
    fn parser_grouping_empty_posix() {
        let mut lexer = Lexer::with_source(Source::Unknown, "{ }");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::EmptyGrouping));
        assert_eq!(e.location.line.value, "{ }");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_grouping_aliasing() {
        let mut lexer = Lexer::with_source(Source::Unknown, " { :; end ");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "{".to_string(),
            "".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "}".to_string(),
            "".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "end".to_string(),
            "}".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::Grouping(list) = result {
            assert_eq!(list.to_string(), ":");
        } else {
            panic!("Not a grouping: {:?}", result);
        }
    }

    #[test]
    fn parser_subshell_short() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(:)");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::Subshell(list) = result {
            assert_eq!(list.to_string(), ":");
        } else {
            panic!("Not a subshell: {:?}", result);
        }
    }

    #[test]
    fn parser_subshell_long() {
        let mut lexer = Lexer::with_source(Source::Unknown, "( foo& bar; )");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::Subshell(list) = result {
            assert_eq!(list.to_string(), "foo& bar");
        } else {
            panic!("Not a subshell: {:?}", result);
        }
    }

    #[test]
    fn parser_subshell_unclosed() {
        let mut lexer = Lexer::with_source(Source::Unknown, " ( oh no");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedSubshell { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, " ( oh no");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 2);
        } else {
            panic!("Wrong error cause: {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, " ( oh no");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 9);
    }

    #[test]
    fn parser_subshell_empty_posix() {
        let mut lexer = Lexer::with_source(Source::Unknown, "( )");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::EmptySubshell));
        assert_eq!(e.location.line.value, "( )");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_do_clause_none() {
        let mut lexer = Lexer::with_source(Source::Unknown, "done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.do_clause()).unwrap();
        assert!(result.is_none(), "result should be none: {:?}", result);
    }

    #[test]
    fn parser_do_clause_short() {
        let mut lexer = Lexer::with_source(Source::Unknown, "do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.do_clause()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(result.to_string(), ":");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_do_clause_long() {
        let mut lexer = Lexer::with_source(Source::Unknown, "do foo; bar& done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.do_clause()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(result.to_string(), "foo; bar&");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_do_clause_unclosed() {
        let mut lexer = Lexer::with_source(Source::Unknown, " do not close ");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.do_clause()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedDoClause { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, " do not close ");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 2);
        } else {
            panic!("Wrong error cause: {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, " do not close ");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 15);
    }

    #[test]
    fn parser_do_clause_empty_posix() {
        let mut lexer = Lexer::with_source(Source::Unknown, "do done");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.do_clause()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::EmptyDoClause));
        assert_eq!(e.location.line.value, "do done");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_do_clause_aliasing() {
        let mut lexer = Lexer::with_source(Source::Unknown, " do :; end ");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "do".to_string(),
            "".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "done".to_string(),
            "".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "end".to_string(),
            "done".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let result = block_on(parser.do_clause()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(result.to_string(), ":");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_short() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for A do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "A");
            assert_eq!(values, None);
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_with_semicolon_before_do() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for B ; do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "B");
            assert_eq!(values, None);
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_with_semicolon_and_newlines_before_do() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for B ; \n\t\n do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "B");
            assert_eq!(values, None);
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_with_newlines_before_do() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for B \n \\\n \n do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "B");
            assert_eq!(values, None);
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_with_zero_values_delimited_by_semicolon() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for foo in; do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "foo");
            assert_eq!(values, Some(vec![]));
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_with_one_value_delimited_by_semicolon_and_newlines() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for foo in bar; \n \n do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "foo");
            let values = values
                .unwrap()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>();
            assert_eq!(values, vec!["bar"]);
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_with_many_values_delimited_by_one_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for in in in a b c\ndo :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "in");
            let values = values
                .unwrap()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>();
            assert_eq!(values, vec!["in", "a", "b", "c"]);
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_with_zero_values_delimited_by_many_newlines() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for foo in \n \n \n do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "foo");
            assert_eq!(values, Some(vec![]));
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_newlines_before_in() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for foo\n \n\nin\ndo :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::For { name, values, body } = result {
            assert_eq!(name.to_string(), "foo");
            assert_eq!(values, Some(vec![]));
            assert_eq!(body.to_string(), ":")
        } else {
            panic!("Not a for loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_aliasing_on_semicolon() {
        let mut lexer = Lexer::with_source(Source::Unknown, " FOR_A if :; done");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "if".to_string(),
            " ;\n\ndo".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "FOR_A".to_string(),
            "for A ".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let first_pass = block_on(parser.take_token_manual(true)).unwrap();
        assert!(first_pass.is_alias_substituted());

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(result.to_string(), "for A do :; done");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_aliasing_on_do() {
        let mut lexer = Lexer::with_source(Source::Unknown, " FOR_A if :; done");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "if".to_string(),
            "\ndo".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "FOR_A".to_string(),
            "for A ".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let first_pass = block_on(parser.take_token_manual(true)).unwrap();
        assert!(first_pass.is_alias_substituted());

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(result.to_string(), "for A do :; done");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_for_loop_missing_name_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, " for ");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::MissingForName));
        assert_eq!(e.location.line.value, " for ");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 6);
    }

    #[test]
    fn parser_for_loop_missing_name_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, " for\ndo :; done");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::MissingForName));
        assert_eq!(e.location.line.value, " for\n");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 5);
    }

    #[test]
    fn parser_for_loop_missing_name_semicolon() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for; do :; done");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::MissingForName));
        assert_eq!(e.location.line.value, "for; do :; done");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 4);
    }

    #[test]
    fn parser_for_loop_invalid_name() {
        // Alias substitution results in "for & do :; done"
        let mut lexer = Lexer::with_source(Source::Unknown, "FOR if do :; done");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "FOR".to_string(),
            "for ".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "if".to_string(),
            "&".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let first_pass = block_on(parser.take_token_manual(true)).unwrap();
        assert!(first_pass.is_alias_substituted());

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::InvalidForName));
        assert_eq!(e.location.line.value, "&");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.column.get(), 1);
        if let Source::Alias { original, alias } = &e.location.line.source {
            assert_eq!(original.line.value, "FOR if do :; done");
            assert_eq!(original.line.number.get(), 1);
            assert_eq!(original.line.source, Source::Unknown);
            assert_eq!(original.column.get(), 5);
            assert_eq!(alias.name, "if");
        } else {
            panic!("Not an alias: {:?}", e.location.line.source);
        }
    }

    #[test]
    fn parser_for_loop_semicolon_after_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, "for X\n; do :; done");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::MissingForBody { opening_location }) = &e.cause {
            assert_eq!(opening_location.line.value, "for X\n");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 1);
        } else {
            panic!("Not MissingForBody: {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, "; do :; done");
        assert_eq!(e.location.line.number.get(), 2);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_for_loop_invalid_values_delimiter() {
        // Alias substitution results in "for A in a b & c; do :; done"
        let mut lexer = Lexer::with_source(Source::Unknown, "for_A_in_a_b if c; do :; done");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "for_A_in_a_b".to_string(),
            "for A in a b ".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "if".to_string(),
            "&".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let first_pass = block_on(parser.take_token_manual(true)).unwrap();
        assert!(first_pass.is_alias_substituted());

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::InvalidForValue));
        assert_eq!(e.location.line.value, "&");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.column.get(), 1);
        if let Source::Alias { original, alias } = &e.location.line.source {
            assert_eq!(original.line.value, "for_A_in_a_b if c; do :; done");
            assert_eq!(original.line.number.get(), 1);
            assert_eq!(original.line.source, Source::Unknown);
            assert_eq!(original.column.get(), 14);
            assert_eq!(alias.name, "if");
        } else {
            panic!("Not an alias: {:?}", e.location.line.source);
        }
    }

    #[test]
    fn parser_for_loop_invalid_token_after_semicolon() {
        let mut lexer = Lexer::with_source(Source::Unknown, " for X; ! do :; done");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::MissingForBody { opening_location }) = &e.cause {
            assert_eq!(opening_location.line.value, " for X; ! do :; done");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 2);
        } else {
            panic!("Not MissingForBody: {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, " for X; ! do :; done");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 9);
    }

    #[test]
    fn parser_while_loop_short() {
        let mut lexer = Lexer::with_source(Source::Unknown, "while true; do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::While { condition, body } = result {
            assert_eq!(condition.to_string(), "true");
            assert_eq!(body.to_string(), ":");
        } else {
            panic!("Not a while loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_while_loop_long() {
        let mut lexer = Lexer::with_source(Source::Unknown, "while false; true& do foo; bar& done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::While { condition, body } = result {
            assert_eq!(condition.to_string(), "false; true&");
            assert_eq!(body.to_string(), "foo; bar&");
        } else {
            panic!("Not a while loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_while_loop_unclosed() {
        let mut lexer = Lexer::with_source(Source::Unknown, "while :");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedWhileClause { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, "while :");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 1);
        } else {
            panic!("Wrong error cause: {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, "while :");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 8);
    }

    #[test]
    fn parser_while_loop_empty_posix() {
        let mut lexer = Lexer::with_source(Source::Unknown, " while do :; done");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::EmptyWhileCondition)
        );
        assert_eq!(e.location.line.value, " while do :; done");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 2);
    }

    #[test]
    fn parser_while_loop_aliasing() {
        let mut lexer = Lexer::with_source(Source::Unknown, " while :; DO :; done");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "DO".to_string(),
            "do".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "while".to_string(),
            ";;".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(result.to_string(), "while :; do :; done");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_until_loop_short() {
        let mut lexer = Lexer::with_source(Source::Unknown, "until true; do :; done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::Until { condition, body } = result {
            assert_eq!(condition.to_string(), "true");
            assert_eq!(body.to_string(), ":");
        } else {
            panic!("Not an until loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_until_loop_long() {
        let mut lexer = Lexer::with_source(Source::Unknown, "until false; true& do foo; bar& done");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let CompoundCommand::Until { condition, body } = result {
            assert_eq!(condition.to_string(), "false; true&");
            assert_eq!(body.to_string(), "foo; bar&");
        } else {
            panic!("Not an until loop: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_until_loop_unclosed() {
        let mut lexer = Lexer::with_source(Source::Unknown, "until :");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        if let ErrorCause::Syntax(SyntaxError::UnclosedUntilClause { opening_location }) = e.cause {
            assert_eq!(opening_location.line.value, "until :");
            assert_eq!(opening_location.line.number.get(), 1);
            assert_eq!(opening_location.line.source, Source::Unknown);
            assert_eq!(opening_location.column.get(), 1);
        } else {
            panic!("Wrong error cause: {:?}", e.cause);
        }
        assert_eq!(e.location.line.value, "until :");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 8);
    }

    #[test]
    fn parser_until_loop_empty_posix() {
        let mut lexer = Lexer::with_source(Source::Unknown, "  until do :; done");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.compound_command()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::EmptyUntilCondition)
        );
        assert_eq!(e.location.line.value, "  until do :; done");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 3);
    }

    #[test]
    fn parser_until_loop_aliasing() {
        let mut lexer = Lexer::with_source(Source::Unknown, " until :; DO :; done");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "DO".to_string(),
            "do".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "until".to_string(),
            ";;".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let result = block_on(parser.compound_command()).unwrap().unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(result.to_string(), "until :; do :; done");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_compound_command_none() {
        let mut lexer = Lexer::with_source(Source::Unknown, "}");
        let mut parser = Parser::new(&mut lexer);

        let option = block_on(parser.compound_command()).unwrap();
        assert_eq!(option, None);
    }

    #[test]
    fn parser_full_compound_command_without_redirections() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(:)");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.full_compound_command()).unwrap().unwrap();
        let FullCompoundCommand { command, redirs } = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(command.to_string(), "(:)");
        assert_eq!(redirs, []);
    }

    #[test]
    fn parser_full_compound_command_with_redirections() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(command) <foo >bar ;");
        let mut parser = Parser::new(&mut lexer);

        let result = block_on(parser.full_compound_command()).unwrap().unwrap();
        let FullCompoundCommand { command, redirs } = result.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(command.to_string(), "(command)");
        assert_eq!(redirs.len(), 2);
        assert_eq!(redirs[0].to_string(), "<foo");
        assert_eq!(redirs[1].to_string(), ">bar");

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, Operator(Semicolon));
    }

    #[test]
    fn parser_full_compound_command_none() {
        let mut lexer = Lexer::with_source(Source::Unknown, "}");
        let mut parser = Parser::new(&mut lexer);

        let option = block_on(parser.full_compound_command()).unwrap();
        assert_eq!(option, None);
    }

    #[test]
    fn parser_short_function_definition_ok() {
        let mut lexer = Lexer::with_source(Source::Unknown, " ( ) ( : ) > /dev/null ");
        let mut parser = Parser::new(&mut lexer);
        let c = SimpleCommand {
            assigns: vec![],
            words: vec!["foo".parse().unwrap()],
            redirs: vec![],
        };

        let result = block_on(parser.short_function_definition(c)).unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Function(f) = result {
            assert_eq!(f.has_keyword, false);
            assert_eq!(f.name.to_string(), "foo");
            assert_eq!(f.body.to_string(), "(:) >/dev/null");
        } else {
            panic!("Not a function definition: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_short_function_definition_not_one_word_name() {
        let mut lexer = Lexer::with_source(Source::Unknown, "(");
        let mut parser = Parser::new(&mut lexer);
        let c = SimpleCommand {
            assigns: vec![],
            words: vec![],
            redirs: vec![],
        };

        let result = block_on(parser.short_function_definition(c)).unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Simple(c) = result {
            assert_eq!(c.to_string(), "");
        } else {
            panic!("Not a simple command: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, Operator(OpenParen));
    }

    #[test]
    fn parser_short_function_definition_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let mut parser = Parser::new(&mut lexer);
        let c = SimpleCommand {
            assigns: vec![],
            words: vec!["foo".parse().unwrap()],
            redirs: vec![],
        };

        let result = block_on(parser.short_function_definition(c)).unwrap();
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Simple(c) = result {
            assert_eq!(c.to_string(), "foo");
        } else {
            panic!("Not a simple command: {:?}", result);
        }
    }

    #[test]
    fn parser_short_function_definition_unmatched_parenthesis() {
        let mut lexer = Lexer::with_source(Source::Unknown, "( ");
        let mut parser = Parser::new(&mut lexer);
        let c = SimpleCommand {
            assigns: vec![],
            words: vec!["foo".parse().unwrap()],
            redirs: vec![],
        };

        let e = block_on(parser.short_function_definition(c)).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::UnmatchedParenthesis)
        );
        assert_eq!(e.location.line.value, "( ");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 3);
    }

    #[test]
    fn parser_short_function_definition_missing_function_body() {
        let mut lexer = Lexer::with_source(Source::Unknown, "( ) ");
        let mut parser = Parser::new(&mut lexer);
        let c = SimpleCommand {
            assigns: vec![],
            words: vec!["foo".parse().unwrap()],
            redirs: vec![],
        };

        let e = block_on(parser.short_function_definition(c)).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingFunctionBody)
        );
        assert_eq!(e.location.line.value, "( ) ");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 5);
    }

    #[test]
    fn parser_short_function_definition_invalid_function_body() {
        let mut lexer = Lexer::with_source(Source::Unknown, "() foo ; ");
        let mut parser = Parser::new(&mut lexer);
        let c = SimpleCommand {
            assigns: vec![],
            words: vec!["foo".parse().unwrap()],
            redirs: vec![],
        };

        let e = block_on(parser.short_function_definition(c)).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::InvalidFunctionBody)
        );
        assert_eq!(e.location.line.value, "() foo ; ");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 4);
    }

    #[test]
    fn parser_short_function_definition_close_parenthesis_alias() {
        let mut lexer = Lexer::with_source(Source::Unknown, " a b ");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "a".to_string(),
            "f( ".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "b".to_string(),
            " c".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "c".to_string(),
            " )\n\n(:)".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let result = block_on(async {
            parser.simple_command().await.unwrap(); // alias
            let c = parser.simple_command().await.unwrap().unwrap().unwrap();
            parser.short_function_definition(c).await.unwrap()
        });
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Function(f) = result {
            assert_eq!(f.has_keyword, false);
            assert_eq!(f.name.to_string(), "f");
            assert_eq!(f.body.to_string(), "(:)");
        } else {
            panic!("Not a function definition: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_short_function_definition_body_alias_and_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, " a b ");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "a".to_string(),
            "f() ".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "b".to_string(),
            " c".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "c".to_string(),
            "\n\n(:)".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let result = block_on(async {
            parser.simple_command().await.unwrap(); // alias
            let c = parser.simple_command().await.unwrap().unwrap().unwrap();
            parser.short_function_definition(c).await.unwrap()
        });
        let result = result.fill(&mut std::iter::empty()).unwrap();
        if let Command::Function(f) = result {
            assert_eq!(f.has_keyword, false);
            assert_eq!(f.name.to_string(), "f");
            assert_eq!(f.body.to_string(), "(:)");
        } else {
            panic!("Not a function definition: {:?}", result);
        }

        let next = block_on(parser.peek_token()).unwrap();
        assert_eq!(next.id, EndOfInput);
    }

    #[test]
    fn parser_short_function_definition_alias_inapplicable() {
        let mut lexer = Lexer::with_source(Source::Unknown, "()b");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "b".to_string(),
            " c".to_string(),
            false,
            origin.clone(),
        ));
        aliases.insert(HashEntry::new(
            "c".to_string(),
            "(:)".to_string(),
            false,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));
        let c = SimpleCommand {
            assigns: vec![],
            words: vec!["f".parse().unwrap()],
            redirs: vec![],
        };

        let e = block_on(parser.short_function_definition(c)).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::InvalidFunctionBody)
        );
        assert_eq!(e.location.line.value, "()b");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 3);
    }

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

    #[test]
    fn parser_pipeline_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let mut parser = Parser::new(&mut lexer);

        let option = block_on(parser.pipeline()).unwrap().unwrap();
        assert_eq!(option, None);
    }

    #[test]
    fn parser_pipeline_one() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo");
        let mut parser = Parser::new(&mut lexer);

        let p = block_on(parser.pipeline()).unwrap().unwrap().unwrap();
        let p = p.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(p.negation, false);
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].to_string(), "foo");
    }

    #[test]
    fn parser_pipeline_many() {
        let mut lexer = Lexer::with_source(Source::Unknown, "one | two | \n\t\n three");
        let mut parser = Parser::new(&mut lexer);

        let p = block_on(parser.pipeline()).unwrap().unwrap().unwrap();
        let p = p.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(p.negation, false);
        assert_eq!(p.commands.len(), 3);
        assert_eq!(p.commands[0].to_string(), "one");
        assert_eq!(p.commands[1].to_string(), "two");
        assert_eq!(p.commands[2].to_string(), "three");
    }

    #[test]
    fn parser_pipeline_negated() {
        let mut lexer = Lexer::with_source(Source::Unknown, "! foo");
        let mut parser = Parser::new(&mut lexer);

        let p = block_on(parser.pipeline()).unwrap().unwrap().unwrap();
        let p = p.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(p.negation, true);
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].to_string(), "foo");
    }

    #[test]
    fn parser_pipeline_double_negation() {
        let mut lexer = Lexer::with_source(Source::Unknown, " !  !");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.pipeline()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::DoubleNegation));
        assert_eq!(e.location.line.value, " !  !");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 2);
    }

    #[test]
    fn parser_pipeline_missing_command_after_negation() {
        let mut lexer = Lexer::with_source(Source::Unknown, "!\nfoo");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.pipeline()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingCommandAfterBang)
        );
        assert_eq!(e.location.line.value, "!\n");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 1);
    }

    #[test]
    fn parser_pipeline_missing_command_after_bar() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo | ;");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.pipeline()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingCommandAfterBar)
        );
        assert_eq!(e.location.line.value, "foo | ;");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 5);
    }

    #[test]
    fn parser_pipeline_bang_after_bar() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo | !");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.pipeline()).unwrap_err();
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::BangAfterBar));
        assert_eq!(e.location.line.value, "foo | !");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 7);
    }

    #[test]
    fn parser_pipeline_no_aliasing_of_bang() {
        let mut lexer = Lexer::with_source(Source::Unknown, "! ok");
        let mut aliases = AliasSet::new();
        let origin = Location::dummy("".to_string());
        aliases.insert(HashEntry::new(
            "!".to_string(),
            "; ; ;".to_string(),
            true,
            origin,
        ));
        let mut parser = Parser::with_aliases(&mut lexer, std::rc::Rc::new(aliases));

        let p = block_on(parser.pipeline()).unwrap().unwrap().unwrap();
        let p = p.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(p.negation, true);
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].to_string(), "ok");
    }

    #[test]
    fn parser_and_or_list_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let mut parser = Parser::new(&mut lexer);

        let option = block_on(parser.and_or_list()).unwrap().unwrap();
        assert_eq!(option, None);
    }

    #[test]
    fn parser_and_or_list_one() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo");
        let mut parser = Parser::new(&mut lexer);

        let aol = block_on(parser.and_or_list()).unwrap().unwrap().unwrap();
        let aol = aol.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(aol.first.to_string(), "foo");
        assert_eq!(aol.rest, vec![]);
    }

    #[test]
    fn parser_and_or_list_many() {
        let mut lexer = Lexer::with_source(Source::Unknown, "first && second || \n\n third;");
        let mut parser = Parser::new(&mut lexer);

        let aol = block_on(parser.and_or_list()).unwrap().unwrap().unwrap();
        let aol = aol.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(aol.first.to_string(), "first");
        assert_eq!(aol.rest.len(), 2);
        assert_eq!(aol.rest[0].0, AndOr::AndThen);
        assert_eq!(aol.rest[0].1.to_string(), "second");
        assert_eq!(aol.rest[1].0, AndOr::OrElse);
        assert_eq!(aol.rest[1].1.to_string(), "third");
    }

    #[test]
    fn parser_and_or_list_missing_command_after_and_and() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo &&");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.and_or_list()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingPipeline(AndOr::AndThen))
        );
        assert_eq!(e.location.line.value, "foo &&");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 7);
    }

    #[test]
    fn parser_list_eof() {
        let mut lexer = Lexer::with_source(Source::Unknown, "");
        let mut parser = Parser::new(&mut lexer);

        let list = block_on(parser.list()).unwrap().unwrap();
        assert_eq!(list.0, vec![]);
    }

    #[test]
    fn parser_list_one_item_without_last_semicolon() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo");
        let mut parser = Parser::new(&mut lexer);

        let list = block_on(parser.list()).unwrap().unwrap();
        let list = list.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(list.0.len(), 1);
        assert_eq!(list.0[0].is_async, false);
        assert_eq!(list.0[0].and_or.to_string(), "foo");
    }

    #[test]
    fn parser_list_one_item_with_last_semicolon() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo;");
        let mut parser = Parser::new(&mut lexer);

        let list = block_on(parser.list()).unwrap().unwrap();
        let list = list.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(list.0.len(), 1);
        assert_eq!(list.0[0].is_async, false);
        assert_eq!(list.0[0].and_or.to_string(), "foo");
    }

    #[test]
    fn parser_list_many_items() {
        let mut lexer = Lexer::with_source(Source::Unknown, "foo & bar ; baz&");
        let mut parser = Parser::new(&mut lexer);

        let list = block_on(parser.list()).unwrap().unwrap();
        let list = list.fill(&mut std::iter::empty()).unwrap();
        assert_eq!(list.0.len(), 3);
        assert_eq!(list.0[0].is_async, true);
        assert_eq!(list.0[0].and_or.to_string(), "foo");
        assert_eq!(list.0[1].is_async, false);
        assert_eq!(list.0[1].and_or.to_string(), "bar");
        assert_eq!(list.0[2].is_async, true);
        assert_eq!(list.0[2].and_or.to_string(), "baz");
    }

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

        let List(items) = block_on(parser.command_line()).unwrap().unwrap();
        assert_eq!(items.len(), 1);
        let item = items.first().unwrap();
        assert_eq!(item.is_async, false);
        let AndOrList { first, rest } = &item.and_or;
        assert!(rest.is_empty(), "expected empty rest: {:?}", rest);
        let Pipeline { commands, negation } = first;
        assert_eq!(*negation, false);
        assert_eq!(commands.len(), 1);
        let cmd = match commands[0] {
            Command::Simple(ref c) => c,
            _ => panic!("Expected a simple command but got {:?}", commands[0]),
        };
        assert_eq!(cmd.words, []);
        assert_eq!(cmd.redirs.len(), 1);
        assert_eq!(cmd.redirs[0].fd, None);
        if let RedirBody::HereDoc(ref here_doc) = cmd.redirs[0].body {
            let HereDoc {
                delimiter,
                remove_tabs,
                content,
            } = here_doc;
            assert_eq!(delimiter.to_string(), "END");
            assert_eq!(*remove_tabs, false);
            assert_eq!(content.to_string(), "foo\n");
        } else {
            panic!("Expected here-document, but got {:?}", cmd.redirs[0].body);
        }
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

        let list = block_on(parser.command_line()).unwrap().unwrap();
        assert_eq!(list.0, []);
    }

    #[test]
    fn parser_command_line_here_doc_without_newline() {
        let mut lexer = Lexer::with_source(Source::Unknown, "<<END");
        let mut parser = Parser::new(&mut lexer);

        let e = block_on(parser.command_line()).unwrap_err();
        assert_eq!(
            e.cause,
            ErrorCause::Syntax(SyntaxError::MissingHereDocContent)
        );
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
        assert_eq!(e.cause, ErrorCause::Syntax(SyntaxError::UnexpectedToken));
        assert_eq!(e.location.line.value, "foo)");
        assert_eq!(e.location.line.number.get(), 1);
        assert_eq!(e.location.line.source, Source::Unknown);
        assert_eq!(e.location.column.get(), 4);
    }
}