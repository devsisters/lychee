//! Fully qualified Unity Catalog table names and their `uc:` URI form.
//!
//! Table references found in SQL code blocks are checked like links. To fit
//! into lychee's pipeline (filters, caching, output formats), each reference is
//! represented as an opaque URI such as `uc:main.sales.orders`.

use std::{borrow::Cow, fmt::Display};

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use url::Url;

use crate::{ErrorKind, Result, Uri};

/// URI scheme of Unity Catalog table references, e.g. `uc:main.sales.orders`.
pub const UNITY_CATALOG_SCHEME: &str = "uc";

/// Characters escaped in each name part, so that the `uc:` URI round-trips:
/// URL delimiters, `%` itself, and `.` (the separator between name parts).
const NAME_PART_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'.')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`');

/// A three-level Unity Catalog name: `catalog.schema.table`.
///
/// Unity Catalog stores names in lowercase and resolves them
/// case-insensitively, so all parts are normalized to lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TableName {
    catalog: String,
    schema: String,
    table: String,
}

impl TableName {
    pub(crate) fn new(catalog: &str, schema: &str, table: &str) -> Self {
        Self {
            catalog: catalog.to_lowercase(),
            schema: schema.to_lowercase(),
            table: table.to_lowercase(),
        }
    }

    /// The `uc:` URI under which this table is checked and reported.
    pub(crate) fn to_uri(&self) -> Uri {
        let encode = |part: &str| utf8_percent_encode(part, NAME_PART_ENCODE_SET).to_string();
        let uri = format!(
            "{UNITY_CATALOG_SCHEME}:{}.{}.{}",
            encode(&self.catalog),
            encode(&self.schema),
            encode(&self.table)
        );
        // All delimiters are percent-encoded above, so this cannot fail.
        Url::parse(&uri)
            .expect("an encoded table name is always a valid URI")
            .into()
    }
}

impl Display for TableName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.catalog, self.schema, self.table)
    }
}

impl TryFrom<&Uri> for TableName {
    type Error = ErrorKind;

    fn try_from(uri: &Uri) -> Result<Self> {
        let invalid = || ErrorKind::InvalidUnityCatalogUri(uri.clone());
        if !uri.is_unity_catalog() {
            return Err(invalid());
        }

        let mut parts = uri.path().split('.').map(decode_name_part);
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Some(catalog)), Some(Some(schema)), Some(Some(table)), None) => {
                Ok(Self::new(&catalog, &schema, &table))
            }
            _ => Err(invalid()),
        }
    }
}

/// Decode a percent-encoded name part of a `uc:` URI. Empty parts are invalid.
fn decode_name_part(part: &str) -> Option<String> {
    percent_decode_str(part)
        .decode_utf8()
        .ok()
        .filter(|part| !part.is_empty())
        .map(Cow::into_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_round_trip() {
        let name = TableName::new("Main", "Sales", "Orders");
        let uri = name.to_uri();
        assert_eq!(uri.as_str(), "uc:main.sales.orders");
        assert!(uri.is_unity_catalog());
        assert_eq!(TableName::try_from(&uri).unwrap(), name);
        assert_eq!(name.to_string(), "main.sales.orders");
    }

    #[test]
    fn test_uri_round_trip_with_special_characters() {
        let name = TableName::new("main", "data#lake", "100%_güte?");
        let uri = name.to_uri();
        assert_eq!(uri.as_str(), "uc:main.data%23lake.100%25_g%C3%BCte%3F");
        assert_eq!(TableName::try_from(&uri).unwrap(), name);
    }

    #[test]
    fn test_invalid_uris() {
        for uri in [
            "uc:main.sales",
            "uc:main.sales.orders.extra",
            "uc:main..orders",
            "uc:",
            "https://example.com/main.sales.orders",
        ] {
            let uri = Uri::try_from(uri).unwrap();
            assert!(
                TableName::try_from(&uri).is_err(),
                "{uri} should be invalid"
            );
        }
    }
}
