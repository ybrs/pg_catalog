Catalog, schema, tables vs. Database.
===

```

PostgreSQL                                      DataFusion
+--------------------------------+              +--------------------------------+
| Server                         |              | Application (DataFusion ctx)   |
+--------------------------------+              +--------------------------------+
             |                                                  |
             v                                                  v
+--------------------------------+              +--------------------------------+
| Database (e.g., mydb)          |<------+       | Catalog (e.g., default)        |
+--------------------------------+      |       +--------------------------------+
             |                          |                       |
             v                          |                       v
+--------------------------------+      |      +--------------------------------+
| Schema (e.g., public, crm)     |      |      | Schema (e.g., public, crm)     |
+--------------------------------+      |      +--------------------------------+
             |                          |                       |
             v                          |                       v
+--------------------------------+      |      +--------------------------------+
| Table (e.g., users, orders)    |      |      | Table (e.g., users, orders)    |
+--------------------------------+      |      +--------------------------------+
                                        |
                                        |
+--------------------------------+      |
| Special Schema: pg_catalog     |<------+
+--------------------------------+
             |
             v
+--------------------------------+
| System Tables, e.g., pg_class  |
+--------------------------------+

```
## SHOW command fix
- Added proper SQL parsing for SHOW commands using sqlparser to handle trailing semicolons.
- SHOW now returns a single column with the variable value, matching PostgreSQL output.
