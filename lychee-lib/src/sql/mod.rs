//! Check SQL code blocks in Markdown documents.
//!
//! Every fenced code block whose language is one of the configured SQL
//! languages (` ```sql ` by default) is parsed with [`sqlparser`], using the
//! configured [`SqlDialect`]. Code which cannot be parsed is reported like a
//! broken link, at the location of the syntax error.
//!
//! Optionally, the tables which valid code reads from or writes into can be
//! checked in Databricks Unity Catalog (see [`SqlOptions::unity_catalog_tables`]).
//! Each table reference becomes a request for a `uc:catalog.schema.table` URI,
//! which the [`Client`](crate::Client) looks up with the workspace configured in
//! [`ClientBuilder::unity_catalog`](crate::ClientBuilder). Because they are
//! URIs, table references can be excluded with the usual
//! [`Filter`](crate::Filter) patterns, e.g. `^uc:hive_metastore\.`.
//!
//! Table names that don't specify a catalog or schema are qualified with
//! `USE CATALOG` and `USE SCHEMA` statements earlier in the document, or with the
//! defaults from [`SqlOptions`]. Names that cannot be qualified are not checked.
//!
//! Add `ignore` to the info string of a code block (` ```sql ignore `) to skip
//! it.

mod blocks;
mod parse;
mod references;
mod table;

use serde::Deserialize;
use sqlparser::dialect::{Dialect, dialect_from_str};
use strum::{AsRefStr, Display, EnumIter, EnumString, VariantNames};

use crate::types::uri::raw::{SourceSpanProvider, SpanProvider};
use crate::{ErrorKind, FileType, InputContent, Request, RequestError};

use blocks::find_sql_blocks;
use parse::parse;
use references::Scope;
pub(crate) use table::TableName;
pub use table::UNITY_CATALOG_SCHEME;

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
    unity_catalog_tables: bool,
    default_catalog: Option<String>,
    default_schema: Option<String>,
}

impl Default for SqlOptions {
    fn default() -> Self {
        Self {
            dialect: SqlDialect::default(),
            languages: vec!["sql".to_string()],
            unity_catalog_tables: false,
            default_catalog: None,
            default_schema: None,
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

    /// Extract the tables referenced by SQL code as `uc:catalog.schema.table`
    /// URIs, to check whether they exist in Databricks Unity Catalog.
    ///
    /// The [`Client`](crate::Client) needs a workspace to look them up, see
    /// [`ClientBuilder::unity_catalog`](crate::ClientBuilder).
    #[must_use]
    pub const fn unity_catalog_tables(mut self, yes: bool) -> Self {
        self.unity_catalog_tables = yes;
        self
    }

    /// Set the catalog of table names which don't specify one, like
    /// `schema.table`. Without a default, such names are only checked after a
    /// `USE CATALOG` statement.
    #[must_use]
    pub fn default_catalog(mut self, catalog: Option<String>) -> Self {
        self.default_catalog = catalog;
        self
    }

    /// Set the schema of table names which don't specify one, like `table`.
    /// Without a default, such names are only checked after a `USE SCHEMA` or
    /// `USE CATALOG` statement.
    #[must_use]
    pub fn default_schema(mut self, schema: Option<String>) -> Self {
        self.default_schema = schema;
        self
    }
}

/// Check the SQL code blocks of a Markdown document.
///
/// Returns an error for each statement which cannot be parsed and, if
/// [`SqlOptions::unity_catalog_tables`] is enabled, a request for each table
/// referenced by valid statements.
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
    let mut scope = options.unity_catalog_tables.then(|| {
        Scope::new(
            options.default_catalog.as_deref(),
            options.default_schema.as_deref(),
        )
    });

    let mut requests = Vec::new();
    for block in &blocks {
        for statement in parse(&block.text, dialect.as_ref()) {
            match statement {
                Ok(statement) => {
                    let Some(scope) = scope.as_mut() else {
                        continue;
                    };
                    for table in scope.references(&statement) {
                        let location = table.location;
                        let offset = block.document_offset(location.line, location.column);
                        requests.push(Ok(Request::new(
                            table.name.to_uri(),
                            content.source.clone(),
                        )
                        .with_element("code".to_string())
                        .with_span(spans.span(offset))));
                    }
                }
                Err(error) => {
                    let location = error.location;
                    let offset = block.document_offset(location.line, location.column);
                    requests.push(Err(RequestError::InvalidSql(
                        content.source.clone(),
                        spans.span(offset),
                        Box::new(ErrorKind::InvalidSql(error.message)),
                    )));
                }
            }
        }
    }
    requests
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use strum::IntoEnumIterator;

    use super::*;

    /// Requests as `line:column: uri` and errors as `line:column: error`
    fn extract(markdown: &str, options: &SqlOptions) -> Vec<String> {
        let content = InputContent::from_string(markdown, FileType::Markdown);
        extract_requests(&content, options)
            .into_iter()
            .map(|request| match request {
                Ok(request) => format!("{}: {}", request.span.unwrap(), request.uri),
                Err(e @ RequestError::InvalidSql(_, span, _)) => format!("{span}: {}", e.error()),
                Err(e) => panic!("Unexpected error {e}"),
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
spark.sql('PICK 1')
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
    fn test_unity_catalog_tables() {
        let options = SqlOptions::new()
            .dialect(SqlDialect::Databricks)
            .unity_catalog_tables(true)
            .default_catalog(Some("main".to_string()))
            .default_schema(Some("crm".to_string()));

        assert_eq!(
            extract(MARKDOWN, &options),
            [
                "5:6: uc:main.sales.orders",
                "6:6: uc:main.crm.customers",
                "13:1: SQL syntax error: Expected: end of statement, found: FORM",
            ]
        );
    }

    #[test]
    fn test_unity_catalog_tables_in_nested_code_block() {
        let markdown = "1. Run this:\n\n   ```sql\n   SELECT * FROM main.s.t\n   ```\n";
        assert_eq!(
            extract(markdown, &SqlOptions::new().unity_catalog_tables(true)),
            ["4:18: uc:main.s.t"]
        );
    }

    #[test]
    fn test_no_requests_for_other_file_types() {
        let content = InputContent::from_string("```sql\nPICK 1\n```", FileType::Html);
        assert!(extract_requests(&content, &SqlOptions::new()).is_empty());
    }
}
