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

The rewrite stopped after analysing the first outer table alias, so a scalar
subquery correlated with multiple outer tables left some predicates inside the
CTE. The analysis step now gathers correlations for **all** outer aliases. The
new Rust test `correlated_with_multiple_outer_tables` covers this case and a
Python functional test runs the provided query successfully.
