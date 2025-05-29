# Task 71
exec_error query: "/* with T as (\n  select T.oid as oid\n  from pg_catalog.pg_class T\n  where T.relnamespace = :schema_id::oid\n    and T.relkind in ('r', 'm', 'v', 'p', 'f')\n    and T.relname in ( :[*f_names] )\n)\n*/\nselect ind_head.indexrelid index_id,\n       k col_idx,\n       k <= indnkeyatts /* true */ in_key,\n       ind_head.indkey[k-1] column_position,\n       ind_head.indoption[k-1] column_options,\n       ind_head.indcollation[k-1] /* null */ as collation,\n       colln.nspname /* null */ as collation_schema,\n       collname /* null */ as collation_str,\n       ind_head.indclass[k-1] as opclass,\n       case when opcdefault then null else opcn.nspname end as opclass_schema,\n       case when opcdefault then null else opcname end as opclass_str,\n       case\n           when indexprs is null then null\n           when ind_head.indkey[k-1] = 0 then chr(27) || pg_catalog.pg_get_indexdef(ind_head.indexrelid, k::int, true)\n           else pg_catalog.pg_get_indexdef(ind_head.indexrelid, k::int, true)\n       end as expression,\n       amcanorder can_order\nfrom pg_catalog.pg_index /* (select *, pg_catalog.generate_subscripts(indkey::int[], 1) + 1 k from pg_catalog.pg_index) */ ind_head\n         join pg_catalog.pg_class ind_stor\n              on ind_stor.oid = ind_head.indexrelid\ncross join unnest(ind_head.indkey) with ordinality u(u, k)\nleft join pg_catalog.pg_collation\non pg_collation.oid = ind_head.indcollation[k-1]\nleft join pg_catalog.pg_namespace colln on collnamespace = colln.oid\ncross join pg_catalog.pg_indexam_has_property(ind_stor.relam, 'can_order') amcanorder /* left join pg_catalog.pg_am am on ind_stor.relam = am.oid*/\n         left join pg_catalog.pg_opclass\n                   on pg_opclass.oid = ind_head.indclass[k-1]\n         left join pg_catalog.pg_namespace opcn on opcnamespace = opcn.oid\n  --  join T on ind_head.indrelid = T.oid\nwhere ind_stor.relnamespace = $1::oid\n  and ind_stor.relkind in ('i', 'I')\nand pg_catalog.age(ind_stor.xmin) <= coalesce(nullif(greatest(pg_catalog.age($2::varchar::xid), -1), -1), 2147483647)\norder by index_id, k"
exec_error params: Some([Some(b"\0\0\0\0\0\0\x08\x98"), Some(b"\0\0\0\0\0\0\0\0")])
exec_error error: NotImplemented("UNNEST with ordinality is not supported yet")

# Task 71: Gave up
Attempted to implement UNNEST WITH ORDINALITY support but DataFusion's planner lacks this feature and rewriting the query proved too complex.


# Task 81:
We have this 

    #[test]
    fn test_alias_subquery_tables() -> Result<(), Box<dyn std::error::Error>> {
        let sql = "SELECT (SELECT count(*) FROM pg_trigger WHERE tgrelid = rel.oid) FROM pg_class rel";
        let out = alias_subquery_tables(sql)?;
        assert!(out.contains("FROM pg_trigger AS subq0_t"));
        Ok(())
    }

this alias_subquery_tables function works. but we need more. we need unbounded columns to turn into bounded columns 

eg:
SELECT (SELECT id FROM pg_trigger WHERE tgrelid = rel.oid) FROM pg_class rel
turns into something like 

SELECT (SELECT id FROM pg_trigger subq0_t WHERE tgrelid = rel.oid) FROM pg_class rel

but we need to also use that alias in unbounded columns. Eg it should turn it into something like below

SELECT (SELECT count(*) FROM pg_trigger subq0_t WHERE subq0_t.tgrelid = rel.oid) FROM pg_class rel


so it gets bounded to a table. otherwise datafusion can't resolve the column and gives ambigitious column error.

So we need to find all unbounded columns in all subqueries and bound them. 

Please write another rewrite function to complement this.

also add a test case for this query

SELECT (SELECT subq0_t.id FROM pg_trigger subq0_t WHERE tgrelid = rel.oid) FROM pg_class rel

### Done

Implemented `bind_subquery_columns` to qualify unqualified column references in
subqueries with their table aliases. Added calls in the query rewrite pipeline
and created tests covering automatic and pre-existing aliases. Queries now bind
`tgrelid` to the generated alias to satisfy DataFusion.
