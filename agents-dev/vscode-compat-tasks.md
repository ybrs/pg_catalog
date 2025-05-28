# Task 61:
exec_error query: "SELECT * FROM unnest(current_schemas(true))"
exec_error error: Diagnostic(Diagnostic { kind: Error, message: "Invalid function 'current_schemas'", span: Some(Span(Location(1,22)..Location(1,37))), notes: [DiagnosticNote { message: "Possible function 'current_schema'", span: None }], helps: [] }, Plan("Invalid function 'current_schemas'.\nDid you mean 'current_schema'?"))
exec_error params: None

We need to return similar to postgresql. We can return pg_catalog and public in an array.

postgres=# select * from current_schemas(true);
   current_schemas
---------------------
 {pg_catalog,public}
(1 row)

# Task 62:
exec_error query: "DISCARD ALL"
exec_error params: None
exec_error error: NotImplemented("Unsupported SQL statement: DISCARD ALL")

just put a stub as discard all

# Task 63:

exec_error query: "SELECT\n    db.oid as oid, \n    db.datname as name, \n    ta.spcname as spcname, \n    db.datallowconn,\n    db.datlastsysoid,\n    has_database_privilege(db.oid, 'CREATE') as cancreate, \n    datdba as owner, \n    db.datistemplate , \n    has_database_privilege(db.datname, 'connect') as canconnect,\n    datistemplate as is_system\n\nFROM\n    pg_database db\n    LEFT OUTER JOIN pg_tablespace ta ON db.dattablespace = ta.oid\n\nORDER BY datname;"
exec_error params: None
exec_error error: Collection([Diagnostic(Diagnostic { kind: Error, message: "column 'datlastsysoid' not found in 'db'", span: None, notes: [], helps: [] }, SchemaError(FieldNotFound { field: Column { relation: Some(Bare { table: "db" }), name: "datlastsysoid" }, valid_fields: [Column { relation: Some(Bare { table: "db" }), name: "datacl" }, Column { relation: Some(Bare { table: "db" }), name: "datallowconn" }, Column { relation: Some(Bare { table: "db" }), name: "datcollate" }, Column { relation: Some(Bare { table: "db" }), name: "datcollversion" }, Column { relation: Some(Bare { table: "db" }), name: "datconnlimit" }, Column { relation: Some(Bare { table: "db" }), name: "datctype" }, Column { relation: Some(Bare { table: "db" }), name: "datdba" }, Column { relation: Some(Bare { table: "db" }), name: "datfrozenxid" }, Column { relation: Some(Bare { table: "db" }), name: "dathasloginevt" }, Column { relation: Some(Bare { table: "db" }), name: "daticurules" }, Column { relation: Some(Bare { table: "db" }), name: "datistemplate" }, Column { relation: Some(Bare { table: "db" }), name: "datlocale" }, Column { relation: Some(Bare { table: "db" }), name: "datlocprovider" }, Column { relation: Some(Bare { table: "db" }), name: "datminmxid" }, Column { relation: Some(Bare { table: "db" }), name: "datname" }, Column { relation: Some(Bare { table: "db" }), name: "dattablespace" }, Column { relation: Some(Bare { table: "db" }), name: "encoding" }, Column { relation: Some(Bare { table: "db" }), name: "oid" }, Column { relation: Some(Bare { table: "db" }), name: "xmin" }, Column { relation: Some(Bare { table: "db" }), name: "xmax" }, Column { relation: Some(Bare { table: "db" }), name: "ctid" }, Column { relation: Some(Bare { table: "db" }), name: "tableoid" }, Column { relation: Some(Bare { table: "db" }), name: "cmin" }, Column { relation: Some(Bare { table: "db" }), name: "cmax" }, Column { relation: Some(Bare { table: "ta" }), name: "oid" }, Column { relation: Some(Bare { table: "ta" }), name: "spcacl" }, Column { relation: Some(Bare { table: "ta" }), name: "spcname" }, Column { relation: Some(Bare { table: "ta" }), name: "spcoptions" }, Column { relation: Some(Bare { table: "ta" }), name: "spcowner" }, Column { relation: Some(Bare { table: "ta" }), name: "xmin" }, Column { relation: Some(Bare { table: "ta" }), name: "xmax" }, Column { relation: Some(Bare { table: "ta" }), name: "ctid" }, Column { relation: Some(Bare { table: "ta" }), name: "tableoid" }, Column { relation: Some(Bare { table: "ta" }), name: "cmin" }, Column { relation: Some(Bare { table: "ta" }), name: "cmax" }] }, Some(""))), Diagnostic(Diagnostic { kind: Error, message: "Invalid function 'has_database_privilege'", span: Some(Span(Location(1,123)..Location(1,145))), notes: [DiagnosticNote { message: "Possible function 'generate_series'", span: None }], helps: [] }, Plan("Invalid function 'has_database_privilege'.\nDid you mean 'generate_series'?")), Diagnostic(Diagnostic { kind: Error, message: "Invalid function 'has_database_privilege'", span: Some(Span(Location(1,224)..Location(1,246))), notes: [DiagnosticNote { message: "Possible function 'generate_series'", span: None }], helps: [] }, Plan("Invalid function 'has_database_privilege'.\nDid you mean 'generate_series'?"))])


## # Task 63: Done
Added missing `datlastsysoid` column and `has_database_privilege` stub. Tests pass.
