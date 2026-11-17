//! Check whether tables referenced in SQL code exist in Databricks Unity
//! Catalog.
//!
//! Table references are represented as `uc:catalog.schema.table` URIs (see
//! [`crate::sql`]) and looked up with the [Table Exists] API of a Databricks
//! workspace. Requests go through the shared [`HostPool`], so per-host rate
//! limits, concurrency limits and request deduplication apply as usual.
//!
//! [Table Exists]: https://docs.databricks.com/api/uc-tables/v1/table-exists

use std::{sync::Arc, time::Duration};

use http::{
    Method, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use url::Url;

use crate::{
    ErrorKind, Result, Status, Uri, ratelimit::HostPool, retry::retry_with_backoff, sql::TableName,
};

/// Connection settings for the Unity Catalog API of a Databricks workspace.
#[derive(Debug, Clone)]
pub struct UnityCatalogConfig {
    /// Workspace URL, e.g. `https://adb-1234567890123456.7.azuredatabricks.net`
    host: Url,
    /// Personal access token or OAuth access token
    token: SecretString,
}

impl UnityCatalogConfig {
    /// Create a configuration for the workspace at `host`, which is accessed
    /// with the given `token`.
    ///
    /// `host` is the workspace URL, like the `DATABRICKS_HOST` environment
    /// variable of the Databricks CLI. The `https://` prefix is optional.
    ///
    /// # Errors
    ///
    /// Returns an error if `host` is not a valid URL with a host name.
    pub fn new(host: &str, token: SecretString) -> Result<Self> {
        let invalid = |reason: String| ErrorKind::InvalidDatabricksHost(host.to_string(), reason);

        let trimmed = host.trim();
        let mut url = if trimmed.contains("://") {
            Url::parse(trimmed)
        } else {
            Url::parse(&format!("https://{trimmed}"))
        }
        .map_err(|e| invalid(e.to_string()))?;

        if url.cannot_be_a_base() || url.host_str().is_none_or(str::is_empty) {
            return Err(invalid("a host name is required".to_string()));
        }
        // Workspace URLs sometimes include a workspace ID like `?o=1234`.
        url.set_query(None);
        url.set_fragment(None);

        Ok(Self { host: url, token })
    }

    /// The URL of the "Table Exists" endpoint for the given table.
    fn table_exists_url(&self, table: &TableName) -> Url {
        let mut url = self.host.clone();
        url.path_segments_mut()
            .expect("the workspace URL can be a base")
            .pop_if_empty()
            .extend([
                "api",
                "2.1",
                "unity-catalog",
                "tables",
                &table.to_string(),
                "exists",
            ]);
        url
    }
}

/// Checks `uc:` URIs against the Unity Catalog of a Databricks workspace.
#[derive(Debug, Clone)]
pub(crate) struct UnityCatalogChecker {
    /// The workspace to check tables against. Without it, `uc:` URIs are
    /// unsupported.
    config: Option<UnityCatalogConfig>,
    host_pool: Arc<HostPool>,
    /// Maximum number of retries for transient errors, like rate limiting.
    max_retries: u64,
    /// Initial wait time between retries, which doubles after each retry.
    retry_wait_time: Duration,
}

impl UnityCatalogChecker {
    pub(crate) const fn new(
        config: Option<UnityCatalogConfig>,
        host_pool: Arc<HostPool>,
        max_retries: u64,
        retry_wait_time: Duration,
    ) -> Self {
        Self {
            config,
            host_pool,
            max_retries,
            retry_wait_time,
        }
    }

    /// Check whether the table referenced by a `uc:` URI exists.
    pub(crate) async fn check(&self, uri: &Uri) -> Status {
        let Some(config) = &self.config else {
            return Status::Unsupported(ErrorKind::MissingUnityCatalogConfig);
        };
        let table = match TableName::try_from(uri) {
            Ok(table) => table,
            Err(e) => return e.into(),
        };

        retry_with_backoff(self.max_retries, self.retry_wait_time, || {
            self.check_table(config, &table)
        })
        .await
    }

    async fn check_table(&self, config: &UnityCatalogConfig, table: &TableName) -> Status {
        let url = Uri::from(config.table_exists_url(table));
        let mut request = match self.host_pool.build_request(Method::GET, &url) {
            Ok(request) => request,
            Err(e) => return e.into(),
        };

        let mut authorization =
            match HeaderValue::try_from(format!("Bearer {}", config.token.expose_secret())) {
                Ok(value) => value,
                Err(e) => return ErrorKind::InvalidHeader(e).into(),
            };
        authorization.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, authorization);

        match self.host_pool.execute_request(request, true).await {
            Ok(response) => table_status(
                table,
                response.status,
                response.text.as_deref().unwrap_or_default(),
            ),
            Err(e) => e.into(),
        }
    }
}

/// Response of the "Table Exists" endpoint.
#[derive(Debug, Deserialize)]
struct TableExists {
    /// Omitted if `false`, like all default values in Databricks API responses.
    #[serde(default)]
    table_exists: bool,
}

/// Error response of the Databricks REST API.
#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    error_code: String,
    #[serde(default)]
    message: String,
}

/// Interpret a response of the "Table Exists" endpoint.
fn table_status(table: &TableName, status: StatusCode, body: &str) -> Status {
    if status.is_success() {
        return match serde_json::from_str::<TableExists>(body) {
            Ok(TableExists { table_exists: true }) => Status::Ok(status),
            Ok(TableExists {
                table_exists: false,
            }) => ErrorKind::UnityCatalogTableNotFound(table.to_string(), None).into(),
            Err(e) => ErrorKind::UnityCatalogApi {
                status,
                message: format!("Unexpected response: {e}"),
            }
            .into(),
        };
    }

    let error = serde_json::from_str::<ApiError>(body).ok();
    match error {
        // e.g. `CATALOG_DOES_NOT_EXIST`, `SCHEMA_DOES_NOT_EXIST`
        Some(error)
            if status == StatusCode::NOT_FOUND && error.error_code.ends_with("_DOES_NOT_EXIST") =>
        {
            ErrorKind::UnityCatalogTableNotFound(table.to_string(), Some(error.message)).into()
        }
        Some(error) if !error.error_code.is_empty() || !error.message.is_empty() => {
            ErrorKind::UnityCatalogApi {
                status,
                message: format!("{}: {}", error.error_code, error.message),
            }
            .into()
        }
        _ => ErrorKind::UnityCatalogApi {
            status,
            message: status
                .canonical_reason()
                .unwrap_or("Unknown status code")
                .to_string(),
        }
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use http::StatusCode;
    use secrecy::SecretString;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    use super::{UnityCatalogChecker, UnityCatalogConfig};
    use crate::{ErrorKind, Status, Uri, ratelimit::HostPool, sql::TableName};

    fn checker(server: &MockServer) -> UnityCatalogChecker {
        let config = UnityCatalogConfig::new(&server.uri(), SecretString::from("token")).unwrap();
        UnityCatalogChecker::new(
            Some(config),
            Arc::new(HostPool::default()),
            2,
            Duration::from_millis(1),
        )
    }

    async fn mock_table(server: &MockServer, table: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/2.1/unity-catalog/tables/{table}/exists"
            )))
            .and(header("Authorization", "Bearer token"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn uri(table: &str) -> Uri {
        Uri::try_from(format!("uc:{table}")).unwrap()
    }

    #[test]
    fn test_config_host() {
        let token = || SecretString::from("token");
        let url = |host: &str| {
            UnityCatalogConfig::new(host, token())
                .unwrap()
                .table_exists_url(&TableName::new("Main", "s", "t"))
                .to_string()
        };

        let expected =
            "https://dbc-1.cloud.databricks.com/api/2.1/unity-catalog/tables/main.s.t/exists";
        assert_eq!(url("dbc-1.cloud.databricks.com"), expected);
        assert_eq!(url("https://dbc-1.cloud.databricks.com/"), expected);
        assert_eq!(url(" https://dbc-1.cloud.databricks.com/?o=123 "), expected);
        assert_eq!(
            url("http://localhost:8080/proxy/"),
            "http://localhost:8080/proxy/api/2.1/unity-catalog/tables/main.s.t/exists"
        );

        assert!(UnityCatalogConfig::new("", token()).is_err());
        assert!(UnityCatalogConfig::new("file:///workspace", token()).is_err());
    }

    #[tokio::test]
    async fn test_existing_table() {
        let server = MockServer::start().await;
        let body = serde_json::json!({ "table_exists": true });
        mock_table(
            &server,
            "main.s.t",
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;

        let status = checker(&server).check(&uri("main.s.t")).await;
        assert_eq!(status, Status::Ok(StatusCode::OK));
    }

    #[tokio::test]
    async fn test_missing_table() {
        let server = MockServer::start().await;
        // Databricks omits `false` values from responses.
        mock_table(
            &server,
            "main.s.t",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({})),
        )
        .await;

        let status = checker(&server).check(&uri("main.s.t")).await;
        assert_eq!(
            status,
            Status::Error(ErrorKind::UnityCatalogTableNotFound(
                "main.s.t".into(),
                None
            ))
        );
        assert_eq!(
            status.details(),
            "Table 'main.s.t' not found in Unity Catalog"
        );
    }

    #[tokio::test]
    async fn test_missing_schema() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "error_code": "SCHEMA_DOES_NOT_EXIST",
            "message": "Schema 'main.s' does not exist.",
        });
        mock_table(
            &server,
            "main.s.t",
            ResponseTemplate::new(404).set_body_json(body),
        )
        .await;

        let status = checker(&server).check(&uri("main.s.t")).await;
        assert!(status.is_error());
        assert_eq!(
            status.details(),
            "Table 'main.s.t' not found in Unity Catalog: Schema 'main.s' does not exist."
        );
    }

    #[tokio::test]
    async fn test_permission_denied() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "error_code": "PERMISSION_DENIED",
            "message": "User does not have USE CATALOG on Catalog 'main'.",
        });
        mock_table(
            &server,
            "main.s.t",
            ResponseTemplate::new(403).set_body_json(body),
        )
        .await;

        let status = checker(&server).check(&uri("main.s.t")).await;
        assert!(status.is_error());
        assert_eq!(status.code(), Some(StatusCode::FORBIDDEN));
        assert!(
            status
                .details()
                .contains("PERMISSION_DENIED: User does not have USE CATALOG"),
            "{}",
            status.details()
        );
    }

    #[tokio::test]
    async fn test_retries_rate_limited_requests() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let body = serde_json::json!({ "table_exists": true });
        mock_table(
            &server,
            "main.s.t",
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;

        let status = checker(&server).check(&uri("main.s.t")).await;
        assert_eq!(status, Status::Ok(StatusCode::OK));
    }

    #[tokio::test]
    async fn test_without_config() {
        let checker =
            UnityCatalogChecker::new(None, Arc::new(HostPool::default()), 0, Duration::ZERO);
        let status = checker.check(&uri("main.s.t")).await;
        assert_eq!(
            status,
            Status::Unsupported(ErrorKind::MissingUnityCatalogConfig)
        );
    }

    #[tokio::test]
    async fn test_invalid_uri() {
        let server = MockServer::start().await;
        let status = checker(&server).check(&uri("main.t")).await;
        assert_eq!(
            status,
            Status::Error(ErrorKind::InvalidUnityCatalogUri(uri("main.t")))
        );
    }
}
