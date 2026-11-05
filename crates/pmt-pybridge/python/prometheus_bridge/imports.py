"""The import rule: the documented Hermes plugin API resolves to the shim
modules; any other Hermes internal module raises ImportError by name."""

import importlib.abc
import importlib.machinery

from . import state


def _refusal(fullname):
    provided = state.API.get("shim_modules", ())
    return ImportError(
        f"cannot import '{fullname}': the Prometheus python bridge provides only the documented "
        f"Hermes plugin API ({', '.join(provided)}), not Hermes internal modules",
        name=fullname,
    )


class _Stub(importlib.abc.Loader):
    """An internal top-level package with nothing in it, so that importing
    one of its modules is refused by the module's full name."""

    def create_module(self, spec):
        return None

    def exec_module(self, module):
        name = module.__name__

        def __getattr__(attr):
            raise _refusal(f"{name}.{attr}")

        module.__getattr__ = __getattr__


class HermesFinder(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path, target=None):
        api = state.API
        top = fullname.split(".")[0]
        if top not in api.get("internal_packages", ()):
            return None
        provided = api.get("shim_modules", ())
        if fullname in provided or any(m.startswith(fullname + ".") for m in provided):
            return None
        if "." not in fullname:
            return importlib.machinery.ModuleSpec(fullname, _Stub(), is_package=True)
        raise _refusal(fullname)


def install():
    import sys

    if not any(isinstance(f, HermesFinder) for f in sys.meta_path):
        sys.meta_path.insert(0, HermesFinder())
