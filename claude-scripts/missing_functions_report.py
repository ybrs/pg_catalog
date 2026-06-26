"""Enumerate every function a catalog view needs that the engine does not provide.

The per-view audit (`audit_catalog_objects.py`) reports only the *first* function a
view fails on. This script finds the *complete* set: it extracts every function call
from every view's `view_sql`, probes a live server to see which names the engine
actually resolves, and for each missing one pulls the authoritative PostgreSQL
signature from the seed `pg_catalog.pg_proc` (argument types, return type, whether it
is set-returning, and - for set-returning functions - the output column names). That
signature is exactly the contract an integration callback must satisfy.

Run from the project ROOT against a running server:

    .venv/bin/python -m claude-scripts.missing_functions_report \
        --conn "host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable" \
        --out claude-scripts/missing_functions.md
"""

import argparse
import json
import re
from collections import defaultdict
from pathlib import Path

import psycopg

from yaml_loader import find_in_doc, load_yaml

SCHEMA_DIR = Path("pg_catalog_data/pg_schema")

# Identifiers that look like calls (`name(`) but are SQL syntax or engine built-ins,
# never integration-supplied. Probing would clear most of these anyway; skipping them
# keeps the probe set small and the report focused on catalog functions.
SQL_NOISE = {
    "select", "from", "where", "and", "or", "not", "in", "case", "when", "then",
    "else", "end", "cast", "as", "on", "using", "join", "left", "right", "inner",
    "outer", "full", "cross", "lateral", "union", "all", "distinct", "group", "by",
    "order", "having", "limit", "offset", "values", "with", "exists", "any", "some",
    "coalesce", "nullif", "count", "sum", "min", "max", "avg", "array", "array_agg",
    "unnest", "row", "over", "partition",
    # window/ordered-set built-ins the engine provides itself
    "rank", "dense_rank", "row_number", "percent_rank", "cume_dist", "ntile",
    "lag", "lead", "first_value", "last_value", "nth_value",
}

# A name(...) call. Captures an optional schema qualifier so `pg_catalog.foo(` and
# `foo(` collapse to the same function name.
CALL_RE = re.compile(r"(?:(\w+)\.)?(\w+)\s*\(")


def collect_view_sqls():
    """Yield ``(qualified_name, view_sql)`` for every view in the catalog."""
    for path in sorted(SCHEMA_DIR.glob("*.yaml")):
        doc = load_yaml(path)
        stack = [([], doc)]
        while stack:
            prefix, node = stack.pop()
            if not isinstance(node, dict):
                continue
            if node.get("type") == "view" and node.get("view_sql"):
                schema = prefix[-2] if len(prefix) >= 2 else "?"
                name = prefix[-1] if prefix else "?"
                yield f"{schema}.{name}", node["view_sql"]
                continue
            for key, value in node.items():
                stack.append((prefix + [key], value))


def called_functions(sql: str):
    """The distinct lower-cased function names called in `sql`, minus SQL noise."""
    names = set()
    for _schema, name in CALL_RE.findall(sql):
        lowered = name.lower()
        if lowered not in SQL_NOISE:
            names.add(lowered)
    return names


def is_resolvable(conn, name: str) -> bool:
    """Whether the engine resolves `name` as either a scalar or a table function.

    Probes both call shapes. A name the engine does not know fails with "Invalid
    function" / "function ... does not exist" (scalar) or "table function ... not
    found" (table). Any other outcome - success, or an argument/type error - means
    the function IS registered, just called differently here.
    """
    for probe in (f"SELECT {name}()", f"SELECT * FROM {name}()"):
        try:
            with conn.cursor() as cur:
                cur.execute(probe)
            return True  # planned with no args -> definitely registered
        except Exception as exc:  # noqa: BLE001
            msg = str(exc).lower()
            unknown = (
                f"invalid function '{name}'" in msg
                or (f"function {name}" in msg and "does not exist" in msg)
                or (f"table function '{name}'" in msg and "not found" in msg)
            )
            if not unknown:
                return True  # a different error -> the function exists
    return False


def pg_proc_names():
    """The set of all function names in the seed `pg_proc`.

    Candidate names from a view body are gated on this so that table aliases with a
    column list (``FROM f(...) s(a, b)`` looks like a call to ``s``) and other
    non-functions are not mistaken for missing functions.
    """
    rows = find_in_doc(load_yaml(SCHEMA_DIR / "pg_catalog__pg_proc.yaml"), "rows") or []
    return {r["proname"] for r in rows if r.get("proname")}


def type_names():
    """Map type OID -> type name from the seed `pg_type`."""
    rows = find_in_doc(load_yaml(SCHEMA_DIR / "pg_catalog__pg_type.yaml"), "rows") or []
    return {int(r["oid"]): r["typname"] for r in rows if r.get("oid") is not None}


def pg_proc_signatures(names):
    """Map each function name to its seed `pg_proc` signature.

    Returns ``name -> dict`` with ``set_returning`` (bool), ``returns`` (type name),
    ``args`` (list of ``(name, type)``), and ``out_columns`` (list of ``(name, type)``
    for set-returning functions). Read from the seed YAML so the signature matches
    real PostgreSQL exactly, without the wire-encoding quirks of the array columns.
    """
    if not names:
        return {}
    typ = type_names()
    want = set(names)
    rows = find_in_doc(load_yaml(SCHEMA_DIR / "pg_catalog__pg_proc.yaml"), "rows") or []
    sigs = {}
    for r in rows:
        name = r.get("proname")
        if name not in want or name in sigs:
            continue
        argtypes = [int(o) for o in str(r.get("proargtypes") or "").split()]
        allargtypes = r.get("proallargtypes") or []
        argmodes = r.get("proargmodes") or []
        argnames = r.get("proargnames") or []
        in_args, out_cols = [], []
        if argmodes:
            # OUT parameters present: proallargtypes / proargmodes / proargnames
            # describe every parameter, so split them into inputs and result columns.
            for i, mode in enumerate(argmodes):
                tname = typ.get(int(allargtypes[i]), str(allargtypes[i])) if i < len(allargtypes) else "?"
                pname = argnames[i] if i < len(argnames) else f"${i + 1}"
                if mode in ("o", "t", "b"):
                    out_cols.append((pname, tname))
                if mode in ("i", "b", "v"):
                    in_args.append((pname, tname))
        else:
            in_args = [(f"${i + 1}", typ.get(o, str(o))) for i, o in enumerate(argtypes)]
        sigs[name] = {
            "set_returning": bool(r.get("proretset")),
            "returns": typ.get(int(r["prorettype"]), str(r.get("prorettype"))),
            "args": in_args,
            "out_columns": out_cols,
        }
    return sigs


def render(missing, sig_by_name, used_by):
    """Render the missing-function report as Markdown."""
    lines = []
    w = lines.append
    set_fns = sorted(n for n in missing if sig_by_name.get(n, {}).get("set_returning"))
    scalar_fns = sorted(n for n in missing if not sig_by_name.get(n, {}).get("set_returning"))

    w("# Missing catalog functions\n")
    w(f"{len(missing)} functions referenced by views are not provided by the engine: "
      f"{len(set_fns)} set-returning (table) and {len(scalar_fns)} scalar. Signatures "
      "are from the seed `pg_catalog.pg_proc` (i.e. real PostgreSQL).\n")

    def emit(title, fns):
        w(f"## {title} ({len(fns)})\n")
        for n in fns:
            sig = sig_by_name.get(n)
            views = ", ".join(sorted(used_by[n]))
            if not sig:
                w(f"### `{n}`\n- signature: (not in seed pg_proc)\n- used by: {views}\n")
                continue
            args = ", ".join(f"{an} {at}" for an, at in sig["args"]) or "(no args)"
            w(f"### `{n}({args})`")
            if sig["set_returning"]:
                cols = ", ".join(f"{cn} {ct}" for cn, ct in sig["out_columns"]) or "(unknown)"
                w(f"- returns: SETOF rows of ({cols})")
            else:
                w(f"- returns: {sig['returns']}")
            w(f"- used by: {views}\n")

    if set_fns:
        emit("Set-returning (table) functions", set_fns)
    if scalar_fns:
        emit("Scalar functions", scalar_fns)
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--conn",
        default="host=127.0.0.1 port=5444 dbname=pgtry user=dbuser password=pencil sslmode=disable",
    )
    parser.add_argument("--out", type=Path, default=Path("claude-scripts/missing_functions.md"))
    parser.add_argument("--json-out", type=Path,
                        default=Path("claude-scripts/missing_functions.json"))
    args = parser.parse_args()

    used_by = defaultdict(set)
    for qn, sql in collect_view_sqls():
        for fn in called_functions(sql):
            used_by[fn].add(qn)

    # Only real PostgreSQL functions (those in pg_proc) are candidates; this drops
    # table/subquery aliases that lexically look like calls.
    real = pg_proc_names()
    candidates = sorted(fn for fn in used_by if fn in real)

    conn = psycopg.connect(args.conn, autocommit=True)
    missing = sorted(fn for fn in candidates if not is_resolvable(conn, fn))
    sig_by_name = pg_proc_signatures(missing)

    args.out.write_text(render(missing, sig_by_name, used_by), encoding="utf-8")
    args.json_out.write_text(
        json.dumps(
            {
                fn: {
                    **sig_by_name.get(fn, {}),
                    "used_by": sorted(used_by[fn]),
                }
                for fn in missing
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    n_set = sum(1 for f in missing if sig_by_name.get(f, {}).get("set_returning"))
    print(f"{len(missing)} missing functions ({n_set} set-returning, "
          f"{len(missing) - n_set} scalar). Wrote {args.out} and {args.json_out}.")


if __name__ == "__main__":
    main()
