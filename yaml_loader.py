"""Fast YAML loading shared across the test suite and the catalog tooling.

The catalog dump has a few very large files (pg_proc, pg_operator, pg_amop,
pg_depend); libyaml's C loader parses them ~7x faster than the pure-Python loader
(~10s vs ~70s over the whole set), which otherwise dominates the snapshot test and
the view analyzer. Everything that reads catalog YAML goes through this one helper.
"""

import yaml

try:
    from yaml import CSafeLoader as _Loader
except ImportError:  # pragma: no cover - libyaml not installed
    _Loader = yaml.SafeLoader


def load_yaml(path):
    """Parse the YAML file at `path` with libyaml when available."""
    with open(path) as handle:
        return yaml.load(handle, Loader=_Loader)


def load_yaml_stream(stream):
    """Parse an already-open YAML file object with libyaml when available."""
    return yaml.load(stream, Loader=_Loader)


def walk_catalog_objects(doc):
    """Yield ``(schema, name, node)`` for each object leaf in a catalog YAML doc.

    The catalog YAML nests ``catalog -> schema -> object -> {type, ...}``; an
    object node is any dict carrying a non-dict ``type`` key. Yielding the
    surrounding schema and object name lets callers key objects without
    re-deriving them from the file name. Schema or name fall back to ``"?"`` when
    the node is shallower than the usual nesting.
    """
    stack = [([], doc)]
    while stack:
        prefix, node = stack.pop()
        if not isinstance(node, dict):
            continue
        if "type" in node and not isinstance(node["type"], dict):
            schema = prefix[-2] if len(prefix) >= 2 else "?"
            name = prefix[-1] if prefix else "?"
            yield schema, name, node
            continue
        for key, value in node.items():
            stack.append((prefix + [key], value))


def find_in_doc(node, key):
    """Return the first non-dict value stored under `key` anywhere in `node`.

    The catalog YAML nests each object a few levels deep
    (`database -> schema -> table -> {type, view_sql, rows, ...}`); this walks to
    the first matching leaf so callers do not depend on the exact nesting.
    """
    if isinstance(node, dict):
        if key in node and not isinstance(node[key], dict):
            return node[key]
        for value in node.values():
            found = find_in_doc(value, key)
            if found is not None:
                return found
    return None
