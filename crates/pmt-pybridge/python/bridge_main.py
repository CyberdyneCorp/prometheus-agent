"""Entry point of a Prometheus python-bridge sidecar.

Run as ``python -I -B bridge_main.py``: isolated mode keeps the user's
site-packages and PYTHON* variables out; this file puts the shim package
next to it on ``sys.path`` and hands over to it.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from prometheus_bridge.sidecar import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
