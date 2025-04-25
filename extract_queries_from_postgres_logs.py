import re
import yaml

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

def extract_queries_from_log(log_file):
    queries = []
    current_query = None
    expecting_detail = False

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
                    current_query["parameters"] = parsed
                    expecting_detail = False
                continue

            if LOG_LINE_REGEX.match(line):
                # New log line, maybe flush multi-line query
                continue

            # multi-line query continuation
            if current_query and "query" in current_query:
                current_query["query"] += " " + line.strip()

    # flush last one
    if current_query:
        queries.append(current_query)

    return queries

def save_queries_to_yaml(queries, output_file):
    with open(output_file, "w") as f:
        yaml.dump({"tests": queries}, f, sort_keys=False, allow_unicode=True)

if __name__ == "__main__":
    input_log = "postgresql.log"
    output_yaml = "queries.yaml"

    queries = extract_queries_from_log(input_log)
    save_queries_to_yaml(queries, output_yaml)

    print(f"Extracted {len(queries)} queries into {output_yaml}")
