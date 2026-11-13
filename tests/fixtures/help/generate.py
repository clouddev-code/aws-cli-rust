"""Regenerate help-page fixtures from the reference AWS CLI.

Run with a Python environment where the reference awscli is installed:

    python tests/fixtures/help/generate.py tests/fixtures/help

Each `<command>_<subcommand>.txt` file is rewritten with the output of
`aws <command> <subcommand> help`, as `col -b | expand` would show it. The
pages are always rendered with mandoc (awscli prefers groff when both are
installed), so the files do not depend on which one the machine has.
"""

import io
import os
import re
import sys

from awscli import help as awscli_help
from awscli.clidriver import create_clidriver


def render(args):
    out = io.StringIO()

    def capture(self, output):
        out.write(output.decode("utf-8"))

    awscli_help.PosixHelpRenderer._send_output_to_pager = capture
    awscli_help.PosixHelpRenderer._exists_on_path = (
        lambda self, name: name == "mandoc"
    )
    rc = create_clidriver().main(args + ["help"])
    if rc not in (0, None):
        sys.exit(f"aws {' '.join(args)} help exited with {rc}")
    # col -b: keep the last character of each overstrike (bold, underline).
    text = re.sub(r".\x08", "", out.getvalue())
    return "\n".join(line.expandtabs() for line in text.split("\n"))


def main():
    directory = sys.argv[1]
    os.environ.setdefault("AWS_DEFAULT_REGION", "us-east-1")
    for name in sorted(os.listdir(directory)):
        if not name.endswith(".txt"):
            continue
        args = name[: -len(".txt")].split("_")
        with open(os.path.join(directory, name), "w") as f:
            f.write(render(args))


if __name__ == "__main__":
    main()
