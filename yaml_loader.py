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
