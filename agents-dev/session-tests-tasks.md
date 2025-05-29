# Task 101

We run this server with 

```
cargo run ./pg_catalog_data/pg_schema --default-catalog pgtry --default-schema pg_catalog --port 5444
```

What we need is to have another parameter `--capture filename`  
- Save all queries, parameters for those queries and the output of these queries to given filename
- It will be a list of queries with "query", "parameters", "result" fields, result will be a list of dictionaries.
- The format of the file will be a yaml file.
- If the query has errors we need to capture that also. Put a "success: false" field for failed queries, for successful queries it will be "success:true". Add an  "error_details" field to errored queries.

### Approach

Implemented the `--capture` CLI option. When provided, every query is written to
the given YAML file with its parameters, results and success flag. Errors are
recorded with details. Tests cover capturing of successful queries, parameterized
queries and failing queries.

# Task 102

You'll see yaml files on captures/*.yaml

What I need you to do is
- write a python test name it "tests/test_captures.py"
- it should first spawn a new server - check the other test file.
- then open the first yaml file, go through each query. 
- if the query is marked with success: false skip that query. 
- if the query is marked with success, send the query to the server, and check the response. 
- Check if the query response matches the data. if the data doesnt match for the query, you can fail the test. 


### Attempt

Tried to implement a replay test for the capture YAMLs but the results produced by the current server did not match the stored outputs (e.g. ACL fields differed). Unable to resolve the discrepancies so giving up on this task.
