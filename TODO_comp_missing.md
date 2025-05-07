# set/show without prefixes
```
postgres=# SET application_name = 'PostgreSQL JDBC Driver';
SET
postgres=# SHOW application_name;
    application_name
------------------------
 PostgreSQL JDBC Driver
(1 row)
```

datafusion doesnt accept keys without prefixes.

# set returns SET
postgres:
```
postgres=# SET application_name = 'PostgreSQL JDBC Driver';
SET

pgtry=> set client.application_name = 'hello';
--
(0 rows)
```

# set should keep the value in pg_settings

```
postgres=# SELECT name, setting FROM pg_settings where name='application_name';
       name       | setting
------------------+---------
 application_name | foobar
(1 row)

postgres=# SET application_name = 'foobar';
SET
```

# add current_setting

SELECT current_setting('application_name');
## without error
SELECT current_setting('nonexistent_param', true); 


# SET application_name shows for connnections

postgres=# SELECT pid, application_name FROM pg_stat_activity;
  pid  | application_name
-------+------------------
  2751 |
  2755 |
 24582 | foobar
  2749 |
  2746 |
  2750 |
(6 rows)



# set local is not supported

```
pgtry=> set local client.application_name = 'hello2';
ERROR:  This feature is not implemented: LOCAL is not supported
```

# set timezone is not supported

```
pgtry=> SET TIME ZONE 'America/Los_Angeles';
ERROR:  This feature is not implemented: Unsupported SQL statement: SET TIME ZONE 'America/Los_Angeles'
```