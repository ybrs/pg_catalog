//! Tests for the small scalar compatibility functions used by information_schema
//! views: the `pg_my_temp_schema`, `getdatabaseencoding`,
//! `pg_relation_is_updatable`, and `_pg_index_position` stubs, plus the computed
//! `information_schema._pg_*` type-precision/length helpers.

use arrow::array::{Array, Int32Array, StringArray};
use datafusion::error::Result as DFResult;

mod common;
use common::base_ctx;

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_my_temp_schema() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let b = ctx
        .sql("SELECT pg_my_temp_schema()")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert_eq!(a.value(0), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_getdatabaseencoding() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let b = ctx
        .sql("SELECT getdatabaseencoding()")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(a.value(0), "UTF8");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_relation_is_updatable() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // Two-arg form, any arg types; returns 0 (not updatable) per row.
    let b = ctx
        .sql("SELECT pg_relation_is_updatable(oid, false) FROM pg_catalog.pg_class LIMIT 3")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert!(a.len() > 0 && (0..a.len()).all(|i| a.value(i) == 0));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_char_max_length_is_null() -> DFResult<()> {
    let ctx = base_ctx().await?;
    let b = ctx
        .sql("SELECT information_schema._pg_char_max_length(23, -1)")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert!(a.is_null(0), "expected NULL char max length");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_format_function() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // %s substitution (the check_constraints view shape), %% literal, %I/%L
    // quoting, and NULL %s rendering as empty string.
    let b = ctx
        .sql(
            "SELECT \
               format('%s IS NOT NULL', 'col') AS a, \
               format('%I = %L', 'my col', 'x''y') AS b, \
               format('100%%') AS c, \
               format('[%s]', CAST(NULL AS TEXT)) AS d",
        )
        .await?
        .collect()
        .await?;
    let col = |i: usize| {
        b[0].column(i)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0)
            .to_string()
    };
    assert_eq!(col(0), "col IS NOT NULL");
    assert_eq!(col(1), "\"my col\" = 'x''y'");
    assert_eq!(col(2), "100%");
    assert_eq!(col(3), "[]", "NULL %s renders as empty string");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_get_function_arg_default_is_null() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // Two-arg form (func oid, argnum); we don't model defaults, so it returns NULL.
    let b = ctx
        .sql("SELECT pg_get_function_arg_default(2619, 1)")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert!(a.is_null(0), "expected NULL parameter default");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_column_is_updatable_is_false() -> DFResult<()> {
    use arrow::array::BooleanArray;
    let ctx = base_ctx().await?;
    // Three-arg form, any arg types; returns false (not updatable) per row, the
    // per-column counterpart of pg_relation_is_updatable's 0.
    let b = ctx
        .sql("SELECT pg_column_is_updatable(oid, 1, false) FROM pg_catalog.pg_class LIMIT 3")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .unwrap();
    assert!(a.len() > 0 && (0..a.len()).all(|i| !a.value(i)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_truetypid_selects_base_for_domains() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // _pg_truetypid(own, typtype, base): returns `base` when typtype = 'd'
    // (a domain), otherwise `own`. Here oids are textual in this catalog.
    let b = ctx
        .sql(
            "SELECT \
               information_schema._pg_truetypid('23', 'd', '1700') AS domain_id, \
               information_schema._pg_truetypid('23', 'b', '1700') AS base_id",
        )
        .await?
        .collect()
        .await?;
    let dom = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let base = b[0]
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(dom.value(0), "1700", "domain picks base type oid");
    assert_eq!(base.value(0), "23", "non-domain picks own type oid");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pg_truetypmod_selects_base_for_domains() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // _pg_truetypmod(own, typtype, base) over int4 typmods.
    let b = ctx
        .sql(
            "SELECT \
               information_schema._pg_truetypmod(CAST(5 AS INT), 'd', CAST(9 AS INT)) AS domain_mod, \
               information_schema._pg_truetypmod(CAST(5 AS INT), 'b', CAST(9 AS INT)) AS base_mod",
        )
        .await?
        .collect()
        .await?;
    let dom = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    let base = b[0]
        .column(1)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    assert_eq!(dom.value(0), 9, "domain picks base typmod");
    assert_eq!(base.value(0), 5, "non-domain picks own typmod");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_information_schema_type_helpers_compute_values() -> DFResult<()> {
    let ctx = base_ctx().await?;
    // For int4 (OID 23) the numeric helpers compute its fixed type facts.
    for (f, expected) in [
        ("information_schema._pg_numeric_precision(23, -1)", 32),
        ("information_schema._pg_numeric_precision_radix(23, -1)", 2),
        ("information_schema._pg_numeric_scale(23, -1)", 0),
    ] {
        let b = ctx.sql(&format!("SELECT {f}")).await?.collect().await?;
        let a = b[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap_or_else(|| panic!("{f} should be Int32"));
        assert_eq!(a.value(0), expected, "{f}");
    }
    // Helpers that don't apply to int4 resolve and return NULL: int4 is neither a
    // character type (octet length) nor a datetime type, and index-position is
    // still a stub.
    for f in [
        "information_schema._pg_char_octet_length(23, -1)",
        "information_schema._pg_datetime_precision(23, -1)",
        "information_schema._pg_index_position(2619, 1)",
    ] {
        let b = ctx.sql(&format!("SELECT {f}")).await?.collect().await?;
        let a = b[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap_or_else(|| panic!("{f} should be Int32"));
        assert!(a.is_null(0), "{f} should be NULL for int4");
    }
    // The interval-type helper is still a NULL text stub.
    let b = ctx
        .sql("SELECT information_schema._pg_interval_type(23, -1)")
        .await?
        .collect()
        .await?;
    let a = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert!(a.is_null(0), "_pg_interval_type should be NULL");
    Ok(())
}
