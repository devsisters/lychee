//! Check SQL code blocks in Markdown documents.
//!
//! Every fenced code block whose language is one of the configured SQL
//! languages (` ```sql ` by default) is parsed with [`sqlparser`], using the
//! configured [`SqlDialect`]. Code which cannot be parsed is reported like a
//! broken link, at the location of the syntax error.
//!
//! Add `ignore` to the info string of a code block (` ```sql ignore `) to skip
//! it.

mod blocks;
mod parse;

use serde::Deserialize;
use sqlparser::dialect::{Dialect, dialect_from_str};
use strum::{AsRefStr, Display, EnumIter, EnumString, VariantNames};

use crate::types::uri::raw::{SourceSpanProvider, SpanProvider};
use crate::{ErrorKind, FileType, InputContent, Request, RequestError};

use blocks::find_sql_blocks;
use parse::parse;

/// SQL dialects for parsing SQL code blocks, see [`sqlparser::dialect`].
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    AsRefStr,
    Display,
    EnumIter,
    EnumString,
    VariantNames,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum SqlDialect {
    /// ANSI SQL:2011
    Ansi,
    /// Google `BigQuery`
    BigQuery,
    /// `ClickHouse`
    ClickHouse,
    /// Databricks SQL
    Databricks,
    /// `DuckDB`
    DuckDb,
    /// A permissive dialect which accepts syntax from many SQL dialects
    #[default]
    Generic,
    /// Apache Hive
    Hive,
    /// Microsoft SQL Server
    MsSql,
    /// `MySQL`
    MySql,
    /// Oracle
    Oracle,
    /// `PostgreSQL`
    PostgreSql,
    /// Amazon Redshift
    Redshift,
    /// Snowflake
    Snowflake,
    /// Apache Spark SQL
    Spark,
    /// `SQLite`
    Sqlite,
    /// Teradata
    Teradata,
}

impl SqlDialect {
    fn parser_dialect(self) -> Box<dyn Dialect> {
        dialect_from_str(self.as_ref())
            .unwrap_or_else(|| unreachable!("sqlparser supports the {self} dialect"))
    }
}

/// Options for checking SQL code blocks, see the [module documentation](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlOptions {
    dialect: SqlDialect,
    languages: Vec<String>,
}

impl Default for SqlOptions {
    fn default() -> Self {
        Self {
            dialect: SqlDialect::default(),
            languages: vec!["sql".to_string()],
        }
    }
}

impl SqlOptions {
    /// Create options with the defaults: code blocks with the `sql` language
    /// are checked for syntax errors with the [`SqlDialect::Generic`] dialect.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the SQL dialect used to parse code blocks.
    #[must_use]
    pub const fn dialect(mut self, dialect: SqlDialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Set the languages of the code blocks which contain SQL, like `sql`.
    /// Languages are compared case-insensitively.
    #[must_use]
    pub fn languages(mut self, languages: Vec<String>) -> Self {
        self.languages = languages;
        self
    }
}

/// Check the SQL code blocks of a Markdown document.
///
/// Returns an error for each statement which cannot be parsed.
pub(crate) fn extract_requests(
    content: &InputContent,
    options: &SqlOptions,
) -> Vec<Result<Request, RequestError>> {
    if content.file_type != FileType::Markdown {
        return Vec::new();
    }

    let blocks = find_sql_blocks(&content.content, &options.languages);
    if blocks.is_empty() {
        return Vec::new();
    }

    let dialect = options.dialect.parser_dialect();
    let spans = SourceSpanProvider::from_input(&content.content);

    let mut requests = Vec::new();
    for block in &blocks {
        let errors = parse(&block.text, dialect.as_ref())
            .into_iter()
            .filter_map(Result::err);
        for error in errors {
            let offset = block.document_offset(error.location.line, error.location.column);
            requests.push(Err(RequestError::InvalidSql(
                content.source.clone(),
                spans.span(offset),
                Box::new(ErrorKind::InvalidSql(error.message)),
            )));
        }
    }
    requests
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use strum::IntoEnumIterator;

    use super::*;

    fn extract(markdown: &str, options: &SqlOptions) -> Vec<String> {
        let content = InputContent::from_string(markdown, FileType::Markdown);
        extract_requests(&content, options)
            .into_iter()
            .map(|request| match request {
                Err(e @ RequestError::InvalidSql(_, span, _)) => format!("{span}: {}", e.error()),
                other => panic!("Unexpected result {other:?}"),
            })
            .collect()
    }

    const MARKDOWN: &str = "\
# Orders

```sql
SELECT o.id, c.name
FROM main.sales.orders o
JOIN customers c ON o.customer_id = c.id
```

Broken:

```sql
SELECT *
FORM main.sales.orders
```

```sql ignore
SELECT * FROM {{ table }}
```

```python
spark.sql('SELEC 1')
```
";

    #[test]
    fn test_all_dialects_are_supported() {
        for dialect in SqlDialect::iter() {
            let _ = dialect.parser_dialect();
            assert_eq!(dialect.as_ref().parse::<SqlDialect>(), Ok(dialect));
        }
        assert_eq!(
            "BigQuery".parse::<SqlDialect>(),
            Err(strum::ParseError::VariantNotFound)
        );
        assert_eq!("bigquery".parse::<SqlDialect>(), Ok(SqlDialect::BigQuery));
    }

    #[test]
    fn test_syntax_errors() {
        assert_eq!(
            extract(MARKDOWN, &SqlOptions::new()),
            ["13:1: SQL syntax error: Expected: end of statement, found: FORM"]
        );
    }

    #[test]
    fn test_dialect() {
        let markdown = "```sql\nSELECT * FROM orders VERSION AS OF 3\n```\n";
        assert_eq!(
            extract(markdown, &SqlOptions::new()),
            ["2:30: SQL syntax error: Expected: end of statement, found: AS"]
        );
        assert_eq!(
            extract(markdown, &SqlOptions::new().dialect(SqlDialect::Databricks)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_span_of_nested_code_block() {
        let markdown = "1. Run this:\n\n   ```sql\n   SELECT * FORM t\n   ```\n";
        assert_eq!(
            extract(markdown, &SqlOptions::new()),
            ["4:13: SQL syntax error: Expected: end of statement, found: FORM"]
        );
    }

    #[test]
    fn test_no_requests_for_other_file_types() {
        let content = InputContent::from_string("```sql\nSELEC 1\n```", FileType::Html);
        assert!(extract_requests(&content, &SqlOptions::new()).is_empty());
    }
}
