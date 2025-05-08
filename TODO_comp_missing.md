# ~~ set/show without prefixes ~~
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

Fixed: 

```
pgtry=> set application_name = 'hello4';
--
(0 rows)

pgtry=> show application_name;
       name       | value
------------------+--------
 application_name | hello4
(1 row)
```


# set returns SET
postgres:
```
postgres=# SET application_name = 'PostgreSQL JDBC Driver';
SET

pgtry=> set client.application_name = 'hello';
--
(0 rows)
```

# ~~SET is per session in postgresql~~

```
postgres=# SET application_name = 'foobar';
SET

postgres=# SELECT name, setting FROM pg_settings where name='application_name';
       name       | setting
------------------+---------
 application_name | foobar
(1 row)
```

this is per session. in another terminal.

```
postgres=# SELECT name, setting FROM pg_settings where name='application_name';
       name       | setting
------------------+---------
 application_name | psql
(1 row)
```

In pgcatalogrs

```
pgtry=> set application_name = 'hello4';
--
(0 rows)

pgtry=> show application_name;
       name       | value
------------------+--------
 application_name | hello4
(1 row)
```

in another session

```
pgtry=> show application_name;
       name       | value
------------------+--------
 application_name | hello4
(1 row)
```

Fixed: shows different values per session.


# set should keep the value in pg_settings

```
postgres=# SET application_name = 'foobar';
SET

postgres=# SELECT name, setting FROM pg_settings where name='application_name';
       name       | setting
------------------+---------
 application_name | foobar
(1 row)
```

this is per session. in another terminal.

```
postgres=# SELECT name, setting FROM pg_settings where name='application_name';
       name       | setting
------------------+---------
 application_name | psql
(1 row)
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

# ~~current_schema(); current_database();~~

```
postgres=# select current_schema();
 current_schema
----------------
 public
(1 row)

postgres=# select current_database();
 current_database
------------------
 postgres
(1 row)
```

TODO: current_schema only returns "public". We can't have multiple schemas (catalog in datafusion)


