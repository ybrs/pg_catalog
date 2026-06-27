"""Generate the user-facing runtime-function resolver reference from the source.

The runtime functions an integration can supply (see ``src/runtime_function_resolvers.rs``)
are declared in two macro tables - ``scalar_resolvers!`` and ``table_resolvers!`` - plus a
handful of hand-written resolvers. This script parses those declarations and emits
``docs/runtime-functions-reference.md``: for every function, the exact Rust setter name and
resolver type, and for every set-returning function, the full row-struct definition with
each field's Rust type. Reading the declarations directly keeps the reference from drifting
from the code.

Run from the project ROOT:

    .venv/bin/python -m claude-scripts.generate_runtime_function_reference

The output is committed; re-run it after changing the resolver declarations.
"""

import re
from pathlib import Path

SOURCE = Path("src/runtime_function_resolvers.rs")
OUTPUT = Path("docs/runtime-functions-reference.md")

# Rust field type for a row-struct column kind (mirrors `col_rust_ty!`).
COLUMN_RUST_TYPE = {
    "text": "String",
    "bool": "bool",
    "int4": "i32",
    "int8": "i64",
    "float8": "f64",
    "timestamptz": "i64",
}

# Scalar return kind -> the `Option<T>` a resolver yields (mirrors `scalar_resolver_ty!`).
SCALAR_RETURN_RUST = {
    "int8": "i64",
    "int4": "i32",
    "float8": "f64",
    "bool": "bool",
    "timestamptz": "i64",
}


def _camel(snake: str) -> str:
    """Upper-camel-case a function name the way the macros name its row struct.

    ``pg_lock_status`` -> ``PgLockStatus``. A leading raw-identifier marker (``r#type``)
    is preserved on the field name elsewhere; struct names never contain one.
    """
    return "".join(part.capitalize() for part in snake.split("_"))


def _extract_block(text: str, macro: str) -> str:
    """Return the body between ``<macro> {`` and its closing brace at column 0."""
    match = re.search(macro + r"!\s*\{(.*?)\n\}", text, re.DOTALL)
    if not match:
        raise SystemExit(f"could not find {macro}! block in {SOURCE}")
    return match.group(1)


def parse_scalars(block: str):
    """Yield (function, setter, resolver_type, default) for each scalar declaration.

    Each declaration is ``name (arg?) -> ret (= default)?;`` where ``arg`` is ``oid`` or
    empty and ``ret`` is one of the scalar return kinds.
    """
    pattern = re.compile(
        r"^\s*(\w+)\s*\(\s*(\w*)\s*\)\s*->\s*(\w+)\s*(?:=\s*([^;]+?))?\s*;",
        re.MULTILINE,
    )
    for name, arg, ret, default in pattern.findall(block):
        inner = SCALAR_RETURN_RUST[ret]
        takes = "i64" if arg == "oid" else ""
        resolver = f"Arc<dyn Fn({takes}) -> Option<{inner}> + Send + Sync>"
        note = " _(timestamp, microseconds UTC)_" if ret == "timestamptz" else ""
        if default:
            default_text = f"`{default.strip()}`"
        else:
            default_text = "`None` (SQL NULL)"
        yield name, f"set_{name}_resolver", f"`{resolver}`{note}", default_text


def parse_tables(block: str):
    """Yield (function, struct_name, setter, [(field, rust_type)]) per SRF declaration.

    Each declaration is ``name -> { col: kind, ... };``.
    """
    pattern = re.compile(r"^\s*(\w+)\s*->\s*\{(.*?)\}\s*;", re.MULTILINE)
    for name, cols in pattern.findall(block):
        fields = []
        for col in cols.split(","):
            col = col.strip()
            if not col:
                continue
            field, kind = (part.strip() for part in col.split(":"))
            fields.append((field, COLUMN_RUST_TYPE[kind]))
        yield name, f"{_camel(name)}Row", f"set_{name}_resolver", fields


# Resolvers defined outside the two macro tables (hand-written), listed here so the
# reference stays complete. The macro-derived entries above are parsed from source and
# cannot drift; these few are kept in sync with the hand-written resolvers in
# src/runtime_function_resolvers.rs and src/user_functions.rs by hand.
HANDWRITTEN_SCALARS = [
    (
        "pg_indexam_progress_phasename",
        "set_pg_indexam_progress_phasename_resolver",
        "`Arc<dyn Fn(i64, i64) -> Option<String> + Send + Sync>`",
        "`None` (SQL NULL)",
    ),
    (
        "pg_get_statisticsobjdef_expressions",
        "set_pg_get_statisticsobjdef_expressions_resolver",
        "`Arc<dyn Fn(i64) -> Option<Vec<String>> + Send + Sync>`",
        "`None` (SQL NULL)",
    ),
    (
        "pg_sequence_last_value",
        "set_pg_sequence_last_value_resolver",
        "`Arc<dyn Fn(i64) -> Option<i64> + Send + Sync>`",
        "`None` (SQL NULL)",
    ),
    (
        "row_security_active",
        "set_row_security_active_resolver",
        "`Arc<dyn Fn(i64) -> bool + Send + Sync>`",
        "`false`",
    ),
]


def render(scalars, tables) -> str:
    """Render the full reference markdown."""
    lines = [
        "# Runtime function reference",
        "",
        "Generated from `src/runtime_function_resolvers.rs` by",
        "`claude-scripts/generate_runtime_function_reference.py` - do not edit by hand.",
        "",
        "Every function below is supplied through the named setter; all setters, resolver",
        "type aliases, and row structs are re-exported from the crate root",
        "(`use datafusion_pg_catalog::...`). See [runtime-functions.md](runtime-functions.md)",
        "for the guide and worked examples.",
        "",
        "## Scalar functions",
        "",
        "Each takes the listed `Arc<dyn Fn ...>` resolver. A `timestamptz` result is an",
        "`i64` count of microseconds since the Unix epoch, UTC.",
        "",
        "| Function | Setter | Resolver type | Default |",
        "| --- | --- | --- | --- |",
    ]
    for name, setter, resolver, default in sorted(scalars):
        lines.append(f"| `{name}` | `{setter}` | {resolver} | {default} |")
    lines += [
        "",
        "## Set-returning functions",
        "",
        "Each resolver returns a `Vec` of the row struct shown; every field is `pub` and",
        "`Option`, and the struct derives `Default`, so `..Default::default()` leaves the",
        "columns you do not set as NULL. A `timestamptz` field is an `i64` count of",
        "microseconds since the Unix epoch, UTC.",
        "",
    ]
    for name, struct, setter, fields in sorted(tables):
        lines.append(f"### `{name}`")
        lines.append("")
        lines.append(f"Setter: `{setter}(Arc<dyn Fn() -> Vec<{struct}> + Send + Sync>)`")
        lines.append("")
        lines.append("```rust")
        lines.append(f"pub struct {struct} {{")
        for field, rust in fields:
            lines.append(f"    pub {field}: Option<{rust}>,")
        lines.append("}")
        lines.append("```")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def main() -> None:
    """Parse the resolver declarations and write the reference markdown."""
    text = SOURCE.read_text()
    scalars = list(parse_scalars(_extract_block(text, "scalar_resolvers")))
    scalars += HANDWRITTEN_SCALARS
    tables = list(parse_tables(_extract_block(text, "table_resolvers")))
    OUTPUT.write_text(render(scalars, tables))
    print(f"wrote {OUTPUT}: {len(scalars)} scalar, {len(tables)} set-returning functions")


if __name__ == "__main__":
    main()
