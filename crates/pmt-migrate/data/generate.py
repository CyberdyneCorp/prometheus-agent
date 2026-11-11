"""Regenerates the embedded Hermes facts from a Hermes checkout.

    python3 -I crates/pmt-migrate/data/generate.py <hermes-agent checkout>

Writes hermes-defaults.json (DEFAULT_CONFIG flattened to dotted keys; an
empty mapping is a leaf), hermes-env.json (OPTIONAL_ENV_VARS split into
credential and setting names) and hermes-personalities.json (the built-in
personalities rendered to prompt text) next to this script.
"""

import json
import pathlib
import subprocess
import sys

checkout = pathlib.Path(sys.argv[1]).resolve()
here = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(checkout))

import hermes_cli.config_defaults as defaults  # noqa: E402
import hermes_cli.personality as personality  # noqa: E402
from hermes_cli.config import OPTIONAL_ENV_VARS  # noqa: E402

revision = subprocess.run(
    ["git", "-C", str(checkout), "rev-parse", "--short=10", "HEAD"],
    capture_output=True, text=True, check=True,
).stdout.strip()

flat = {}


def walk(path, value):
    if isinstance(value, dict) and value:
        for key, inner in value.items():
            walk(path + [str(key)], inner)
    else:
        flat[".".join(path)] = value


walk([], defaults.DEFAULT_CONFIG)


def write(name, document):
    with open(here / name, "w", encoding="utf-8") as out:
        json.dump(document, out, indent=1, ensure_ascii=False, default=str)


write("hermes-defaults.json", {
    "source": f"hermes-agent {revision} (hermes_cli/config_defaults.py DEFAULT_CONFIG, flattened)",
    "config_version": defaults.DEFAULT_CONFIG["_config_version"],
    "defaults": dict(sorted(flat.items())),
})
write("hermes-env.json", {
    "source": f"hermes-agent {revision} (hermes_cli/config.py OPTIONAL_ENV_VARS)",
    "secrets": sorted(k for k, v in OPTIONAL_ENV_VARS.items() if v.get("password")),
    "settings": sorted(k for k, v in OPTIONAL_ENV_VARS.items() if not v.get("password")),
})
write("hermes-personalities.json", {
    "source": f"hermes-agent {revision} (hermes_cli/personality.py BUILTIN_PERSONALITIES, rendered)",
    "personalities": {
        k: personality.render_personality_prompt(v)
        for k, v in personality.BUILTIN_PERSONALITIES.items()
    },
})
