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
