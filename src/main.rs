//! Arlineo: a code map for teams of humans and AI agents working in many git worktrees.
//!
//! The binary reads a repository through `git`, extracts facts from Rust, SQL and
//! TypeScript/JavaScript sources with hand-written lexers, builds a call graph and answers
//! questions about it. See `SPEC.md` for the full design.

mod check;
mod cli;
mod config;
mod conflicts;
mod diff;
mod error;
mod extract;
mod git;
mod graph;
mod index;
mod lex;
mod model;
mod output;
mod overlay;
mod query;
mod source;
mod store;
mod util;

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    std::process::exit(cli::run(args));
}
