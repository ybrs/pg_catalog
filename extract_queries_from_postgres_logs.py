import re

import sqlparse
import yaml
import sqlglot
import hashlib


class LiteralString(str):
    pass

def literal_representer(dumper, data):
    return dumper.represent_scalar('tag:yaml.org,2002:str', data, style='|')

yaml.add_representer(LiteralString, literal_representer)


LOG_LINE_REGEX = re.compile(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3} ")

def parse_parameters(detail_line):
    params = {}
    raw_params = detail_line.split(",")
    for param in raw_params:
        if "=" not in param:
            continue
        key, value = param.strip().split("=", 1)
        key = key.strip()
        value = value.strip()
        if value.startswith("'") and value.endswith("'"):
            value = value[1:-1]
        elif value == "NULL":
            value = None
        params[key] = value
    return params

def normalize_sql(sql):
    try:
        return sqlglot.parse_one(sql, dialect="postgres").sql()
    except:
        print(sql)
        print("=====")
        raise

def sql_hash(sql):
    tree = sqlglot.parse_one(sql, dialect="postgres")
    return hashlib.md5(tree.sql().encode()).hexdigest()

def extract_queries_from_log(log_file):
    queries = []
    current_query = None
    expecting_detail = False

    seen_hashes = set()

    with open(log_file, "r") as f:
        for line in f:
            line = line.rstrip()

            if "statement:" in line:
                m = re.search(r"statement:\s*(.*)", line)
                if m:
                    if current_query:
                        queries.append(current_query)
                    current_query = {
                        "query": m.group(1).strip(),
                        "expected": ""
                    }
                    expecting_detail = False
                continue

            if "execute" in line and ":" in line:
                m = re.search(r"execute\s+<.*>:\s*(.*)", line)
                if m:
                    if current_query:
                        queries.append(current_query)
                    current_query = {
                        "query": m.group(1).strip(),
                        "expected": ""
                    }
                    expecting_detail = True
                continue

            if "DETAIL:" in line and expecting_detail:
                m = re.search(r"DETAIL:\s*parameters:\s*(.*)", line)
                if m and current_query:
                    parsed = parse_parameters(m.group(1))
                    if parsed:
                        current_query["parameters"] = parsed
                    expecting_detail = False
                continue

            if LOG_LINE_REGEX.match(line):
                continue

            if current_query and "query" in current_query:
                current_query["query"] += " " + line.strip()

    if current_query:
        queries.append(current_query)

    # Now normalize, deduplicate, and prettify
    final_queries = []
    for entry in queries:
        query_text = entry["query"]

        norm_query = normalize_sql(query_text)
        query_hash = sql_hash(query_text)

        if query_hash in seen_hashes:
            print("skipping query", query_hash)
            continue
        seen_hashes.add(query_hash)
        pretty_query = sqlparse.format(query_text, reindent=True, keyword_case="upper").strip()

        try:
            new_entry = {
                "query": query_text,
                "pretty_query": LiteralString(pretty_query),
                "query_hash": query_hash,
                "expected": ""
            }
        except:
            print(norm_query)
            import ipdb; ipdb.set_trace()
            raise
        if "parameters" in entry:
            new_entry["parameters"] = entry["parameters"]

        final_queries.append(new_entry)

    return final_queries

def save_queries_to_yaml(queries, output_file):
    with open(output_file, "w") as f:
        yaml.dump({"tests": queries}, f, sort_keys=False, allow_unicode=True)

if __name__ == "__main__":
    import sys
    
    input_log = "postgresql.log"
    output_yaml = "queries.yaml"
    input_log = sys.argv[1]
    output_yaml = sys.argv[2]
    queries = extract_queries_from_log(input_log)
    save_queries_to_yaml(queries, output_yaml)

    print(f"Extracted {len(queries)} unique, formatted queries into {output_yaml}")
