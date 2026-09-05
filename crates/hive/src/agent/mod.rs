//! Agent: a team member's engine (claude job / codex thread / grok
//! session), optionally shown in a tmux pane.

mod control;
mod seams;
mod spawn;
mod support;

#[cfg(test)]
pub(crate) mod testhook;
#[cfg(test)]
mod tests;

pub use spawn::*;
pub use support::*;
