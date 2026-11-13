"""Regenerate --generate-cli-skeleton fixtures from the reference AWS CLI.

    python tests/fixtures/skeleton/generate.py > tests/fixtures/skeleton/cases.json

`input` and `yaml-input` come from running the CLI. `output` is built with
botocore's ArgumentGenerator directly, because the CLI validates the stubbed
response and fails for many operations.
"""

import json
import os
import subprocess
import sys

import awscli.botocore.session as botocore_session
from awscli.botocore import xform_name
from awscli.botocore.utils import ArgumentGenerator

AWS = [sys.executable, "-m", "awscli"]

OPERATIONS = [
    ("sts", "AssumeRole"),
    ("sts", "GetCallerIdentity"),
    ("dynamodb", "CreateTable"),
    ("dynamodb", "PutItem"),
    ("s3api", "PutObject"),
    ("cloudtrail", "LookupEvents"),
    ("sqs", "SetQueueAttributes"),
    ("ec2", "RunInstances"),
    ("lambda", "CreateFunction"),
    ("events", "PutEvents"),
    ("ec2", "DescribeInstances"),
]
SERVICE = {"s3api": "s3"}

session = botocore_session.get_session()
cases = []
for command, op in OPERATIONS:
    cli_op = xform_name(op, "-")
    for mode in ("input", "yaml-input"):
        out = subprocess.run(
            [*AWS, command, cli_op, "--generate-cli-skeleton", mode],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        cases.append({"command": command, "operation": cli_op, "mode": mode, "expected": out})
    model = session.get_service_model(SERVICE.get(command, command))
    shape = model.operation_model(op).output_shape
    skeleton = ArgumentGenerator(use_member_names=True).generate_skeleton(shape) if shape else {}
    # The JSON formatter prints nothing for an empty response.
    text = (
        json.dumps(skeleton, indent=4, default=str, ensure_ascii=False).replace(
            "1970-01-01 00:00:00", "1970-01-01T00:00:00"
        )
        + "\n"
        if skeleton
        else ""
    )
    cases.append(
        {
            "command": command,
            "operation": cli_op,
            "mode": "output",
            "expected": text,
        }
    )
json.dump(cases, sys.stdout, indent=1, ensure_ascii=False)
sys.stdout.write("\n")
