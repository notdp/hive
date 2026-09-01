//! Agent: an agent CLI instance running in a tmux pane.

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
