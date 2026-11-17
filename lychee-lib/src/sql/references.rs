//! Find the tables which SQL statements reference.

use std::collections::HashSet;
use std::ops::ControlFlow;

use log::debug;
use sqlparser::ast::{
    CreateTableLikeKind, ObjectName, Query, Spanned, Statement, TableFactor, TableObject, Use,
    Visit, Visitor,
};
use sqlparser::tokenizer::Location;

use super::table::TableName;

/// A table which a statement reads from or writes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableReference {
    /// The fully qualified table name.
    pub(crate) name: TableName,
    /// Where the table name starts, relative to the code.
    pub(crate) location: Location,
}

/// Name resolution state of a document.
///
/// Like a SQL session, it carries over from one statement and code block to
/// the next: `USE` statements change the current catalog and schema, and
/// tables or views created earlier in the document are not looked up.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    catalog: Option<String>,
    schema: Option<String>,
    /// Tables and views created by the document, both as written and fully
    /// qualified (if possible).
    created: HashSet<String>,
}

impl Scope {
    /// Create a scope with the given current catalog and schema, which are
    /// used to qualify table names that don't specify them.
    pub(crate) fn new(catalog: Option<&str>, schema: Option<&str>) -> Self {
        Self {
            catalog: catalog.map(str::to_lowercase),
            schema: schema.map(str::to_lowercase),
            created: HashSet::new(),
        }
    }

    /// Returns the tables a statement reads from or writes into, which need
    /// to exist. Applies the effects of the statement on the scope, like
    /// changing the current schema with `USE`.
    pub(crate) fn references(&mut self, statement: &Statement) -> Vec<TableReference> {
        if let Statement::Use(statement) = statement {
            self.apply_use(statement);
            return Vec::new();
        }

        let mut relations = Relations::default();
        let _ = statement.visit(&mut relations);

        let mut references = Vec::new();
        for relation in relations.found {
            if self.is_created(&relation.parts) {
                continue;
            }
            match self.resolve(&relation.parts) {
                Some(name) => references.push(TableReference {
                    name,
                    location: relation.location,
                }),
                None => debug!(
                    "Not checking table '{}': cannot determine its catalog and schema",
                    relation.parts.join(".")
                ),
            }
        }

        if let Some(parts) = created_name(statement) {
            if let Some(name) = self.resolve(&parts) {
                self.created.insert(name.to_string());
            }
            self.created.insert(parts.join("."));
        }
        references
    }

    /// Qualify a (lowercase) table name with the current catalog and schema.
    fn resolve(&self, parts: &[String]) -> Option<TableName> {
        match parts {
            [catalog, schema, table] => Some(TableName::new(catalog, schema, table)),
            [schema, table] => Some(TableName::new(self.catalog.as_deref()?, schema, table)),
            [table] => Some(TableName::new(
                self.catalog.as_deref()?,
                self.schema.as_deref()?,
                table,
            )),
            _ => None,
        }
    }

    fn is_created(&self, parts: &[String]) -> bool {
        self.created.contains(&parts.join("."))
            || self
                .resolve(parts)
                .is_some_and(|name| self.created.contains(&name.to_string()))
    }

    fn apply_use(&mut self, statement: &Use) {
        match statement {
            Use::Catalog(name) => {
                if let Some([catalog]) = name_parts(name).as_deref() {
                    self.catalog = Some(catalog.clone());
                    // Databricks resets the current schema with the catalog.
                    self.schema = Some("default".to_string());
                }
            }
            Use::Schema(name) | Use::Database(name) | Use::Object(name) => {
                match name_parts(name).as_deref() {
                    Some([catalog, schema]) => {
                        self.catalog = Some(catalog.clone());
                        self.schema = Some(schema.clone());
                    }
                    Some([schema]) => self.schema = Some(schema.clone()),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// A table name found in a statement, before name resolution.
#[derive(Debug)]
struct Relation {
    /// The lowercase parts of the name, e.g. `["sales", "orders"]`.
    parts: Vec<String>,
    location: Location,
}

/// Collects the tables a statement reads from or writes into.
///
/// These are the tables in `FROM` and `JOIN` clauses (including subqueries),
/// `MERGE` sources, the sources of `CREATE TABLE ... LIKE` and `CLONE`, and
/// the targets of `INSERT`, `UPDATE`, `DELETE` and `MERGE`. Tables which are
/// only named by DDL statements (`CREATE`, `ALTER`, `DROP`, ...) are not
/// collected, and neither are table-valued functions or references to common
/// table expressions.
#[derive(Debug, Default)]
struct Relations {
    /// The common table expressions of the queries being visited, innermost
    /// last.
    ctes: Vec<CommonTableExpressions>,
    found: Vec<Relation>,
}

/// The common table expressions (CTEs) of a query's `WITH` clause.
#[derive(Debug)]
struct CommonTableExpressions {
    /// The lowercase names of the CTEs.
    names: Vec<String>,
    /// The queries of the CTEs, to recognize them while visiting. They are
    /// only compared by address and never dereferenced.
    queries: Vec<*const Query>,
    recursive: bool,
    /// How many CTEs are visible at the current position. The query body sees
    /// all CTEs, while the query of a CTE only sees the CTEs before it (unless
    /// the `WITH` clause is `RECURSIVE`).
    visible: usize,
}

impl CommonTableExpressions {
    fn new(query: &Query) -> Self {
        let ctes = query.with.as_ref().map_or(&[][..], |with| &with.cte_tables);
        Self {
            names: ctes
                .iter()
                .map(|cte| cte.alias.name.value.to_lowercase())
                .collect(),
            queries: ctes.iter().map(|cte| &raw const *cte.query).collect(),
            recursive: query.with.as_ref().is_some_and(|with| with.recursive),
            visible: ctes.len(),
        }
    }

    /// The index of the CTE whose query is `query`, if any.
    fn position(&self, query: &Query) -> Option<usize> {
        self.queries
            .iter()
            .position(|&cte_query| std::ptr::eq(cte_query, query))
    }

    fn contains(&self, name: &str) -> bool {
        self.names[..self.visible].iter().any(|cte| cte == name)
    }
}

impl Relations {
    fn add(&mut self, name: &ObjectName) {
        let Some(parts) = name_parts(name) else {
            return;
        };

        // Names like `` delta.`/data/events` `` refer to files, not tables.
        if !parts.iter().all(|part| is_valid_name_part(part)) {
            debug!("Not checking table '{name}': not a Unity Catalog name");
            return;
        }

        if let [table] = parts.as_slice()
            && self.ctes.iter().any(|ctes| ctes.contains(table))
        {
            return;
        }

        self.found.push(Relation {
            parts,
            location: name.span().start,
        });
    }
}

impl Visitor for Relations {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        // Entering the query of a CTE, which can only see the CTEs before it.
        if let Some(outer) = self.ctes.last_mut()
            && let Some(index) = outer.position(query)
        {
            outer.visible = if outer.recursive {
                outer.names.len()
            } else {
                index
            };
        }

        self.ctes.push(CommonTableExpressions::new(query));
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.ctes.pop();

        // Leaving the query of a CTE. The rest of the outer query sees all CTEs.
        if let Some(outer) = self.ctes.last_mut()
            && outer.position(query).is_some()
        {
            outer.visible = outer.names.len();
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        // Table-valued functions like `range(10)` have arguments.
        if let TableFactor::Table {
            name, args: None, ..
        } = table_factor
        {
            self.add(name);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        match statement {
            Statement::Insert(insert) => {
                if let TableObject::TableName(name) = &insert.table {
                    self.add(name);
                }
            }
            Statement::CreateTable(create) => {
                if let Some(
                    CreateTableLikeKind::Plain(like) | CreateTableLikeKind::Parenthesized(like),
                ) = &create.like
                {
                    self.add(&like.name);
                }
                if let Some(source) = &create.clone {
                    self.add(source);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// The name of the table or view a statement creates, if any.
fn created_name(statement: &Statement) -> Option<Vec<String>> {
    let name = match statement {
        Statement::CreateTable(create) => &create.name,
        Statement::CreateView(create) => &create.name,
        Statement::Cache { table_name, .. } => table_name,
        _ => return None,
    };
    name_parts(name)
}

/// The lowercase parts of a name, unless a part is not a plain identifier.
fn name_parts(name: &ObjectName) -> Option<Vec<String>> {
    name.0
        .iter()
        .map(|part| part.as_ident().map(|ident| ident.value.to_lowercase()))
        .collect()
}

/// Unity Catalog names cannot contain periods, spaces, slashes, or control
/// characters.
fn is_valid_name_part(part: &str) -> bool {
    !part.is_empty()
        && !part
            .chars()
            .any(|c| c == '.' || c == '/' || c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sqlparser::dialect::DatabricksDialect;

    use super::*;
    use crate::sql::parse::parse;

    /// Fully qualified names of the tables referenced by valid statements.
    fn tables(sql: &str) -> Vec<String> {
        tables_in_scope(sql, &mut Scope::default())
    }

    fn tables_in_scope(sql: &str, scope: &mut Scope) -> Vec<String> {
        parse(sql, &DatabricksDialect {})
            .into_iter()
            .flat_map(|statement| {
                let statement =
                    statement.unwrap_or_else(|e| panic!("Unexpected syntax error: {}", e.message));
                scope.references(&statement)
            })
            .map(|reference| reference.name.to_string())
            .collect()
    }

    #[test]
    fn test_table_locations() {
        let sql =
            "SELECT *\nFROM main.sales.orders o\n  JOIN `Main`.sales.customers c ON o.id = c.id";
        let statement = parse(sql, &DatabricksDialect {}).remove(0).unwrap();
        let locations: Vec<_> = Scope::default()
            .references(&statement)
            .into_iter()
            .map(|reference| {
                let location = reference.location;
                (reference.name.to_string(), location.line, location.column)
            })
            .collect();
        assert_eq!(
            locations,
            [
                ("main.sales.orders".to_string(), 2, 6),
                ("main.sales.customers".to_string(), 3, 8),
            ]
        );
    }

    #[test]
    fn test_tables_in_queries() {
        assert_eq!(
            tables(
                "SELECT * FROM main.a.t1
                 WHERE id IN (SELECT id FROM main.a.t2)
                   AND EXISTS (SELECT 1 FROM main.a.t3 LEFT ANTI JOIN main.a.t4 USING (id))"
            ),
            ["main.a.t1", "main.a.t2", "main.a.t3", "main.a.t4"]
        );
        assert_eq!(
            tables("SELECT * FROM main.a.t VERSION AS OF 3"),
            ["main.a.t"]
        );
        assert_eq!(
            tables("SELECT * FROM main.a.t PIVOT (sum(v) FOR k IN ('x', 'y'))"),
            ["main.a.t"]
        );
    }

    #[test]
    fn test_tables_in_dml() {
        assert_eq!(
            tables("INSERT INTO main.a.target SELECT * FROM main.a.source"),
            ["main.a.target", "main.a.source"]
        );
        assert_eq!(
            tables(
                "MERGE INTO main.a.target t USING main.a.source s ON t.id = s.id
                 WHEN MATCHED THEN UPDATE SET * WHEN NOT MATCHED THEN INSERT *"
            ),
            ["main.a.target", "main.a.source"]
        );
        assert_eq!(tables("UPDATE main.a.t SET x = 1"), ["main.a.t"]);
        assert_eq!(tables("DELETE FROM main.a.t WHERE x = 1"), ["main.a.t"]);
    }

    #[test]
    fn test_ddl_targets_are_not_checked() {
        assert_eq!(
            tables("CREATE TABLE main.a.new AS SELECT * FROM main.a.old"),
            ["main.a.old"]
        );
        assert_eq!(
            tables("DROP TABLE IF EXISTS main.a.t"),
            Vec::<String>::new()
        );
        assert_eq!(
            tables("ALTER TABLE main.a.t SET TBLPROPERTIES ('a' = 'b')"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_sources_of_created_tables_are_checked() {
        assert_eq!(
            tables("CREATE TABLE main.a.new LIKE main.a.old"),
            ["main.a.old"]
        );
        assert_eq!(
            tables("CREATE TABLE main.a.new CLONE main.a.old"),
            ["main.a.old"]
        );
    }

    #[test]
    fn test_created_tables_are_not_checked() {
        let mut scope = Scope::default();
        assert_eq!(
            tables_in_scope(
                "CREATE TEMPORARY VIEW recent AS SELECT * FROM main.a.events;
                 SELECT * FROM recent;
                 CREATE TABLE main.a.copy AS SELECT * FROM recent;
                 INSERT INTO main.a.copy SELECT * FROM main.a.events",
                &mut scope
            ),
            ["main.a.events", "main.a.events"]
        );
        // Created tables carry over to other code blocks of the document.
        assert_eq!(
            tables_in_scope("SELECT * FROM main.a.copy CROSS JOIN recent", &mut scope),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_ctes_are_not_checked() {
        assert_eq!(
            tables(
                "WITH a AS (SELECT * FROM main.s.t1), b AS (SELECT * FROM a)
                 SELECT * FROM b JOIN main.s.b USING (id)"
            ),
            ["main.s.t1", "main.s.b"]
        );
        // A CTE is only in scope within its query.
        let mut scope = Scope::new(Some("main"), Some("s"));
        assert_eq!(
            tables_in_scope(
                "SELECT * FROM (WITH x AS (SELECT 1) SELECT * FROM x) CROSS JOIN x",
                &mut scope
            ),
            ["main.s.x"]
        );
    }

    #[test]
    fn test_cte_queries_only_see_preceding_ctes() {
        let mut scope = Scope::new(Some("main"), Some("s"));
        // `orders` in the CTE's own query is the table, not the CTE.
        assert_eq!(
            tables_in_scope(
                "WITH orders AS (SELECT * FROM orders WHERE amount > 0) SELECT * FROM orders",
                &mut scope
            ),
            ["main.s.orders"]
        );
        // `b` is the table in the first CTE, and the CTE in the second.
        assert_eq!(
            tables_in_scope(
                "WITH a AS (SELECT * FROM b), b AS (SELECT * FROM b), c AS (SELECT * FROM b)
                 SELECT * FROM a, b, c",
                &mut scope
            ),
            ["main.s.b", "main.s.b"]
        );
        // Subqueries of a CTE's query see the same CTEs.
        assert_eq!(
            tables_in_scope(
                "WITH a AS (SELECT 1), b AS (SELECT * FROM (SELECT * FROM a JOIN c))
                 SELECT * FROM b",
                &mut scope
            ),
            ["main.s.c"]
        );
    }

    #[test]
    fn test_recursive_ctes_see_themselves() {
        assert_eq!(
            tables(
                "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM t WHERE n < 5)
                 SELECT * FROM t"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_non_table_relations_are_ignored() {
        assert_eq!(tables("SELECT * FROM range(10)"), Vec::<String>::new());
        assert_eq!(
            tables("SELECT * FROM read_files('s3://bucket/path', format => 'json')"),
            Vec::<String>::new()
        );
        assert_eq!(
            tables("SELECT * FROM delta.`/mnt/data/events`"),
            Vec::<String>::new()
        );
        assert_eq!(
            tables("SELECT * FROM VALUES (1), (2) AS t(x)"),
            Vec::<String>::new()
        );
        assert_eq!(
            tables("SELECT * FROM IDENTIFIER(:table) JOIN main.s.t USING (id)"),
            ["main.s.t"]
        );
    }

    #[test]
    fn test_unqualified_names_need_a_catalog_and_schema() {
        assert_eq!(
            tables("SELECT * FROM orders JOIN sales.customers"),
            Vec::<String>::new()
        );

        let mut scope = Scope::new(Some("Main"), None);
        assert_eq!(
            tables_in_scope("SELECT * FROM orders JOIN sales.customers", &mut scope),
            ["main.sales.customers"]
        );

        let mut scope = Scope::new(Some("main"), Some("sales"));
        assert_eq!(
            tables_in_scope("SELECT * FROM orders JOIN crm.customers", &mut scope),
            ["main.sales.orders", "main.crm.customers"]
        );
    }

    #[test]
    fn test_use_statements() {
        let mut scope = Scope::default();
        assert_eq!(
            tables_in_scope(
                "USE CATALOG prod; SELECT * FROM t;
                 USE SCHEMA sales; SELECT * FROM t;
                 USE dev.crm; SELECT * FROM t;",
                &mut scope
            ),
            ["prod.default.t", "prod.sales.t", "dev.crm.t"]
        );
        // The current schema carries over to other code blocks of the document.
        assert_eq!(
            tables_in_scope("SELECT * FROM t", &mut scope),
            ["dev.crm.t"]
        );
    }
}
