//! The spellings `PostgreSQL` accepts for the session-identity functions.
//!
//! WHY THIS EXISTS
//! ===============
//! `current_user`, `session_user`, `current_role`, `user`, `current_catalog` and
//! `current_schema` are SQL-standard niladic functions that `PostgreSQL` spells
//! WITHOUT parentheses. `sqlparser` models only some of them that way, and
//! `DataFusion` resolves only the ones that reach it as a function call - so
//! several spellings a client may legitimately send fail here while working
//! against a real server.
//!
//! Measured against `PostgreSQL` 17.4 and against this crate, the gap is:
//!
//! ```text
//!   spelling                          PostgreSQL     this crate (before)
//!   SELECT current_user               role           role
//!   SELECT session_user               role           role
//!   SELECT current_role               role           No field named current_role
//!   SELECT user                       role           No field named user
//!   SELECT current_catalog            database       No field named current_catalog
//!   SELECT current_schema             schema         No field named current_schema
//!   SELECT pg_catalog.current_user()  role           parse error
//!   SELECT pg_catalog.session_user()  role           parse error
//! ```
//!
//! The six failures are what this module removes, by rewriting each spelling
//! into the one form that both re-parses and resolves here.
//!
//! WHY TWO MECHANISMS
//! ==================
//! Four of the six parse fine and fail later, so they are fixed on the parsed
//! statement like every other rewrite in this crate. The other two do not parse
//! at all - `sqlparser` reads `current_user` as a parenthesis-less keyword and
//! then rejects the `(` that follows - so nothing downstream can see them, and
//! they have to be dealt with on the text before it is parsed.
//!
//! That text edit is made by LOCATING the parentheses with the tokenizer and
//! deleting exactly those two characters from the original string. It is not a
//! search-and-replace: the tokenizer knows a string literal from an identifier,
//! so `SELECT 'pg_catalog.current_user()'` is left alone. Re-emitting the token
//! stream was tried first and rejected - `Token`'s `Display` does not round-trip
//! escaped literals (`E'a\nb'` comes back as a real newline and `'it''s'` loses
//! its doubled quote), which would corrupt every psql query that embeds `E'\n'`.
//!
//! WHAT IS DELIBERATELY NOT MATCHED
//! ================================
//! `pg_catalog.current_role()` and `pg_catalog.user()` are errors in
//! `PostgreSQL` too ("function ... does not exist"), because no such catalog
//! function exists there. They are left failing rather than invented.
//!
//! A QUOTED identifier is never rewritten. `PostgreSQL` reserves all six of
//! these words, so `SELECT user` cannot be a column reference and `"user"` is
//! the only way to name such a column - which makes the quote the exact line
//! between the function and a column that happens to share its name.

use datafusion::error::{DataFusionError, Result};
use sqlparser::ast::{
    visit_expressions_mut, Expr, Function, FunctionArgumentList, FunctionArguments, Ident,
    ObjectName, ObjectNamePart, Statement,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Location, Token, Tokenizer};
use std::ops::ControlFlow;

/// The identity keywords `sqlparser` reads as parenthesis-less, so a following
/// `(` is a parse error even though `PostgreSQL` accepts the parenthesised,
/// schema-qualified call.
const PARENTHESIS_LESS_TO_SQLPARSER: &[&str] = &["current_user", "session_user"];

/// Each identity keyword this crate does not resolve as written, paired with the
/// spelling it does resolve.
///
/// The replacement is chosen so it survives the rest of the pipeline: every pass
/// after this one re-parses the SQL the previous pass serialised, so a
/// replacement has to mean the same thing when read back. That rules out the
/// obvious choices in both directions - `current_role` written without
/// parentheses parses back as a bare column reference, and `current_user`
/// written WITH them does not parse at all - which is why the two columns below
/// disagree about parentheses.
const SPELLINGS: &[IdentitySpelling] = &[
    // Parses as a bare identifier; needs parentheses to become a call.
    IdentitySpelling {
        written: "current_role",
        resolved: "current_role",
        parenthesised: true,
    },
    IdentitySpelling {
        written: "current_schema",
        resolved: "current_schema",
        parenthesised: true,
    },
    // Parses as a parenthesis-less function whose name nothing is registered
    // under; `current_user` is the registered name for the same value, and it
    // must stay parenthesis-less or it stops parsing.
    IdentitySpelling {
        written: "user",
        resolved: "current_user",
        parenthesised: false,
    },
    IdentitySpelling {
        written: "current_catalog",
        resolved: "current_database",
        parenthesised: true,
    },
];

/// One identity keyword and the call this crate answers it with.
struct IdentitySpelling {
    /// The keyword as a client writes it, matched case-insensitively and only
    /// when unquoted.
    written: &'static str,
    /// The function name that is actually registered.
    resolved: &'static str,
    /// Whether the replacement is emitted as `name()` rather than bare `name`.
    /// Both spellings exist because `sqlparser` accepts only one of them per
    /// name - see [`SPELLINGS`].
    parenthesised: bool,
}

/// Rewrite every session-identity spelling `PostgreSQL` accepts into the one
/// this crate resolves, leaving all other SQL untouched.
///
/// Runs before the routing decision as well as before execution: a bare
/// `current_role` reaches the planner as a column reference, which the router
/// never recognises as a catalog function, so without this the statement is
/// handed to the embedding host instead of being answered here.
///
/// # Errors
///
/// Returns `DataFusionError::Plan` if `sql` cannot be tokenized or parsed as
/// `PostgreSQL` dialect. A statement this crate cannot read is reported rather
/// than passed through unexamined.
pub fn normalize_session_identity_spellings(sql: &str) -> Result<String> {
    let without_empty_parentheses = drop_parentheses_after_a_parenthesis_less_keyword(sql)?;

    let dialect = PostgreSqlDialect {};
    let mut statements = Parser::parse_sql(&dialect, &without_empty_parentheses)
        .map_err(|e| DataFusionError::Plan(format!("SQL parse error: {e}")))?;
    if statements.is_empty() {
        return Ok(without_empty_parentheses);
    }
    for statement in &mut statements {
        rewrite_keywords_to_registered_calls(statement);
    }
    Ok(statements
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("; "))
}

/// Delete the empty parentheses that follow a keyword `sqlparser` reads as
/// parenthesis-less, so `pg_catalog.current_user()` becomes
/// `pg_catalog.current_user` - a spelling this crate already resolves.
///
/// The parentheses are located with the tokenizer and removed from the ORIGINAL
/// text by byte offset, so nothing else in the statement can be disturbed and a
/// string literal that merely contains the same characters is not a match.
///
/// Deliberately also matches the unqualified `current_user()`, which
/// `PostgreSQL` rejects as a syntax error. Accepting it costs one branch fewer
/// and is consistent with this crate already answering `current_role()`, which
/// `PostgreSQL` rejects for the same reason.
///
/// # Errors
///
/// Returns `DataFusionError::Plan` if `sql` cannot be tokenized.
fn drop_parentheses_after_a_parenthesis_less_keyword(sql: &str) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize_with_location()
        .map_err(|e| DataFusionError::Plan(format!("SQL tokenize error: {e}")))?;

    // Whitespace and comments sit between the keyword and its parentheses in
    // `current_user ()`, so the pattern is matched over the significant tokens
    // and the spans are taken from the originals.
    let significant: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| !matches!(t.token, Token::Whitespace(_)))
        .map(|(index, _)| index)
        .collect();

    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for triple in significant.windows(3) {
        let (keyword, open, close) = (&tokens[triple[0]], &tokens[triple[1]], &tokens[triple[2]]);
        let Token::Word(word) = &keyword.token else {
            continue;
        };
        if word.quote_style.is_some()
            || !PARENTHESIS_LESS_TO_SQLPARSER
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&word.value))
        {
            continue;
        }
        if !matches!(open.token, Token::LParen) || !matches!(close.token, Token::RParen) {
            continue;
        }
        // Both are single characters, so the cut runs from where `(` starts to
        // one past where `)` starts. Taking `Span::end` instead would depend on
        // whether it is inclusive, which this does not need to know.
        let (Some(from), Some(to)) = (
            byte_offset_of(sql, open.span.start),
            byte_offset_of(sql, close.span.start),
        ) else {
            continue;
        };
        cuts.push((from, to + 1));
    }

    if cuts.is_empty() {
        return Ok(sql.to_string());
    }

    let mut out = String::with_capacity(sql.len());
    let mut copied = 0;
    for (from, to) in cuts {
        out.push_str(&sql[copied..from]);
        copied = to;
    }
    out.push_str(&sql[copied..]);
    Ok(out)
}

/// The byte offset in `sql` of the 1-based line/column `location` names, or
/// `None` when the location is past the end of the text.
///
/// The tokenizer counts columns in characters, so this walks the string rather
/// than treating a column as a byte offset - otherwise any statement containing
/// a multi-byte character before the match would be cut in the wrong place.
fn byte_offset_of(sql: &str, location: Location) -> Option<usize> {
    let mut line = 1_u64;
    let mut column = 1_u64;
    for (offset, character) in sql.char_indices() {
        if line == location.line && column == location.column {
            return Some(offset);
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line == location.line && column == location.column).then_some(sql.len())
}

/// Replace each identity keyword in `statement` with the call this crate has
/// registered for it.
fn rewrite_keywords_to_registered_calls(statement: &mut Statement) {
    let _ = visit_expressions_mut(statement, |expr| {
        if let Some(spelling) = spelling_written_as(expr) {
            *expr = registered_call(spelling);
        }
        ControlFlow::<()>::Continue(())
    });
}

/// The entry of [`SPELLINGS`] that `expr` is written as, if any.
///
/// Matches the two shapes `sqlparser` produces for a parenthesis-less keyword:
/// a bare identifier (`current_role`), and a one-part function with no argument
/// list at all (`user`). A function name of more than one part is a qualified
/// column reference such as `t.user` and is left alone, as is anything quoted.
fn spelling_written_as(expr: &Expr) -> Option<&'static IdentitySpelling> {
    let written = match expr {
        Expr::Identifier(ident) if ident.quote_style.is_none() => &ident.value,
        Expr::Function(function) if matches!(function.args, FunctionArguments::None) => {
            match function.name.0.as_slice() {
                [ObjectNamePart::Identifier(ident)] if ident.quote_style.is_none() => &ident.value,
                _ => return None,
            }
        }
        _ => return None,
    };
    SPELLINGS
        .iter()
        .find(|spelling| spelling.written.eq_ignore_ascii_case(written))
}

/// The call expression that answers `spelling`, parenthesised or not as that
/// entry requires.
fn registered_call(spelling: &IdentitySpelling) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(
            spelling.resolved,
        ))]),
        args: if spelling.parenthesised {
            FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                args: vec![],
                clauses: vec![],
            })
        } else {
            FunctionArguments::None
        },
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spelling `PostgreSQL` accepts comes out as something this crate
    /// resolves, and the result re-parses - which is what the rest of the
    /// pipeline requires, since each pass re-reads what the last one wrote.
    #[test]
    fn every_accepted_spelling_becomes_a_call_that_re_parses() {
        for (written, expected) in [
            ("SELECT current_role", "SELECT current_role()"),
            ("SELECT CURRENT_ROLE", "SELECT current_role()"),
            ("SELECT current_schema", "SELECT current_schema()"),
            ("SELECT user", "SELECT current_user"),
            ("SELECT current_catalog", "SELECT current_database()"),
            (
                "SELECT pg_catalog.current_user()",
                "SELECT pg_catalog.current_user",
            ),
            (
                "SELECT pg_catalog.session_user()",
                "SELECT pg_catalog.session_user",
            ),
        ] {
            let rewritten = normalize_session_identity_spellings(written).unwrap();
            assert_eq!(rewritten, expected, "rewriting {written}");
            // Running it again must not change it further: the pipeline applies
            // roughly thirty passes and each re-parses the last one's output.
            assert_eq!(
                normalize_session_identity_spellings(&rewritten).unwrap(),
                expected,
                "{written} should reach a fixed point"
            );
        }
    }

    /// A quoted identifier of the same name is a column, and stays one.
    ///
    /// This is the whole safety argument for rewriting a bare `user`: because
    /// `PostgreSQL` reserves the word, quoting is the only way to name such a
    /// column, so an unquoted occurrence cannot be one.
    #[test]
    fn a_quoted_identifier_is_left_as_a_column_reference() {
        for sql in [
            "SELECT \"user\" FROM t",
            "SELECT \"current_role\" FROM t",
            "SELECT t.user FROM t",
            "SELECT t.current_role FROM t",
        ] {
            let rewritten = normalize_session_identity_spellings(sql).unwrap();
            assert!(
                !rewritten.contains("current_user") && !rewritten.contains("current_role()"),
                "{sql} names a column, not a function, but became {rewritten}"
            );
        }
    }

    /// A string literal that merely contains the text is not a match.
    ///
    /// The reason the parenthesis removal locates its target with the tokenizer
    /// rather than by searching the text.
    #[test]
    fn a_string_literal_holding_the_spelling_is_untouched() {
        let sql = "SELECT 'pg_catalog.current_user()' AS described";
        assert_eq!(
            normalize_session_identity_spellings(sql).unwrap(),
            sql,
            "the literal is data, not a call to rewrite"
        );
    }

    /// Escaped string literals survive, which re-emitting the token stream did
    /// not manage.
    ///
    /// `E'\n'` is in every psql `\dp`; a rewrite that unescaped it would corrupt
    /// the query rather than fail it.
    #[test]
    fn escaped_literals_survive_the_parenthesis_removal() {
        let sql = "SELECT pg_catalog.current_user(), E'a\\nb', 'it''s' FROM t";
        let rewritten = normalize_session_identity_spellings(sql).unwrap();
        assert!(
            rewritten.contains("E'a\\nb'"),
            "the escape must survive verbatim: {rewritten}"
        );
        assert!(
            rewritten.contains("'it''s'"),
            "the doubled quote must survive verbatim: {rewritten}"
        );
    }

    /// A multi-byte character before the match does not shift the cut.
    ///
    /// The tokenizer reports columns in characters; treating one as a byte
    /// offset would slice this statement in the middle of a character.
    #[test]
    fn a_multibyte_character_before_the_call_does_not_move_the_cut() {
        let sql = "SELECT 'caf\u{e9} \u{2013} bar', pg_catalog.current_user()";
        assert_eq!(
            normalize_session_identity_spellings(sql).unwrap(),
            "SELECT 'caf\u{e9} \u{2013} bar', pg_catalog.current_user"
        );
    }

    /// Ordinary SQL with none of these words is returned as the parser
    /// normalises it, with nothing else changed.
    #[test]
    fn unrelated_sql_is_not_rewritten() {
        let sql = "SELECT a, b FROM t WHERE c = 1";
        assert_eq!(normalize_session_identity_spellings(sql).unwrap(), sql);
    }

    /// The spellings `PostgreSQL` itself rejects are not invented here.
    ///
    /// `pg_catalog.current_role()` and `pg_catalog.user()` name functions that do
    /// not exist in `PostgreSQL` either, so they keep failing rather than being
    /// answered by something `PostgreSQL` would not answer.
    #[test]
    fn spellings_postgresql_rejects_are_left_failing() {
        let rewritten =
            normalize_session_identity_spellings("SELECT pg_catalog.current_role()").unwrap();
        assert!(
            rewritten.contains("pg_catalog.current_role"),
            "left as written for the planner to reject: {rewritten}"
        );
    }
}
