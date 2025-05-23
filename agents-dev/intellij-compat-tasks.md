# Task 1 – implement current_user
Query: select current_database(), current_schema(), current_user

Params: []

Error: Invalid function 'current_user' … Did you mean 'current_time'?

Cause: DataFusion session lacks a current_user() scalar UDF.

Fix: Register a zero-arg UDF that returns the login name (mirrors how current_database() is handled in server.rs). Add both current_user and pg_catalog.current_user aliases.

Done: Added register_current_user in server.rs and called it for every query. The UDF uses client metadata to return the login name and registers pg_catalog.current_user alias. Added integration test verifying SELECT current_database(), current_schema(), current_user returns expected values.

# Task 2 – add virtual columns 
Query (excerpt):

```
select T.oid::bigint as id, T.spcname … T.xmin … pg_catalog.pg_tablespace_location(T.oid)
from pg_catalog.pg_tablespace T
Error:

column 'xmin' not found in 't'

```

Cause:

xmin is a system column not present in the static YAML schema.

UDF pg_catalog.pg_tablespace_location currently registered without schema alias; caller uses schema-qualified name.


you need to implement these columns

System column	Type	What it really is
ctid	tid	Physical location of the row on disk (block #, offset).
xmin	xid	X-transaction min – the transaction ID that inserted this row/version.
xmax	xid	Transaction ID that deleted (or is deleting) the row version. 0 means “still alive.”
cmin/cmax	cid	Command IDs inside the inserting/deleting transaction (rarely queried directly).
tableoid	oid	OID of the table this row belongs to (handy with inherited partitions or SET search_path).

Fix:

Extend parse_schema_* to include virtual system columns (xmin, etc.) with dummy values. So each table gets these columns/values. Unless these columns are defined in the schema yaml files.

Done: build_table now injects ctid, xmin, xmax, cmin, cmax and tableoid with placeholder values. Tests verify the columns exist and remain hidden from wildcard selects.


# Task 3- function for pg_tablespace

Invalid function 'pg_catalog.pg_tablespace_location'

When registering the existing pg_tablespace_location UDF, call .with_aliases(["pg_catalog.pg_tablespace_location"]).

this can just return a dummy value.

Done: the registration now includes the schema-qualified alias and a test exercises
`SELECT pg_catalog.pg_tablespace_location('pg_default')`, expecting a NULL result.

# Task 4 – reuse Task 1 for current_user inside pg_user query

requires task 1

Query: select usesuper from pg_user where usename = current_user

Params: []

Error: there is no pg_user table

Solution: add a pg_user table dynamically - there is no schema to this. so you need to create the schema on the fly on startup

postgres=# select * from pg_catalog.pg_user;
 usename | usesysid | usecreatedb | usesuper | userepl | usebypassrls |  passwd  | valuntil | useconfig
---------+----------+-------------+----------+---------+--------------+----------+----------+-----------
 abadur  |       10 | t           | t        | t       | t            | ******** |          |
 test    |    16385 | f           | f        | f       | f            | ******** |          |
 dbuser  |    27735 | t           | t        | f       | f            | ******** |          |
(3 rows)

postgres=# \d pg_user
                        View "pg_catalog.pg_user"
    Column    |           Type           | Collation | Nullable | Default
--------------+--------------------------+-----------+----------+---------
 usename      | name                     |           |          |
 usesysid     | oid                      |           |          |
 usecreatedb  | boolean                  |           |          |
 usesuper     | boolean                  |           |          |
 userepl      | boolean                  |           |          |
 usebypassrls | boolean                  |           |          |
 passwd       | text                     |           |          |
 valuntil     | timestamp with time zone |           |          |
 useconfig    | text[]                   | C         |          |


# Task 5 – recognise oid as a scalar type in parameters
Representative query: … where cls.relnamespace = $1::oid

Params: binary OID value e.g. \x00\x00\x00\x08\x98

Error: NotImplemented("Unsupported SQL type Custom … oid")

Cause: The type-inference layer treats oid as a custom type; DataFusion doesn’t map it to a built-in type.

Fix:

Extend replace_regtype_cast or add a new rewrite to normalise ::oid casts to ::int4 (Postgres treats OID as uint32 internally).

Update execute_sql_inner param decoding to accept Type::OID (code already handles INT2/4/8).

# Task 6 – add array_agg shim
Query (excerpt): select … array_agg(inhparent::bigint order by inhseqno)::varchar

Error: Invalid function 'pg_catalog.array_agg'

Cause: missing aggregate registration.

Fix: create an aggregate UDF array_agg / pg_catalog.array_agg that reuses the existing ArrayCollector accumulator used for pg_get_array.

DONE: Added register_array_agg using DataFusion's array_agg_udaf and alias pg_catalog.array_agg. Registered the UDF in the server and tests confirm it works.

# Task 7 – add missing catalog columns (conexclop, etc.)
Query: constraint metadata union

Error: FieldNotFound { field: 'conexclop' }

Cause: Column absent in static schema.

Fix: pg_constraint table has with conexclop oid[] (nullable). but apparently it fails in unnest 
```
pgtry=> select conexclop from pg_catalog.pg_constraint limit 1;
 conexclop
-----------

(1 row)
```

but this fails

```
pgtry=> select array(select unnest from unnest(C.conexclop)) from pg_catalog.pg_constraint C;
ERROR:  Schema error: No field named c.conexclop.
```

this query is rewritten to 

final sql "WITH __cte1 AS (SELECT unnest AS col FROM UNNEST(C.conexclop)) SELECT pg_catalog.pg_get_array(__cte1.col) AS alias_1 FROM pg_catalog.pg_constraint AS C LEFT JOIN __cte1 ON true"

find what the issue might be first. add comments to this "agents-dev/intellij-compat-tasks.md" on this task about the approach to take and how you can fix it. 

fix it. 

add tests for this use case.

Approach: the column existed but the ARRAY subquery rewrite produced a
correlated CTE referencing `C.conexclop`. DataFusion couldn't resolve this and
raised a `FieldNotFound` error. We detect the specific pattern
`ARRAY(SELECT unnest FROM UNNEST(col))` and replace it with the original column
expression before further rewrites. The added functional test ensures the query
now returns `NULL` instead of failing.

# Task 8 – implement SHOW TRANSACTION ISOLATION LEVEL
Query: SHOW TRANSACTION ISOLATION LEVEL

Error: 'transaction.isolation.level' is not a variable which can be viewed with 'SHOW'

Fix: intercept SHOW TRANSACTION ISOLATION LEVEL in SimpleQueryHandler / ExtendedQueryHandler and return a single-row result with text read committed (Postgres default).

Implemented: the server now handles this statement directly and returns a single
`transaction_isolation` column with the value `read committed`. Describe handlers
also provide matching metadata so prepared queries work. Tests were added.

# Task 9 - regoper::text

Now this works

```
pgtry=> select array(select unnest from unnest(conexclop) ) from pg_catalog.pg_constraint;
```

but this doesnt work giving wrong error
```
pgtry=> select array(select unnest::regoper::varchar from unnest(conexclop) ) from pg_catalog.pg_constraint;
ERROR:  Schema error: No field named conexclop.
```

possible cause is this. 

```
pgtry=> select conexclop::regoper::text from pg_catalog.pg_constraint;
ERROR:  This feature is not implemented: Unsupported SQL type Custom(ObjectName([Identifier(Ident { value: "regoper", quote_style: None, span: Span(Location(1,19)..Location(1,26)) })]), [])
```

since we keep conexclop as _text and it's always null, we can shortcut this ::regoper::{whatevertype} for now. just return null.

# Task 10 - ::oid type

these queries crash

```
select A.oid as access_method_id,\n       A.xmin as state_number,\n       A.amname as access_method_name\n       ,\n       A.amhandler::oid as handler_id,\n       pg_catalog.quote_ident(N.nspname) || '.' || pg_catalog.quote_ident(P.proname) as handler_name,\n       A.amtype as access_method_type\n       \nfrom pg_am A\n  join pg_proc P on A.amhandler::oid = P.oid\n  join pg_namespace N on P.pronamespace = N.oid\n  \n--  where pg_catalog.age(A.xmin) <= #TXAGE
```

because 

```
pgtry=> select amhandler::oid::text from pg_catalog.pg_am;
ERROR:  This feature is not implemented: Unsupported SQL type Custom(ObjectName([Identifier(Ident { value: "oid", quote_style: None, span: Span(Location(1,19)..Location(1,22)) })]), [])
```

We have register_scalar_regclass_oid which implements oid() udf.

So you need to add ::oid type using oid() function. or rewrite column::oid as oid(column) (also handle scalar values $1::oid should work too)

these queries work

```
pgtry=> SELECT 'pg_constraint'::regclass::oid;
 oid
------
 2606
(1 row)
```

Done: Added rewrite_oid_cast that converts any `::oid` cast. String columns are
rewritten to `oid(col)` while placeholders and numeric literals become plain
INT8 casts. Tests verify `amhandler::oid` and parameter casts succeed.

# Task 11 - this query should return 

This requires task number 9 and 10. dont start if those tasks are not completed 

This query should return without crashing. 

```
exec_error query: "select T.oid table_id,
       relkind table_kind,
       C.oid::bigint con_id,
       C.xmin::varchar::bigint con_state_id,
       conname con_name,
       contype con_kind,
       conkey con_columns,
       conindid index_id,
       confrelid ref_table_id,
       condeferrable is_deferrable,
       condeferred is_init_deferred,
       confupdtype on_update,
       confdeltype on_delete,
      connoinherit no_inherit,
      pg_catalog.pg_get_expr(conbin, T.oid) /* consrc */ con_expression,
       confkey ref_columns,
       conexclop::int[] excl_operators,
       array(select unnest::regoper::varchar from unnest(conexclop)) excl_operators_str
from pg_catalog.pg_constraint C
         join pg_catalog.pg_class T
              on C.conrelid = T.oid
   where relkind in ('r', 'v', 'f', 'p')
     and relnamespace = $1::oid
     and contype in ('p', 'u', 'f', 'c', 'x')
     and connamespace = $2::oid 
```



