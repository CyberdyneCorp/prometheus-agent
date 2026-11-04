"""``register_cli(subparser)``: the memory provider's command tree."""


def register_cli(subparser):
    subparser.add_argument("action", choices=["status", "sync"])


def memcli_command(args):
    print(f"memcli {args.action}: ok")
