//! Parse SQL code statement by statement.

use std::sync::LazyLock;

use regex::Regex;
use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Token};

/// A statement which cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyntaxError {
    /// The parser's error message, without the location.
    pub(crate) message: String,
    /// Where the parser gave up, relative to the code.
    pub(crate) location: Location,
}

/// Parse the statements of `sql`.
///
/// Statements are parsed like `sqlparser::parser::Parser::parse_statements`
/// does, but a statement which cannot be parsed doesn't stop parsing: its
/// syntax error is returned in its place, and parsing continues after the
/// next `;`. If the code cannot be tokenized, only that error is returned.
pub(crate) fn parse(sql: &str, dialect: &dyn Dialect) -> Vec<Result<Statement, SyntaxError>> {
    let mut parser = match Parser::new(dialect).try_with_sql(sql) {
        Ok(parser) => parser,
        Err(error) => return vec![Err(syntax_error(&error, sql))],
    };

    let mut statements = Vec::new();
    loop {
        while parser.consume_token(&Token::SemiColon) {}
        if parser.peek_token_ref().token == Token::EOF {
            break;
        }

        match parse_statement(&mut parser) {
            Ok(statement) => statements.push(Ok(statement)),
            Err(error) => {
                statements.push(Err(syntax_error(&error, sql)));
                skip_statement(&mut parser);
            }
        }
    }
    statements
}

/// Parse a statement, which must be followed by `;` or the end of the code.
fn parse_statement(parser: &mut Parser<'_>) -> Result<Statement, ParserError> {
    let statement = parser.parse_statement()?;
    match parser.peek_token_ref().token {
        Token::SemiColon | Token::EOF => Ok(statement),
        _ => parser.expected_ref("end of statement", parser.peek_token_ref()),
    }
}

/// Skip the rest of a statement which cannot be parsed, up to and including
/// the next `;`.
fn skip_statement(parser: &mut Parser<'_>) {
    while !matches!(parser.next_token().token, Token::SemiColon | Token::EOF) {}
}

fn syntax_error(error: &ParserError, sql: &str) -> SyntaxError {
    let message = match error {
        ParserError::TokenizerError(message) | ParserError::ParserError(message) => message,
        ParserError::RecursionLimitExceeded => "recursion limit exceeded",
    };

    match split_location(message) {
        Some((message, location)) => SyntaxError {
            message: message.to_string(),
            location,
        },
        // Errors at the end of the input, like in `SELECT * FROM`, have no
        // location.
        None => SyntaxError {
            message: message.to_string(),
            location: end_location(sql),
        },
    }
}

/// Split the ` at Line: {line}, Column: {column}` suffix, which `sqlparser`
/// appends to most error messages, from the message.
fn split_location(message: &str) -> Option<(&str, Location)> {
    static LOCATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)^(.*) at Line: (\d+), Column: (\d+)$")
            .expect("location pattern is a valid regex")
    });

    let captures = LOCATION.captures(message)?;
    let line = captures[2].parse().ok()?;
    let column = captures[3].parse().ok()?;
    Some((captures.get(1)?.as_str(), Location::new(line, column)))
}

/// The location of the last non-whitespace character in `sql`.
fn end_location(sql: &str) -> Location {
    let mut end = Location::new(1, 1);
    let (mut line, mut column) = (1, 0);
    for c in sql.chars() {
        if c == '\n' {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
        if !c.is_whitespace() {
            end = Location::new(line, column);
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sqlparser::dialect::DatabricksDialect;

    use super::*;

    /// Parsed statements as `OK`, and syntax errors as `line:column message`
    fn results(sql: &str) -> Vec<String> {
        parse(sql, &DatabricksDialect {})
            .into_iter()
            .map(|result| match result {
                Ok(_) => "OK".to_string(),
                Err(error) => format!(
                    "{}:{} {}",
                    error.location.line, error.location.column, error.message
                ),
            })
            .collect()
    }

    #[test]
    fn test_valid_code() {
        assert_eq!(
            results("SELECT * FROM t; INSERT INTO u SELECT * FROM t"),
            ["OK", "OK"]
        );
        assert_eq!(results(""), Vec::<String>::new());
        assert_eq!(results("-- just a comment\n;;"), Vec::<String>::new());
    }

    #[test]
    fn test_syntax_error_with_location() {
        assert_eq!(
            results("SELECT\n  user_id,\n  count(*)\nFORM events\nGROUP BY 1"),
            ["4:6 Expected: end of statement, found: events"]
        );
    }

    #[test]
    fn test_syntax_error_at_end_of_input() {
        assert_eq!(
            results("SELECT *\nFROM  \n\n"),
            ["2:4 Expected: identifier, found: EOF"]
        );
    }

    #[test]
    fn test_tokenizer_error() {
        assert_eq!(
            results("SELECT 1; SELECT 'unterminated"),
            ["1:18 Unterminated string literal"]
        );
    }

    #[test]
    fn test_unsupported_syntax_is_an_error() {
        // Statements which sqlparser cannot parse are reported, even if
        // Databricks accepts them.
        assert_eq!(
            results("VACUUM main.sales.orders RETAIN 168 HOURS"),
            ["1:26 Expected: end of statement, found: RETAIN"]
        );
        assert_eq!(
            results("SELECT * FROM {{ catalog }}.sales.orders"),
            ["1:15 Expected: identifier, found: {"]
        );
    }

    #[test]
    fn test_statements_are_parsed_separately() {
        assert_eq!(
            results(
                "SELEC 1;
                 SELECT * FROM orders;
                 VACUUM orders RETAIN 168 HOURS;
                 SELECT * FROM customers"
            ),
            [
                "1:1 Expected: an SQL statement, found: SELEC",
                "OK",
                "3:32 Expected: end of statement, found: RETAIN",
                "OK",
            ]
        );
    }

    #[test]
    fn test_split_location() {
        assert_eq!(
            split_location("Expected: x, found: y at Line: 12, Column: 3"),
            Some(("Expected: x, found: y", Location::new(12, 3)))
        );
        assert_eq!(split_location("Expected: x, found: EOF"), None);
    }
}
