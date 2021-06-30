// This file is part of yash, an extended POSIX shell.
// Copyright (C) 2021 WATANABE Yuki
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

//! Return built-in.

use std::future::ready;
use std::future::Future;
use std::pin::Pin;
use yash_env::builtin::Result;
use yash_env::exec::ExitStatus;
use yash_env::expansion::Field;
use yash_env::Env;

/// Part of the shell execution environment the return built-in depends on.
pub trait ReturnBuiltinEnv {
    // TODO Current value of $?
    // TODO Current execution context (stack trace)
    // TODO stderr
}

impl ReturnBuiltinEnv for Env {}

// TODO Describe in terms of Divert. Should we differentiate API-level
// description from end-user-level one?
/// Implementation of the return built-in.
///
/// The return built-in quits the currently executing innermost function or
/// script.
///
/// If the shell is not currently executing any function or script, the built-in
/// will exit the current shell execution environment unless it is an
/// interactive session.
///
/// # Syntax
///
/// ```sh
/// return [-n] [exit_status]
/// ```
///
/// # Options
///
/// The **`-n`** (**`--no-return`**) option makes the built-in not actually quit
/// a function or script. This option will be helpful when you want to set the
/// exit status to an arbitrary value without any other side effect.
///
/// # Operands
///
/// The optional ***exit_status*** operand, if given, should be a non-negative
/// integer and will be the exit status of the built-in.
///
/// # Exit status
///
/// The *exit_status* operand will be the exit status of the built-in.
///
/// If the operand is not given:
///
/// - If the currently executing script is a trap, the exit status will be the
///   value of `$?` before entering the trap.
/// - Otherwise, the exit status will be the current value of `$?`.
///
/// # Errors
///
/// If the *exit_status* operand is given but not a valid non-negative integer,
/// it is a syntax error. In that case, an error message is printed, and the
/// exit status will be 2, but the built-in still quits a function or script.
///
/// This implementation treats an *exit_status* value greater than 4294967295 as
/// a syntax error.
///
/// # Portability
///
/// POSIX only requires the return built-in to quit a function or dot script.
/// The behavior for other kinds of scripts is a non-standard extension.
///
/// The `-n` (`--no-return`) option is a non-standard extension.
///
/// Many implementations do not support *exit_status* values greater than 255.
pub fn return_builtin<E: ReturnBuiltinEnv>(_env: &mut E, args: Vec<Field>) -> Result {
    // TODO Parse arguments correctly
    let exit_status: u32 = match args.get(2) {
        Some(field) => field.value.parse().unwrap_or(2),
        None => 0,
    };
    (ExitStatus(exit_status), None)
}

/// Implementation of the return built-in.
///
/// This function calls [`return_builtin`] and wraps the result in a `Future`.
pub fn return_builtin_async(
    env: &mut Env,
    args: Vec<Field>,
) -> Pin<Box<dyn Future<Output = Result>>> {
    Box::pin(ready(return_builtin(env, args)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use yash_env::exec::ExitStatus;

    #[derive(Default)]
    struct DummyEnv;

    impl ReturnBuiltinEnv for DummyEnv {}

    #[test]
    fn returns_exit_status_12_with_n_option() {
        let mut env = DummyEnv::default();
        let arg0 = Field::dummy("return".to_string());
        let arg1 = Field::dummy("-n".to_string());
        let arg2 = Field::dummy("12".to_string());
        let args = vec![arg0, arg1, arg2];

        let result = return_builtin(&mut env, args);
        assert_eq!(result, (ExitStatus(12), None));
    }

    #[test]
    fn returns_exit_status_47_with_n_option() {
        let mut env = DummyEnv::default();
        let arg0 = Field::dummy("return".to_string());
        let arg1 = Field::dummy("-n".to_string());
        let arg2 = Field::dummy("47".to_string());
        let args = vec![arg0, arg1, arg2];

        let result = return_builtin(&mut env, args);
        assert_eq!(result, (ExitStatus(47), None));
    }
}
