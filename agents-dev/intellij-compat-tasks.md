# Task 71
exec_error query: "/* with T as (\n  select T.oid as oid\n  from pg_catalog.pg_class T\n  where T.relnamespace = :schema_id::oid\n    and T.relkind in ('r', 'm', 'v', 'p', 'f')\n    and T.relname in ( :[*f_names] )\n)\n*/\nselect ind_head.indexrelid index_id,\n       k col_idx,\n       k <= indnkeyatts /* true */ in_key,\n       ind_head.indkey[k-1] column_position,\n       ind_head.indoption[k-1] column_options,\n       ind_head.indcollation[k-1] /* null */ as collation,\n       colln.nspname /* null */ as collation_schema,\n       collname /* null */ as collation_str,\n       ind_head.indclass[k-1] as opclass,\n       case when opcdefault then null else opcn.nspname end as opclass_schema,\n       case when opcdefault then null else opcname end as opclass_str,\n       case\n           when indexprs is null then null\n           when ind_head.indkey[k-1] = 0 then chr(27) || pg_catalog.pg_get_indexdef(ind_head.indexrelid, k::int, true)\n           else pg_catalog.pg_get_indexdef(ind_head.indexrelid, k::int, true)\n       end as expression,\n       amcanorder can_order\nfrom pg_catalog.pg_index /* (select *, pg_catalog.generate_subscripts(indkey::int[], 1) + 1 k from pg_catalog.pg_index) */ ind_head\n         join pg_catalog.pg_class ind_stor\n              on ind_stor.oid = ind_head.indexrelid\ncross join unnest(ind_head.indkey) with ordinality u(u, k)\nleft join pg_catalog.pg_collation\non pg_collation.oid = ind_head.indcollation[k-1]\nleft join pg_catalog.pg_namespace colln on collnamespace = colln.oid\ncross join pg_catalog.pg_indexam_has_property(ind_stor.relam, 'can_order') amcanorder /* left join pg_catalog.pg_am am on ind_stor.relam = am.oid*/\n         left join pg_catalog.pg_opclass\n                   on pg_opclass.oid = ind_head.indclass[k-1]\n         left join pg_catalog.pg_namespace opcn on opcnamespace = opcn.oid\n  --  join T on ind_head.indrelid = T.oid\nwhere ind_stor.relnamespace = $1::oid\n  and ind_stor.relkind in ('i', 'I')\nand pg_catalog.age(ind_stor.xmin) <= coalesce(nullif(greatest(pg_catalog.age($2::varchar::xid), -1), -1), 2147483647)\norder by index_id, k"
exec_error params: Some([Some(b"\0\0\0\0\0\0\x08\x98"), Some(b"\0\0\0\0\0\0\0\0")])
exec_error error: NotImplemented("UNNEST with ordinality is not supported yet")

# Task 71: Gave up
Attempted to implement UNNEST WITH ORDINALITY support but DataFusion's planner lacks this feature and rewriting the query proved too complex.
# Task 72
exec_error query: "   SELECT\n       db.oid as oid, \n       db.datname as name, \n       ta.spcname as spcname, \n       db.datallowconn,\n              16383 as datlastsysoid,\n       has_database_privilege(db.oid, 'CREATE') as cancreate, \n       datdba as owner, \n       db.datistemplate , \n       has_database_privilege(db.datname, 'connect') as canconnect,\n       datistemplate as is_system\n   \n   FROM\n       pg_database db\n       LEFT OUTER JOIN pg_tablespace ta ON db.dattablespace = ta.oid\n      \n   ORDER BY datname;\n   "
exec_error params: None
exec_error error: Collection([Diagnostic(Diagnostic { kind: Error, message: "Invalid function 'has_database_privilege'", span: Some(Span(Location(1,118)..Location(1,140))), notes: [DiagnosticNote { message: "Possible function 'list_any_value'", span: None }], helps: [] }, Plan("Invalid function 'has_database_privilege'.\nDid you mean 'list_any_value'?")), Diagnostic(Diagnostic { kind: Error, message: "Invalid function 'has_database_privilege'", span: Some(Span(Location(1,219)..Location(1,241))), notes: [DiagnosticNote { message: "Possible function 'list_any_value'", span: None }], helps: [] }, Plan("Invalid function 'has_database_privilege'.\nDid you mean 'list_any_value'?"))])
# Task 72: Done
Implemented stub for has_database_privilege returning TRUE for compatibility and added tests.

# Task 73:
exec_error query: "SELECT\n    nsp.oid,\n    nsp.nspname as name,\n    has_schema_privilege(nsp.oid, 'CREATE') as can_create,\n    has_schema_privilege(nsp.oid, 'USAGE') as has_usage,\n    CASE\n    WHEN nsp.nspname like 'pg_%' or nsp.nspname = 'information_schema'\n        THEN true\n    ELSE false END as is_system\nFROM\n    pg_namespace nsp\n    ORDER BY nspname;"
exec_error params: None
exec_error error: Collection([Diagnostic(Diagnostic { kind: Error, message: "Invalid function 'has_schema_privilege'", span: Some(Span(Location(1,49)..Location(1,69))), notes: [DiagnosticNote { message: "Possible function 'has_database_privilege'", span: None }], helps: [] }, Plan("Invalid function 'has_schema_privilege'.\nDid you mean 'has_database_privilege'?")), Diagnostic(Diagnostic { kind: Error, message: "Invalid function 'has_schema_privilege'", span: Some(Span(Location(1,104)..Location(1,124))), notes: [DiagnosticNote { message: "Possible function 'has_database_privilege'", span: None }], helps: [] }, Plan("Invalid function 'has_schema_privilege'.\nDid you mean 'has_database_privilege'?"))])
# Task 73: Done
Implemented stub for has_schema_privilege returning TRUE for compatibility and added tests.


# Task 74:
exec_error query: "SELECT  rel.oid,\n        (SELECT count(*) FROM pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE) AS triggercount,\n        (SELECT count(*) FROM pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE AND tgenabled = 'O') AS has_enable_triggers,\n        (CASE WHEN rel.relkind = 'p' THEN true ELSE false END) AS is_partitioned,\n        nsp.nspname AS schema,\n        nsp.oid AS schemaoid,\n        rel.relname AS name,\n        CASE\n    WHEN nsp.nspname like 'pg_%' or nsp.nspname = 'information_schema'\n        THEN true\n    ELSE false END as is_system\nFROM    pg_class rel\nINNER JOIN pg_namespace nsp ON rel.relnamespace= nsp.oid\n    WHERE rel.relkind IN ('r','t','f','p')\n        AND NOT rel.relispartition\n    ORDER BY nsp.nspname, rel.relname;"
exec_error params: None
exec_error error: Diagnostic(Diagnostic { kind: Error, message: "column 'tgrelid' is ambiguous", span: None, notes: [DiagnosticNote { message: "possible column __cte1.tgrelid", span: None }, DiagnosticNote { message: "possible column __cte2.tgrelid", span: None }], helps: [] }, SchemaError(AmbiguousReference { field: Column { relation: None, name: "tgrelid" } }, Some("")))

We can fix this with simply adding a table alias in all subqueries (if not exists). The query below just works

pgtry=> SELECT
  rel.oid,
  (SELECT count(*) FROM pg_trigger trig WHERE trig.tgrelid = rel.oid AND trig.tgisinternal = FALSE) AS triggercount,
  (SELECT count(*) FROM pg_trigger trig WHERE trig.tgrelid = rel.oid AND trig.tgisinternal = FALSE AND trig.tgenabled = 'O') AS has_enable_triggers,
  (CASE WHEN rel.relkind = 'p' THEN true ELSE false END) AS is_partitioned,
  nsp.nspname AS schema,
  nsp.oid AS schemaoid,
  rel.relname AS name,
  CASE
    WHEN nsp.nspname like 'pg_%' or nsp.nspname = 'information_schema'
    THEN true
    ELSE false END as is_system
FROM pg_class rel
INNER JOIN pg_namespace nsp ON rel.relnamespace = nsp.oid
WHERE rel.relkind IN ('r','t','f','p')
  AND NOT rel.relispartition
ORDER BY nsp.nspname, rel.relname;

# Task 74: Done
Added rewrite that aliases tables inside subqueries to avoid ambiguous column errors. Included integration test executing the original query.
