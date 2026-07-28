// SQL transformation utilities for aliasing columns.
// Walks parsed queries and assigns unique aliases to avoid duplicate column names.
// Included so result sets match PostgreSQL naming expectations.

use datafusion::error::{DataFusionError, Result};
use sqlparser::ast::{
    visit_statements_mut, DataType, Expr, Ident, Query, Select, SelectItem, SetExpr, Statement,
    TableFactor,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::ops::ControlFlow;

/// Give every projected expression in `select` a unique `alias_N` name and record
/// the name `PostgreSQL` would have reported for it in `alias_map`.
///
/// `counter` is threaded through the whole statement so aliases stay unique across
/// every projection that gets rewritten. Wildcards are left untouched because their
/// column list is only known after planning, so there is nothing to alias.
fn alias_projection(
    select: &mut Select,
    counter: &mut usize,
    alias_map: &mut HashMap<String, String>,
) {
    let mut aliased_projection = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => match expr {
                Expr::Cast {
                    expr: _inner_expr,
                    data_type,
                    ..
                } => match data_type {
                    DataType::Regclass => {
                        let alias = format!("alias_{}", *counter);
                        *counter += 1;
                        let name = "regclass";
                        // here obj is
                        // obj: ObjectName([Identifier(Ident { value: "oid", quote_style: None, span: Span(Location(1,35)..Location(1,38)) })])
                        // find the value "oid" and put it to name
                        alias_map.insert(alias.clone(), name.into());

                        aliased_projection.push(SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: Ident::new(alias),
                        });
                    }
                    DataType::Custom(obj, _) if obj.0.len() == 1 => {
                        let alias = format!("alias_{}", *counter);
                        *counter += 1;
                        let name = obj
                            .0
                            .last()
                            .and_then(|part| part.as_ident())
                            .map_or_else(|| "?column?".to_string(), |ident| ident.value.clone());

                        // here obj is
                        // obj: ObjectName([Identifier(Ident { value: "oid", quote_style: None, span: Span(Location(1,35)..Location(1,38)) })])
                        // find the value "oid" and put it to name
                        alias_map.insert(alias.clone(), name);

                        aliased_projection.push(SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: Ident::new(alias),
                        });
                    }
                    _ => {
                        let alias = format!("alias_{}", *counter);
                        *counter += 1;
                        alias_map.insert(alias.clone(), data_type.to_string().to_lowercase());
                        aliased_projection.push(SelectItem::ExprWithAlias {
                            expr: expr.clone(),
                            alias: Ident::new(alias),
                        });
                    }
                },
                Expr::Function(f) => {
                    let alias = format!("alias_{}", *counter);
                    *counter += 1;
                    let name = f.clone().name.to_string();
                    alias_map.insert(alias.clone(), name);

                    aliased_projection.push(SelectItem::ExprWithAlias {
                        expr: expr.clone(),
                        alias: Ident::new(alias),
                    });
                }

                Expr::Wildcard(_) | Expr::QualifiedWildcard(_, _) => {
                    aliased_projection.push(SelectItem::UnnamedExpr(expr.clone()));
                }
                _ => {
                    let alias = format!("alias_{}", *counter);
                    *counter += 1;

                    let name = match expr {
                        Expr::CompoundIdentifier(segments) => segments
                            .last()
                            .map_or("?column?".to_string(), |id| id.value.clone()),
                        Expr::Identifier(id) => id.value.clone(),
                        _ => "?column?".to_string(),
                    };

                    alias_map.insert(alias.clone(), name);

                    aliased_projection.push(SelectItem::ExprWithAlias {
                        expr: expr.clone(),
                        alias: Ident::new(alias),
                    });
                }
            },
            _ => aliased_projection.push(item.clone()),
        }
    }
    select.projection = aliased_projection;
}

/// Alias the projection of a set expression, and descend into any derived tables
/// it selects from.
///
/// Only `depth == 0` projections are aliased: those are the columns the client
/// sees and whose names [`restore_aliased_column_names`] can map back. Renaming
/// nested projections would break references to them from the enclosing query.
fn alias_columns_in_set_expr(
    expr: &mut SetExpr,
    counter: &mut usize,
    alias_map: &mut HashMap<String, String>,
    depth: usize,
) {
    match expr {
        SetExpr::Select(select) => {
            if depth == 0 {
                alias_projection(select, counter, alias_map);
            }
            for table_with_joins in &mut select.from {
                if let TableFactor::Derived { subquery, .. } = &mut table_with_joins.relation {
                    alias_columns_in_query(subquery, counter, alias_map, depth + 1);
                }
            }
        }

        SetExpr::SetOperation { left, right, .. } => {
            alias_columns_in_set_expr(left, counter, alias_map, depth);
            alias_columns_in_set_expr(right, counter, alias_map, depth);
        }
        SetExpr::Query(subquery) => {
            alias_columns_in_query(subquery, counter, alias_map, depth + 1);
        }
        _ => {}
    }
}

/// Run [`alias_columns_in_set_expr`] over a query's body and over each of its CTE
/// bodies, which are one level deeper than the query that defines them.
fn alias_columns_in_query(
    query: &mut Query,
    counter: &mut usize,
    alias_map: &mut HashMap<String, String>,
    depth: usize,
) {
    alias_columns_in_set_expr(&mut query.body, counter, alias_map, depth);

    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            alias_columns_in_query(&mut cte.query, counter, alias_map, depth + 1);
        }
    }
}

/// The client-visible name a projection item produces, for plain column refs.
/// Returns `None` for wildcards or complex expressions we don't disambiguate.
fn projection_output_name(item: &SelectItem) -> Option<String> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
        SelectItem::UnnamedExpr(Expr::Identifier(id)) => Some(id.value.clone()),
        SelectItem::UnnamedExpr(Expr::CompoundIdentifier(segs)) => {
            segs.last().map(|id| id.value.clone())
        }
        _ => None,
    }
}

/// Disambiguate duplicate column names within a single SELECT's projection by
/// aliasing the second and later occurrences (`nspname`, `nspname`, ... ->
/// `nspname`, `nspname_2`, ...).
fn dedup_projection(select: &mut Select) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for item in &mut select.projection {
        let Some(name) = projection_output_name(item) else {
            continue;
        };
        let count = seen.entry(name.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            let expr = match item {
                SelectItem::UnnamedExpr(e) => e.clone(),
                SelectItem::ExprWithAlias { expr, .. } => expr.clone(),
                _ => continue,
            };
            *item = SelectItem::ExprWithAlias {
                expr,
                alias: Ident::new(format!("{name}_{count}")),
            };
        }
    }
}

/// Walk into nested `SELECT`s (derived tables, set-operation branches, CTEs) and
/// disambiguate duplicate projection names in each. The top level (`depth == 0`)
/// is skipped - [`alias_unnamed_columns`] already aliases it, and renaming
/// top-level columns would change the client-facing result names.
fn dedup_in_set_expr(expr: &mut SetExpr, depth: usize) {
    match expr {
        SetExpr::Select(select) => {
            if depth > 0 {
                dedup_projection(select);
            }
            for table_with_joins in &mut select.from {
                if let TableFactor::Derived { subquery, .. } = &mut table_with_joins.relation {
                    dedup_in_query(subquery, depth + 1);
                }
                for join in &mut table_with_joins.joins {
                    if let TableFactor::Derived { subquery, .. } = &mut join.relation {
                        dedup_in_query(subquery, depth + 1);
                    }
                }
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            dedup_in_set_expr(left, depth);
            dedup_in_set_expr(right, depth);
        }
        SetExpr::Query(subquery) => dedup_in_query(subquery, depth + 1),
        _ => {}
    }
}

/// Recurse [`dedup_in_set_expr`] through a query body and its CTEs.
fn dedup_in_query(query: &mut Query, depth: usize) {
    dedup_in_set_expr(&mut query.body, depth);
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            dedup_in_query(&mut cte.query, depth + 1);
        }
    }
}

/// Disambiguate duplicate column names inside *nested* SELECT projections.
///
/// `DataFusion`'s optimizer asserts a column's name matches its projection
/// expression and panics ("Internal error: Assertion failed: `col.name()` ==
/// `matching_name`") when a derived table projects two columns with the same
/// name, for example `SELECT nr.nspname, ..., nc.nspname` in
/// `constraint_column_usage`. The top-level projection is already disambiguated
/// by [`alias_unnamed_columns`]; this covers the nested case it skips. Inner
/// names aren't client-visible (they're under a column-alias list, or a duplicate
/// name was unreferenceable anyway), so no rename-back is needed.
///
/// # Errors
///
/// Returns [`DataFusionError::External`] if `sql` does not parse as `PostgreSQL`
/// dialect SQL.
pub fn disambiguate_duplicate_columns(sql: &str) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;
    let _ = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(query) = stmt {
            dedup_in_query(query, 0);
        }
        ControlFlow::<()>::Continue(())
    });
    Ok(statements
        .into_iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Assign unique aliases to every projected column and return a map
/// of alias to original name so duplicate column names do not confuse
/// clients.
///
/// # Errors
///
/// Returns [`DataFusionError::External`] if `sql` does not parse as `PostgreSQL`
/// dialect SQL.
pub fn alias_unnamed_columns(sql: &str) -> Result<(String, HashMap<String, String>)> {
    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;

    let mut alias_map = HashMap::new();
    let mut counter = 1;

    let _ = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(query) = stmt {
            alias_columns_in_query(query, &mut counter, &mut alias_map, 0);
        }
        ControlFlow::<()>::Continue(())
    });

    let res = statements
        .into_iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    log::debug!("result: {res:?} alias_map: {alias_map:?}");

    Ok((res, alias_map))
}

/// Rename a top-level projection's `alias_N` columns back to their real names,
/// using the map [`alias_unnamed_columns`] produced.
///
/// `alias_unnamed_columns` renames every unnamed top-level column to a unique
/// `alias_N` so duplicate names cannot confuse `DataFusion`, and the real names are
/// restored on the result schema sent to the client. A `CREATE VIEW` body keeps its
/// projection names as the view's schema, so a view built from the aliased SQL would
/// expose `alias_N` to everyone reading it. Applying this before `CREATE VIEW` gives
/// the view its real `PostgreSQL` column names. Aliases happen only at the top level
/// (`alias_columns_in_set_expr` aliases at `depth == 0`), so only the outermost
/// projection - and each branch of a top-level set operation - is restored.
///
/// # Errors
///
/// Returns [`DataFusionError::External`] if `sql` does not parse as `PostgreSQL`
/// dialect SQL.
pub fn restore_aliased_column_names<S: BuildHasher>(
    sql: &str,
    alias_map: &HashMap<String, String, S>,
) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| DataFusionError::External(Box::new(e)))?;
    let _ = visit_statements_mut(&mut statements, |stmt| {
        if let Statement::Query(query) = stmt {
            restore_in_set_expr(&mut query.body, alias_map);
        }
        ControlFlow::<()>::Continue(())
    });
    Ok(statements
        .into_iter()
        .map(|stmt| stmt.to_string())
        .collect::<Vec<_>>()
        .join(" "))
}

/// Restore the real name of each top-level projection column whose alias is an
/// `alias_N` key in `alias_map`, recursing only into the branches of a top-level
/// set operation (where the aliases also live).
fn restore_in_set_expr<S: BuildHasher>(expr: &mut SetExpr, alias_map: &HashMap<String, String, S>) {
    match expr {
        SetExpr::Select(select) => {
            for item in &mut select.projection {
                if let SelectItem::ExprWithAlias { alias, .. } = item {
                    if let Some(real_name) = alias_map.get(&alias.value) {
                        *alias = Ident::new(real_name.clone());
                    }
                }
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            restore_in_set_expr(left, alias_map);
            restore_in_set_expr(right, alias_map);
        }
        SetExpr::Query(subquery) => restore_in_set_expr(&mut subquery.body, alias_map),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    /// A derived table projecting the same name twice gets its second occurrence
    /// aliased, while the outer projection keeps its client-facing names.
    #[test]
    fn test_disambiguate_duplicate_columns_in_derived_table() -> Result<(), Box<dyn Error>> {
        // Two `nspname`s in a derived table -> the second becomes `nspname_2`,
        // so DataFusion doesn't hit its name-mismatch assertion. The outer
        // (depth 0) projection is left alone.
        let sql = "SELECT a, b FROM (SELECT nr.nspname, nc.nspname FROM x nr, y nc) t(a, b)";
        let out = disambiguate_duplicate_columns(sql)?;
        assert!(
            out.contains("nc.nspname AS nspname_2"),
            "second nspname aliased: {out}"
        );
        assert_eq!(
            out.matches("nspname").count(),
            3,
            "only the dup is renamed: {out}"
        );
        Ok(())
    }

    /// A nested projection whose names are already unique is passed through
    /// unchanged, so no query pays for aliases it does not need.
    #[test]
    fn test_disambiguate_leaves_unique_columns_untouched() -> Result<(), Box<dyn Error>> {
        let sql = "SELECT x FROM (SELECT a.p, b.q FROM a, b) t";
        let out = disambiguate_duplicate_columns(sql)?;
        assert!(
            !out.contains("_2"),
            "no aliasing when names are unique: {out}"
        );
        Ok(())
    }

    /// Build the alias map `alias_unnamed_columns` is expected to produce, mapping
    /// `alias_1`, `alias_2`, ... to the given original column names in order.
    fn alias_maps(nums: &[&str]) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for (i, &val) in nums.iter().enumerate() {
            let key = format!("alias_{}", i + 1);
            map.insert(key, val.to_string());
        }
        map
    }

    /// One `alias_unnamed_columns` expectation: the input SQL, the substrings the
    /// rewritten SQL must contain, and the alias map the rewrite must return.
    type AliasCase = (&'static str, Vec<&'static str>, HashMap<String, String>);

    /// The cases exercised by `test_alias_unnamed_columns`, kept next to the test
    /// but out of its body so each new case does not grow the test itself.
    fn alias_unnamed_columns_cases() -> Vec<AliasCase> {
        vec![
            (
                "SELECT t.id FROM foo",
                vec!["SELECT t.id AS alias_1", "FROM foo"],
                alias_maps(&["id"]),
            ),
            (
                "SELECT t.id AS f FROM foo",
                vec!["SELECT t.id AS f FROM foo"], // Should stay the same
                alias_maps(&[]),                   // empty alias
            ),
            (
                "SELECT t.* FROM foo",
                vec!["SELECT t.* FROM foo"], // No aliasing needed
                alias_maps(&[]),             // empty alias
            ),
            (
                "SELECT t.id, t.* FROM foo",
                vec!["SELECT t.id AS alias_1, t.* FROM foo"], // Only t.id gets alias
                alias_maps(&["id"]),
            ),
            (
                "SELECT 1 FROM foo",
                vec!["SELECT 1 AS alias_1", "FROM foo"], // literal should also get alias
                alias_maps(&["?column?"]), // postgresql also returns ?column? in this case
            ),
            (
                "SELECT 1, 1 FROM foo",
                vec!["SELECT 1 AS alias_1, 1 AS alias_2 FROM foo"], // literal should also get alias
                alias_maps(&["?column?", "?column?"]), // postgresql also returns ?column? in this case
            ),
            (
                "SELECT t.id + 1 FROM foo",
                vec!["SELECT t.id + 1 AS", "FROM foo"], // expressions get alias
                alias_maps(&["?column?"]),
            ),
            (
                "WITH cte AS (SELECT t.a FROM t) SELECT * FROM cte",
                vec!["SELECT t.a FROM t", "SELECT * FROM cte"],
                alias_maps(&[]),
            ),
            (
                "select * from (SELECT t.a FROM t)",
                vec!["SELECT t.a FROM t"],
                alias_maps(&[]),
            ),
            (
                "select * from (SELECT t.a, t.b FROM t) T1",
                vec!["SELECT t.a, t.b FROM t"],
                alias_maps(&[]),
            ),
            (
                "select 'pg_constraint'::regclass::oid",
                vec!["SELECT 'pg_constraint'::REGCLASS::oid AS alias_1"],
                alias_maps(&["oid"]),
            ),
            (
                "select '1'::int4;",
                vec!["SELECT '1'::INT4 AS alias_1"],
                alias_maps(&["int4"]),
            ),
            (
                "select '1'::int8;",
                vec!["SELECT '1'::INT8 AS alias_1"],
                alias_maps(&["int8"]),
            ),
            (
                "select '1'::varchar;",
                vec!["SELECT '1'::VARCHAR AS alias_1"],
                alias_maps(&["varchar"]),
            ),
            (
                "select '1'::varchar(120);",
                vec!["SELECT '1'::VARCHAR(120) AS alias_1"],
                alias_maps(&["varchar(120)"]),
            ),
            (
                "select 'pg_constraint'::regclass",
                vec!["SELECT 'pg_constraint'::REGCLASS AS alias_1"],
                alias_maps(&["regclass"]),
            ),
            (
                "select substr('foo', 1, 2)",
                vec!["SELECT SUBSTR('foo', 1, 2) AS "],
                alias_maps(&["?column?"]),
            ),
        ]
    }

    /// Every projected expression that carries no explicit alias is renamed to a
    /// unique `alias_N`, and the returned map reports the name `PostgreSQL` would
    /// have given it. Wildcards and already-aliased columns are left alone.
    #[test]
    fn test_alias_unnamed_columns() {
        for (input, expected_substrings, expected_alias_map) in alias_unnamed_columns_cases() {
            let (transformed, aliases) = alias_unnamed_columns(input).unwrap();
            for expected in expected_substrings {
                assert!(
                    transformed.contains(expected),
                    "Expected substring not found:\ninput: {input}\nexpected: {expected}\nactual: {transformed}"
                );
                assert_eq!(
                    aliases, expected_alias_map,
                    "alias maps failed input: {input} expected {expected_alias_map:?} actual {aliases:?}"
                );
            }
        }
    }

    /// Aliasing then restoring round-trips back to the original column names, so a
    /// view created from the rewritten SQL exposes the names clients expect.
    #[test]
    fn test_restore_aliased_column_names_recovers_real_names() -> Result<(), Box<dyn Error>> {
        // A qualified ref and an unqualified ref both lose their name to alias_N;
        // restoring gives the real PostgreSQL names a view must expose.
        let (aliased, map) = alias_unnamed_columns("SELECT a.rolname, usename FROM t")?;
        assert!(aliased.contains("AS alias_1") && aliased.contains("AS alias_2"));
        let restored = restore_aliased_column_names(&aliased, &map)?;
        assert!(
            restored.contains("AS rolname") && restored.contains("AS usename"),
            "real names restored: {restored}"
        );
        assert!(
            !restored.contains("alias_"),
            "no alias_N remains: {restored}"
        );
        Ok(())
    }

    /// A column the query already named with `AS` never gets an `alias_N`, so
    /// restoring must leave that name exactly as written.
    #[test]
    fn test_restore_leaves_explicit_aliases_untouched() -> Result<(), Box<dyn Error>> {
        // A column the body already named with AS is never aliased, so restore is a
        // no-op for it.
        let (aliased, map) = alias_unnamed_columns("SELECT '*'::text AS passwd FROM t")?;
        let restored = restore_aliased_column_names(&aliased, &map)?;
        assert!(
            restored.contains("AS passwd"),
            "explicit alias kept: {restored}"
        );
        assert!(!restored.contains("alias_"), "no alias_N: {restored}");
        Ok(())
    }
}
