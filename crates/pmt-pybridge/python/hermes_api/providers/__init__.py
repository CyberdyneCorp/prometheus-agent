"""``providers``: model-provider profile registration."""

from prometheus_bridge import state


def register_provider(profile):
    """Records a ProviderProfile; the bridge translates it for the host."""
    state.REGISTRY["providers"][profile.name] = profile
    return profile


def get_provider_profile(name):
    return state.REGISTRY["providers"].get(name)


def list_providers():
    return list(state.REGISTRY["providers"].values())
