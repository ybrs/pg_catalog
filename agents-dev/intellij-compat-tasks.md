# Task 1 – implement current_user
Query: select current_database(), current_schema(), current_user

Params: []

Error: Invalid function 'current_user' … Did you mean 'current_time'?

Cause: DataFusion session lacks a current_user() scalar UDF.

Fix: Register a zero-arg UDF that returns the login name (mirrors how current_database() is handled in server.rs). Add both current_user and pg_catalog.current_user aliases.

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


# Task 3- function for pg_tablespace

Invalid function 'pg_catalog.pg_tablespace_location'

When registering the existing pg_tablespace_location UDF, call .with_aliases(["pg_catalog.pg_tablespace_location"]).

this can just return a dummy value.

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

# Task 8 – implement SHOW TRANSACTION ISOLATION LEVEL
Query: SHOW TRANSACTION ISOLATION LEVEL

Error: 'transaction.isolation.level' is not a variable which can be viewed with 'SHOW'

Fix: intercept SHOW TRANSACTION ISOLATION LEVEL in SimpleQueryHandler / ExtendedQueryHandler and return a single-row result with text read committed (Postgres default).