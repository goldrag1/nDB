//! nDB query-language lexer + parser.
//!
//! Takes query source text and produces a **name-based AST**
//! ([`NameQuery`]) — type / role / property fields are still strings.
//! The engine-side resolver converts a [`NameQuery`] into an
//! [`ndb_engine::QueryRequest`] (id-based wire AST) by looking up the
//! dictionaries.
//!
//! Authoritative spec:
//! `docs/superpowers/specs/2026-05-27-query-language.md`.
//!
//! ```rust
//! use ndb_query::parse_query;
//! let q = parse_query("match customer(name: ?n) return ?n limit 10").unwrap();
//! assert_eq!(q.patterns.len(), 1);
//! assert_eq!(q.returns[0].name, "n");
//! assert_eq!(q.limit, Some(10));
//! ```
//!
//! Errors carry source spans — see [`ParseError::span`] and
//! [`Span::locate`] for line/column rendering.

#![warn(missing_docs)]

pub mod ast;
pub mod error;
pub mod lex;
pub mod parse;
pub mod resolve;
pub mod run;

pub use ast::{
    NameAsOf, NameBinding, NameCmpOp, NameExpr, NamePattern, NameQuery, NameRecursion,
    NameReturn, NameTerm,
};
pub use error::{ParseError, Span};
pub use lex::{Tok, TokKind, lex};
pub use parse::parse_query;
pub use resolve::{Dictionaries, ResolveError, TypeKindObserved, resolve};
pub use run::{
    RunError, RunErrorEnvelope, SpanInfo, execute_text, execute_text_read, parse_resolve,
};
