# Task 82:

Because datafusion doesnt support outer table refs in CTE's.

Which rewrites these 

```
SELECT (SELECT 1 FROM t2 WHERE t2.id = t1.id AND t2.flag = 'Y') FROM t1
```

into 

```
WITH __cte1 AS (SELECT 1 AS col, t2.id FROM t2 AS subq0_t WHERE t2.flag = 'Y' GROUP BY t2.id) SELECT __cte1.col AS alias_1 FROM t1 LEFT OUTER JOIN __cte1 ON t1.id = __cte1.id
```

See t1.id = __cte1.id is moved outside.

The issue is 

We get this query
```
SELECT
    (SELECT pg_attrdef.adbin FROM pg_attrdef WHERE adrelid = cls.oid AND adnum = attr.attnum) AS default
FROM pg_attribute AS attr
JOIN pg_type AS typ ON attr.atttypid = typ.oid
JOIN pg_class AS cls ON cls.oid = attr.attrelid
JOIN pg_namespace AS ns ON ns.oid = cls.relnamespace;
```

We rewrite this query with rewrite_subquery_as_cte 

```
WITH __cte1 AS (SELECT pg_attrdef.adbin AS col, adnum FROM pg_attrdef AS subq0_t WHERE adrelid = cls.oid) SELECT __cte1.col AS default FROM pg_attribute AS attr JOIN pg_type AS typ ON attr.atttypid = typ.oid JOIN pg_class AS cls ON cls.oid = attr.attrelid JOIN pg_namespace AS ns ON ns.oid = cls.relnamespace LEFT OUTER JOIN __cte1 ON attr.attnum = __cte1.adnum AND adnum = attr.attnum
```

for some reason adrelid = cls.oid stays inside.

First I want you to find the reason for this. It can be that if we have multiple joins outside it's not working correctly. or any other reason. Then come up with a solution.

Add unit tests on rust side

If you can find the issue include this exact query in python functional tests. 

### Done

The scalar to CTE rewriter stopped when it found the first outer table alias.
As a result only one correlated predicate was lifted and the remaining one
stayed inside the subquery. The analysis step now collects predicates for all
outer aliases and passes them to the join builder so every correlation is
exposed. A new Rust unit test covers the rewrite and a functional Python test
executes the failing query to ensure it succeeds.


# Task 83:

We get this query
```
SELECT  rel.oid,
        (SELECT count(*) FROM pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE) AS triggercount,
        (SELECT count(*) FROM pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE AND tgenabled = 'O') AS has_enable_triggers,
        (CASE WHEN rel.relkind = 'p' THEN true ELSE false END) AS is_partitioned,
        nsp.nspname AS schema,
        nsp.oid AS schemaoid,
        rel.relname AS name,
        CASE
    WHEN nsp.nspname like 'pg_%' or nsp.nspname = 'information_schema'
        THEN true
    ELSE false END as is_system
FROM    pg_class rel
INNER JOIN pg_namespace nsp ON rel.relnamespace= nsp.oid
    WHERE rel.relkind IN ('r','t','f','p')
        AND NOT rel.relispartition
    ORDER BY nsp.nspname, rel.relname;
```

this is rewritten as 

```
WITH 
    __cte1 AS (SELECT count(*) AS col, tgrelid FROM pg_trigger AS subq0_t WHERE tgisinternal = false GROUP BY tgrelid), 
    __cte2 AS (SELECT count(*) AS col, tgrelid FROM pg_trigger AS subq1_t WHERE tgisinternal = false AND tgenabled = 'O' GROUP BY tgrelid) 
    SELECT 
        rel.oid AS alias_1, 
        __cte1.col AS triggercount, 
        __cte2.col AS has_enable_triggers, 
        (CASE WHEN rel.relkind = 'p' THEN true ELSE false END) AS is_partitioned, 
        nsp.nspname AS schema, 
        nsp.oid AS schemaoid, 
        rel.relname AS name, 
        CASE WHEN nsp.nspname LIKE 'pg_%' OR nsp.nspname = 'information_schema' THEN true ELSE false END AS is_system 
        FROM pg_class AS rel 
        INNER JOIN pg_namespace AS nsp ON rel.relnamespace = nsp.oid 
        LEFT OUTER JOIN __cte1 ON rel.oid = __cte1.tgrelid AND tgrelid = rel.oid 
        LEFT OUTER JOIN __cte2 ON rel.oid = __cte2.tgrelid AND tgrelid = rel.oid 
        WHERE rel.relkind IN ('r', 't', 'f', 'p') 
            AND NOT rel.relispartition 
        ORDER BY nsp.nspname, rel.relname
```

Which gives ambigious column for tgrelid


When we move tgrelid from subquery it puts it into the outer join condition

Though it somehow puts 

```
LEFT OUTER JOIN __cte1 ON rel.oid = __cte1.tgrelid AND tgrelid = rel.oid 
```

rel.oid = __cte1.tgrelid is correct. the second AND tgrelid = rel.oid is duplicate and doesnt have the __cte1 alias.

You can either remove duplicate column, or just add correct alias as 

```
LEFT OUTER JOIN __cte1 ON rel.oid = __cte1.tgrelid AND __cte1.tgrelid = rel.oid 
```

Add this query to python functional tests. also add this condition to rust unit tests.

Note that this works !

    #[test]
    fn multiple_correlated_aliases() -> Result<()> {
        let sql = "SELECT (SELECT pg_attrdef.adbin FROM pg_attrdef WHERE adrelid = cls.oid AND adnum = attr.attnum) AS default \nFROM pg_attribute AS attr\nJOIN pg_type AS typ ON attr.atttypid = typ.oid\nJOIN pg_class AS cls ON cls.oid = attr.attrelid\nJOIN pg_namespace AS ns ON ns.oid = cls.relnamespace";

        let out = rewrite(sql)?;
        let s = out.sql.clone();
        println!("rewritten: {}", s);

        assert!(s.contains("cls.oid = __cte1.adrelid"), "cls predicate not in JOIN");
        assert!(s.contains("attr.attnum = __cte1.adnum"), "attr predicate not in JOIN");
        assert!(!s.contains("WHERE adrelid = cls.oid"), "predicate left in CTE");

        Ok(())
    }

and also this query works 
```
def test_rewrite_multiple_correlated_aliases(server):
    sql = (
        "SELECT (SELECT adbin FROM pg_catalog.pg_attrdef WHERE adrelid = cls.oid "
        "AND adnum = attr.attnum) AS default "
        "FROM pg_catalog.pg_attribute AS attr "
        "JOIN pg_catalog.pg_type AS typ ON attr.atttypid = typ.oid "
        "JOIN pg_catalog.pg_class AS cls ON cls.oid = attr.attrelid "
        "JOIN pg_catalog.pg_namespace AS ns ON ns.oid = cls.relnamespace"
    )
    with psycopg.connect(CONN_STR) as conn:
        cur = conn.cursor()
        cur.execute(sql)
        cur.fetchone()
```

so it probably adds when there are multiple subqueries

### Done

`collect_paths` ignored bare identifiers so `tgrelid = rel.oid` was mistaken for
an outer-only predicate and copied twice into the JOIN clause. The function now
records identifier nodes, preventing the duplicate condition. A new Rust unit
test checks the rewritten JOIN and a functional Python test runs the original
query successfully.
