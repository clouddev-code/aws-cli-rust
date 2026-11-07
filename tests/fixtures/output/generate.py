"""Regenerate output-formatter fixtures from the reference AWS CLI.

Run with a Python environment where the reference awscli is installed:

    python tests/fixtures/output/generate.py > tests/fixtures/output/cases.json
"""

import io
import json
import sys
from types import SimpleNamespace

import jmespath

from awscli.formatter import StreamedYAMLFormatter, TableFormatter, YAMLFormatter

LONG = (
    "This is a fairly long description that keeps going past the eighty "
    "column limit so that the YAML emitter has to fold it onto more lines."
)

CASES = {
    "nested": {
        "Reservations": [
            {
                "ReservationId": "r-1",
                "OwnerId": "123456789012",
                "Instances": [
                    {
                        "InstanceId": "i-1",
                        "State": {"Name": "running", "Code": 16},
                        "Tags": [{"Key": "Name", "Value": "web"}],
                    },
                    {"InstanceId": "i-2", "State": {"Name": "stopped", "Code": 80}},
                ],
            }
        ]
    },
    "single_scalar_key": {"Account": "123456789012"},
    "flat": {"UserId": "AIDA", "Account": "1", "Arn": "arn:aws:iam::1:user/a"},
    "scalar_list": {"Names": ["a", "b", "c"]},
    "list_of_lists": {"Rows": [["a", 1], ["b", 2]]},
    "uneven_keys": {"Items": [{"A": 1, "B": True}, {"A": 2, "C": None}]},
    "wide_chars": {"Items": [{"Name": "日本語の名前", "Id": "1"}, {"Name": "x", "Id": "22"}]},
    "too_wide": {
        "Things": [
            {"Description": LONG, "Name": "first", "Id": "id-000001"},
        ]
    },
    "too_wide_many": {
        "Things": [
            {"Description": LONG, "Name": "first", "Id": "id-000001"},
            {"Description": "short", "Name": "second", "Id": "id-000002"},
        ]
    },
    "numbers": {"Int": 3, "Float": 1.5, "Big": 1e20, "Small": 0.0001, "Neg": -2, "Zero": 0.0},
    "empty_values": {"List": [], "Map": {}, "Str": "", "Null": None},
    "yaml_strings": {
        "true": "true",
        "yes": "yes",
        "on": "on",
        "null": "null",
        "tilde": "~",
        "int": "123",
        "float": "1.5",
        "exp": "1e3",
        "hex": "0x1F",
        "octal": "0o17",
        "time": "2015-08-30T12:36:00+00:00",
        "date": "2015-08-30",
        "colon": "a: b",
        "colon_end": "a:",
        "hash": "#x",
        "space_hash": "a #b",
        "lead": " lead",
        "trail": "trail ",
        "dash": "-",
        "dash_space": "- a",
        "at": "@x",
        "backtick": "`x",
        "quote": "it's",
        "dquote": 'say "hi"',
        "brace": "{a}",
        "bracket": "[a]",
        "comma": "a, b",
        "star": "*a",
        "amp": "&a",
        "bang": "!a",
        "pct": "%a",
        "pipe": "|a",
        "gt": ">a",
        "question": "?a",
        "question_space": "? a",
        "multiline": "line one\nline two",
        "trailing_newline": "line\n",
        "tab": "a\tb",
        "unicode": "héllo 日本",
        "emoji": "smile 😀",
        "control": "bell\x07",
        "long": LONG,
        "long_word": "x" * 100,
        "url": "https://example.com/a?b=c#d",
        "path": "/var/log/app.log",
        "arn": "arn:aws:s3:::bucket/key",
        "json": '{"a": 1}',
        "equals": "a=b",
        "plain": "hello world",
        "dot": ".",
        "dots": "...",
        "docstart": "---",
        "nan": ".nan",
        "inf": ".inf",
        "y": "y",
        "n": "n",
        "No": "No",
        "OFF": "OFF",
    },
    "yaml_folding": {
        "long_tab": LONG.replace(" so ", "\tso "),
        "long_lead": " " + LONG,
        "long_quote": LONG + " it's",
        "long_multiline": LONG + "\n" + LONG,
        "space_break": "a \nb",
        "break_space": "a\n b",
        "wide": "日本語 " * 30,
        "k" * 130: "long key",
        "multi\nline key": 1,
        "": "empty key",
        "nested": [{"deep": {"deeper": [LONG, " " + LONG, LONG.replace(" ", "\t", 3)]}}],
    },
    "bools_and_nulls": {"T": True, "F": False, "N": None},
    "top_list": [{"a": 1}, {"a": 2, "b": [1, 2]}],
    "nested_lists": {"L": [[1, 2], [], [[3]]], "M": [{"x": []}]},
    "unsorted_keys": {"b": 1, "a": 2, "C": 3, "_x": 4},
}

SCALARS = {"scalar_string": "hello", "scalar_int": 5, "scalar_null": None, "scalar_bool": True}


def render(cls, data, scalar=False):
    # Scalars only reach the formatter through --query.
    query = jmespath.compile("v") if scalar else None
    ns = SimpleNamespace(query=query, color="off")
    out = io.StringIO()
    cls(ns)("OperationName", {"v": data} if scalar else data, out)
    return out.getvalue()


cases = []
for name, data in {**CASES, **SCALARS}.items():
    case = {"name": name, "data": data}
    scalar = name in SCALARS
    case["yaml"] = render(YAMLFormatter, data, scalar)
    case["yaml_stream"] = render(StreamedYAMLFormatter, data, scalar)
    if name not in SCALARS:
        try:
            case["table"] = render(TableFormatter, data)
        except ValueError as e:
            case["table_error"] = str(e)
    cases.append(case)

json.dump(cases, sys.stdout, indent=1, ensure_ascii=False)
sys.stdout.write("\n")
