Catalog, schema, tables vs. Database.
===

```

PostgreSQL                                      DataFusion
┌────────────────────────────────┐              ┌────────────────────────────────┐
│ Server                         │              │ Application (DataFusion ctx)   │
└────────────────────────────────┘              └────────────────────────────────┘
             │                                                  │
             ▼                                                  ▼
┌────────────────────────────────┐              ┌────────────────────────────────┐
│ Database (e.g., mydb)          │◄─────┐       │ Catalog (e.g., default)        │
└────────────────────────────────┘      │       └────────────────────────────────┘
             │                          │                       │
             ▼                          │                       ▼
┌────────────────────────────────┐      │      ┌────────────────────────────────┐
│ Schema (e.g., public, crm)     │      │      │ Schema (e.g., public, crm)     │
└────────────────────────────────┘      │      └────────────────────────────────┘
             │                          │                       │
             ▼                          │                       ▼
┌────────────────────────────────┐      │      ┌────────────────────────────────┐
│ Table (e.g., users, orders)    │      │      │ Table (e.g., users, orders)    │
└────────────────────────────────┘      │      └────────────────────────────────┘
                                        │
                                        │
┌────────────────────────────────┐      │
│ Special Schema: pg_catalog     │◄─────┘
└────────────────────────────────┘
             │
             ▼
┌────────────────────────────────┐
│ System Tables, e.g., pg_class  │
└────────────────────────────────┘

```