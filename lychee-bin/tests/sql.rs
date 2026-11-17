//! Integration tests for checking SQL code blocks (`--include-sql`).
#[cfg(test)]
mod sql {
    use assert_cmd::{Command, cargo::cargo_bin_cmd};
    use predicates::{prelude::PredicateBooleanExt, str::contains};
    use std::path::PathBuf;
    use test_utils::fixtures_path;

    const SYNTAX_ERROR: &str =
        "[ERROR] error: (at 15:6) | SQL syntax error: Expected: end of statement, found: main";

    fn fixture() -> PathBuf {
        fixtures_path!().join("sql").join("TEST_SQL.md")
    }

    fn lychee() -> Command {
        cargo_bin_cmd!()
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
            .arg(fixture())
            .assert()
            .failure()
            .code(2)
            .stdout(contains(SYNTAX_ERROR))
            .stdout(contains("1 Error"));
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
        let markdown = "```sql\nSELEC 1\n```\n\n```SparkSQL\nSELEC 2\n```\n";

        lychee()
            .args(["--include-sql", "--sql-languages=sparksql"])
            .args(["--default-extension=md", "-"])
            .write_stdin(markdown)
            .assert()
            .failure()
            .stdout(contains("(at 6:1) | SQL syntax error"))
            .stdout(contains("(at 2:1)").not());
    }
}
