//! Integration tests for checking SQL code blocks (`--include-sql`) and the
//! tables they reference (`--unity-catalog`).
#[cfg(test)]
mod sql {
    use assert_cmd::{Command, cargo::cargo_bin_cmd};
    use predicates::{prelude::PredicateBooleanExt, str::contains};
    use serde_json::json;
    use std::path::PathBuf;
    use test_utils::fixtures_path;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    const SYNTAX_ERROR: &str =
        "[ERROR] error: (at 15:6) | SQL syntax error: Expected: end of statement, found: main";

    fn fixture() -> PathBuf {
        fixtures_path!().join("sql").join("TEST_SQL.md")
    }

    /// A `lychee` command without Databricks credentials from the environment
    fn lychee() -> Command {
        let mut cmd = cargo_bin_cmd!();
        cmd.env_remove("DATABRICKS_HOST")
            .env_remove("DATABRICKS_TOKEN");
        cmd
    }

    /// Serve the Unity Catalog "Table Exists" endpoint for the fixture's tables
    async fn unity_catalog_server() -> MockServer {
        let server = MockServer::start().await;
        let table = |name: &str| {
            Mock::given(method("GET"))
                .and(path(format!("/api/2.1/unity-catalog/tables/{name}/exists")))
                .and(header("Authorization", "Bearer secret"))
        };

        for name in ["main.sales.orders", "main.sales.customers"] {
            table(name)
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"table_exists": true})),
                )
                .mount(&server)
                .await;
        }
        // Databricks omits `false` values from responses.
        table("main.sales.refunds")
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        table("legacy.sales.orders")
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error_code": "CATALOG_DOES_NOT_EXIST",
                "message": "Catalog 'legacy' does not exist.",
            })))
            .mount(&server)
            .await;

        server
    }

    #[test]
    fn test_sql_is_not_checked_by_default() {
        lychee()
            .arg(fixture())
            .assert()
            .success()
            .stdout(contains("0 Total"));
    }

    #[test]
    fn test_include_sql() {
        lychee()
            .arg("--include-sql")
            .arg("--sql-dialect=databricks")
            .arg(fixture())
            .assert()
            .failure()
            .code(2)
            .stdout(contains(SYNTAX_ERROR))
            .stdout(contains("1 Error"))
            // Tables are only checked with `--unity-catalog`
            .stdout(contains("uc:").not());
    }

    #[test]
    fn test_dump_include_sql() {
        lychee()
            .args(["--dump", "--include-sql"])
            .arg(fixture())
            .assert()
            .success()
            .stdout("")
            .stderr(contains("Invalid SQL code block"))
            .stderr(contains(
                "at 15:6: SQL syntax error: Expected: end of statement, found: main",
            ));
    }

    #[test]
    fn test_sql_dialect() {
        let markdown = "```sql\nSELECT * FROM main.sales.orders VERSION AS OF 3\n```\n";

        lychee()
            .args(["--include-sql", "--default-extension=md", "-"])
            .write_stdin(markdown)
            .assert()
            .failure()
            .stdout(contains(
                "SQL syntax error: Expected: end of statement, found: AS",
            ));

        lychee()
            .args(["--include-sql", "--sql-dialect=databricks"])
            .args(["--default-extension=md", "-"])
            .write_stdin(markdown)
            .assert()
            .success();
    }

    #[test]
    fn test_sql_languages() {
        let markdown = "```sql\nPICK 1\n```\n\n```SparkSQL\nPICK 2\n```\n";

        lychee()
            .args(["--include-sql", "--sql-languages=sparksql"])
            .args(["--default-extension=md", "-"])
            .write_stdin(markdown)
            .assert()
            .failure()
            .stdout(contains("(at 6:1) | SQL syntax error"))
            .stdout(contains("(at 2:1)").not());
    }

    #[test]
    fn test_dump_unity_catalog_tables() {
        // No credentials are needed, since tables are not looked up. The
        // Databricks dialect is the default with `--unity-catalog`.
        lychee()
            .args(["--dump", "--unity-catalog"])
            .arg(fixture())
            .assert()
            .success()
            .stdout(
                "uc:main.sales.orders\n\
                 uc:main.sales.customers\n\
                 uc:main.sales.refunds\n\
                 uc:legacy.sales.orders\n\
                 uc:main.sales.orders\n",
            )
            .stderr(contains("SQL syntax error"));
    }

    #[tokio::test]
    async fn test_unity_catalog() {
        let server = unity_catalog_server().await;

        lychee()
            .args(["--unity-catalog", "--sql-dialect=databricks"])
            .env("DATABRICKS_HOST", server.uri())
            .env("DATABRICKS_TOKEN", "secret")
            .arg(fixture())
            .assert()
            .failure()
            .code(2)
            .stdout(contains(SYNTAX_ERROR))
            .stdout(contains(
                "[ERROR] uc:main.sales.refunds (at 21:15) | Table 'main.sales.refunds' not found in Unity Catalog",
            ))
            .stdout(contains(
                "[ERROR] uc:legacy.sales.orders (at 27:15) | Table 'legacy.sales.orders' not found in Unity Catalog: Catalog 'legacy' does not exist.",
            ))
            .stdout(contains("6 Total"))
            .stdout(contains("3 OK"))
            .stdout(contains("3 Errors"));
    }

    #[tokio::test]
    async fn test_unity_catalog_exclude() {
        let server = unity_catalog_server().await;

        lychee()
            .args(["--unity-catalog", "--sql-dialect=databricks"])
            .args([
                "--databricks-host",
                &server.uri(),
                "--databricks-token=secret",
            ])
            .args(["--exclude", r"^uc:legacy\."])
            .arg(fixture())
            .assert()
            .failure()
            .stdout(contains("uc:main.sales.refunds"))
            .stdout(contains("uc:legacy.sales.orders").not())
            .stdout(contains("1 Excluded"));
    }

    #[tokio::test]
    async fn test_unity_catalog_invalid_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error_code": "UNAUTHENTICATED",
                "message": "Invalid access token.",
            })))
            .mount(&server)
            .await;

        lychee()
            .args(["--unity-catalog", "--sql-dialect=databricks"])
            .args(["--databricks-host", &server.uri(), "--databricks-token=wrong"])
            .arg(fixture())
            .assert()
            .failure()
            .stdout(contains(
                "[401] uc:main.sales.refunds (at 21:15) | Unity Catalog API error (401 Unauthorized): UNAUTHENTICATED: Invalid access token. Check the Databricks access token",
            ));
    }

    #[test]
    fn test_unity_catalog_requires_credentials() {
        lychee()
            .args([
                "--unity-catalog",
                "--databricks-host=https://example.invalid",
            ])
            .arg(fixture())
            .assert()
            .failure()
            .code(1)
            .stderr(contains(
                "`--unity-catalog` requires a Databricks workspace URL and access token",
            ));
    }

    #[test]
    fn test_unity_catalog_offline() {
        // Tables are not looked up in offline mode, so no credentials are needed.
        lychee()
            .args(["--unity-catalog", "--offline", "--sql-dialect=databricks"])
            .arg(fixture())
            .assert()
            .failure()
            .code(2)
            .stdout(contains(SYNTAX_ERROR))
            .stdout(contains("5 Excluded"));
    }
}
