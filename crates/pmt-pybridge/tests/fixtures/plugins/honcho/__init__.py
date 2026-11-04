"""A Hermes plugin with a `hermes honcho ...` style CLI command."""


def _setup(parser):
    parser.add_argument("action", choices=["status", "sync"])


def _run(args):
    print(f"honcho {args.action}: ok")


def register(ctx):
    ctx.register_cli_command("honcho", "Manage the Honcho memory provider", _setup, _run)
