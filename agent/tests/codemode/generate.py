#!/usr/bin/env python3
"""Generate the offline seed corpus and prompts for real model-written scripts."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
CORPUS = ROOT / "corpus"
ALPHA = 'pub fn alpha() {\n    println!("alpha");\n}\n// needle alpha\n'
BETA = 'def beta():\n    return "beta"\n\n# needle beta\n'
GUIDE = "# Guide\n\nalpha beta alpha\n"
MODELS = ["frontier-a", "frontier-b", "mid-size-a", "mid-size-b", "local-a"]
TASKS = [
    "Read src/alpha.rs and return its character count.",
    "Count occurrences of needle in src/alpha.rs.",
    "Return the number of lines in src/beta.rs.",
    "Read docs/guide.md and return it in uppercase.",
    "Read data.json as JSON and sum its items.",
    "Read data.json as JSON and return its name.",
    "Read one numbered line from src/alpha.rs with offset 2.",
    "Count alpha regex matches in src/alpha.rs.",
    "Return the number of distinct words in docs/guide.md.",
    "Check whether stripped src/beta.rs ends in beta.",
    "Print the alpha count and return the source length.",
    "Parse data.json and return the number of items.",
    "Glob the project and return the number of paths.",
    "Glob the project and filter paths ending in .rs.",
    "Glob the project and return only Markdown paths.",
    "Grep alpha and return the number of matches.",
    "Grep alpha and return the distinct matching paths.",
    "Grep alpha and sum the match line numbers.",
    "List the root directory and return its entry count.",
    "Check whether list_dir returns Cargo.toml.",
    "Run a mock bash command and return its exit code.",
    "Run a mock bash command and count stdout lines.",
    "Run a mock bash command and check that stderr is empty.",
    "Run a mock bash command and uppercase stdout.",
    "Edit src/alpha.rs and return the tool result.",
    "Edit src/beta.rs and return the tool result.",
    "Edit docs/guide.md and return the tool result.",
    "Edit src/alpha.rs with an empty replacement.",
    "Read alpha and beta concurrently and return both lengths.",
    "Glob all files, read them concurrently, and sum source lengths.",
]


def toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return str(value).lower()
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{json.dumps(str(key))} = {toml_value(item)}" for key, item in value.items()) + " }"
    raise TypeError(f"unsupported expectation value: {value!r}")


def write_case(
    name: str,
    code: str,
    calls: list[str] | None = None,
    value: Any = None,
    *,
    failure: str | None = None,
    stdout: str | None = None,
    adversarial: bool = False,
    task: str = "",
) -> None:
    base = name[:-3] if name.endswith(".py") else name
    (CORPUS / f"{base}.py").write_text(code.rstrip() + "\n")
    expected = ['origin = "adversarial"' if adversarial else 'origin = "deterministic-seed"']
    if task:
        expected.append(f"task = {json.dumps(task)}")
    if adversarial:
        expected.append("adversarial = true")
    expected.append(f"calls = {toml_value(calls or [])}")
    if failure is not None:
        expected.append(f"failure = {json.dumps(failure)}")
    if value is not None:
        expected.append(f"value = {toml_value(value)}")
    if stdout is not None:
        expected.append(f"stdout = {toml_value(stdout)}")
    (CORPUS / f"{base}.expect.toml").write_text("\n".join(expected) + "\n")


def variants(call: str, kwargs_call: str, expression: str = "value") -> list[str]:
    return [
        f"value = await {call}\n{expression}",
        f"value = await {kwargs_call}\n{expression}",
        f"async def run():\n    return await {call}\nvalue = await run()\n{expression}",
        f"try:\n    value = await {call}\nexcept ToolError as error:\n    value = str(error)\n{expression}",
    ]


def regular_cases() -> None:
    number = 0

    def family(task: str, calls: list[str], snippets: list[str], values: list[Any], stdout: str | None = None) -> None:
        nonlocal number
        assert len(snippets) == len(values) == 4
        for style, (code, value) in enumerate(zip(snippets, values)):
            number += 1
            write_case(
                f"regular_{number:03d}_{['direct', 'kwargs', 'helper', 'caught'][style]}.py",
                code,
                calls,
                value,
                stdout=stdout,
                task=task,
            )

    read_tasks = [
        (TASKS[0], "src/alpha.rs", len(ALPHA), "len(value)"),
        (TASKS[1], "src/alpha.rs", 1, 'value.count("needle")'),
        (TASKS[2], "src/beta.rs", 4, "len(value.splitlines())"),
        (TASKS[3], "docs/guide.md", GUIDE.upper(), "value.upper()"),
        (TASKS[4], "data.json", 6, 'sum(json.loads(value)["items"])'),
        (TASKS[5], "data.json", "demo", 'json.loads(value)["name"]'),
        (TASKS[7], "src/alpha.rs", 3, 'len(re.findall("alpha", value))'),
        (TASKS[8], "docs/guide.md", 4, "len(set(value.split()))"),
        (TASKS[9], "src/beta.rs", True, 'value.strip().endswith("beta")'),
    ]
    for task, path, expected, expression in read_tasks:
        snippets = variants(f"read_text({path!r})", f"read_text(path={path!r})", expression)
        if "json." in expression:
            snippets = ["import json\n" + snippet for snippet in snippets]
        if "re." in expression:
            snippets = ["import re\n" + snippet for snippet in snippets]
        family(task, ["read_text"], snippets, [expected] * 4)

    offset_task = TASKS[6]
    call = "read('src/alpha.rs', 2, 1)"
    kwargs = "read(path='src/alpha.rs', offset=2, limit=1)"
    family(offset_task, ["read"], variants(call, kwargs), ['2:     println!("alpha");\n'] * 4)

    printed = []
    for snippet in variants("read_text('src/alpha.rs')", "read_text(path='src/alpha.rs')", "len(value)"):
        lines = snippet.splitlines()
        lines.insert(-1, "print(value.count('alpha'))")
        printed.append("\n".join(lines))
    family(TASKS[10], ["read_text"], printed, [len(ALPHA)] * 4, "3\n")
    json_snippets = ["import json\n" + snippet for snippet in variants("read_text('data.json')", "read_text(path='data.json')", 'len(json.loads(value)["items"])')]
    family(TASKS[11], ["read_text"], json_snippets, [3] * 4)

    family(TASKS[12], ["glob"], variants("glob('*')", "glob(pattern='*')", "len(value)"), [3] * 4)
    family(TASKS[13], ["glob"], variants("glob('*.rs')", "glob(pattern='*.rs')", "[path for path in value if path.endswith('.rs')]"), [["src/alpha.rs", "src/beta.rs"]] * 4)
    family(TASKS[14], ["glob"], variants("glob('*.md')", "glob(pattern='*.md')", "[path for path in value if path.endswith('.md')]"), [["docs/guide.md"]] * 4)
    family(TASKS[15], ["grep"], variants("grep('alpha')", "grep(pattern='alpha')", "len(value)"), [3] * 4)
    family(TASKS[16], ["grep"], variants("grep('alpha')", "grep(pattern='alpha')", "sorted({match['path'] for match in value})"), [[ "src/alpha.rs", "src/beta.rs" ]] * 4)
    family(TASKS[17], ["grep"], variants("grep('alpha')", "grep(pattern='alpha')", "sum(match['line'] for match in value)"), [6] * 4)
    family(TASKS[18], ["list_dir"], variants("list_dir('.')", "list_dir(path='.')", "len(value)"), [3] * 4)
    family(TASKS[19], ["list_dir"], variants("list_dir('.')", "list_dir(path='.')", "'Cargo.toml' in value"), [True] * 4)

    family(TASKS[20], ["bash"], variants("bash('printf alpha')", "bash(command='printf alpha')", "value['exit_code']"), [0] * 4)
    family(TASKS[21], ["bash"], variants("bash('printf alpha')", "bash(command='printf alpha')", "len(value['stdout'].splitlines())"), [2] * 4)
    family(TASKS[22], ["bash"], variants("bash('printf alpha')", "bash(command='printf alpha')", "value['stderr'] == ''"), [True] * 4)
    family(TASKS[23], ["bash"], variants("bash('printf alpha')", "bash(command='printf alpha')", "value['stdout'].upper()"), ["ALPHA\nBETA\n"] * 4)

    for task, path, old, new in [
        (TASKS[24], "src/alpha.rs", "alpha", "gamma"),
        (TASKS[25], "src/beta.rs", "beta", "delta"),
        (TASKS[26], "docs/guide.md", "Guide", "Handbook"),
        (TASKS[27], "src/alpha.rs", "needle", ""),
    ]:
        call = f"edit({path!r}, {old!r}, {new!r})"
        kwargs = f"edit(path={path!r}, old_string={old!r}, new_string={new!r})"
        family(task, ["edit"], variants(call, kwargs), [f"updated {path}"] * 4)

    gather_call = "asyncio.gather(read_text('src/alpha.rs'), read_text('src/beta.rs'))"
    gather_kwargs = "asyncio.gather(read_text(path='src/alpha.rs'), read_text(path='src/beta.rs'))"
    gather_snippets = ["import asyncio\n" + snippet for snippet in variants(gather_call, gather_kwargs, "[len(text) for text in value]")]
    family(TASKS[28], ["read_text", "read_text"], gather_snippets, [[len(ALPHA), len(BETA)]] * 4)

    loop_code = "paths = await glob('*')\nvalues = []\nfor path in paths:\n    values.append(await read_text(path))\nsum(len(text) for text in values)"
    gather_code = "import asyncio\npaths = await glob('*')\nvalues = await asyncio.gather(*[read_text(path) for path in paths])\nsum(len(text) for text in values)"
    helper_code = "import asyncio\nasync def load(path):\n    return await read_text(path)\npaths = await glob('*')\nvalues = await asyncio.gather(*[load(path) for path in paths])\nsum(len(text) for text in values)"
    caught_code = "import asyncio\ntry:\n    paths = await glob('*')\n    values = await asyncio.gather(*[read_text(path) for path in paths])\nexcept ToolError:\n    values = []\nsum(len(text) for text in values)"
    family(TASKS[29], ["glob", "read_text", "read_text", "read_text"], [loop_code, gather_code, helper_code, caught_code], [sum(map(len, [ALPHA, BETA, GUIDE]))] * 4)
    assert number == 120


def adversarial_cases() -> None:
    number = 0

    def family(kind: str, snippets: list[str], failure: str, calls: list[str] | None = None) -> None:
        nonlocal number
        for code in snippets:
            number += 1
            write_case(f"adversarial_{number:03d}_{kind}.py", code, calls, failure=failure, adversarial=True, task=kind)

    family("missing_await", ["read_text('src/alpha.rs').strip()", "len(read_text('src/alpha.rs'))"], "runtime")
    family("banned_import", ["import pandas\npandas.DataFrame()", "import os\nos.listdir('.')"], "unsupported")
    family("syntax", ["value =", "def broken(:\n    pass"], "syntax")
    family("infinite_loop", ["while True:\n    pass", "value = 0\nwhile value >= 0:\n    value += 1"], "limit")
    family("deep_recursion", ["def recur():\n    return recur()\nrecur()", "def recur(n):\n    return recur(n + 1)\nrecur(0)"], "limit")
    family("huge_print", ["print('x' * 20000)", "for index in range(20000):\n    print(index)"], "limit")
    family("match", ["value = 1\nmatch value:\n    case 1:\n        result = True\nresult", "match {'a': 1}:\n    case {'a': value}:\n        value"], "unsupported")
    family("inheritance", ["class Base:\n    pass\nclass Child(Base):\n    pass", "class Child(dict):\n    pass"], "unsupported")
    family("open", ["open('src/alpha.rs')", "with open('src/alpha.rs') as source:\n    source.read()"], "unsupported")
    family("call_cap", ["for index in range(6):\n    await read_text('src/alpha.rs')", "async def load():\n    await read_text('src/alpha.rs')\nfor index in range(6):\n    await load()"], "limit", ["read_text"] * 4)
    family("tool_error", ["await edit('src/alpha.rs', 'missing', 'new')", "await edit(path='src/beta.rs', old_string='missing', new_string='new')"], "tool_error", ["edit"])
    family("unknown_tool", ["await missing_tool('x')", "await search_code('x')"], "unknown_tool")
    family("huge_value", ["'x' * 20000", "result = 'y' * 20001\nresult"], "limit")
    family("wrong_type", ["value = await read_text('src/alpha.rs')\nraise ValueError('boom')", "text = await read_text('src/alpha.rs')\nresult = int(text)\nresult"], "runtime", ["read_text"])
    family("sleep", ["import asyncio\nawait asyncio.sleep(1)", "import asyncio\nasync def wait():\n    await asyncio.sleep(1)\nawait wait()"], "unsupported")
    assert number == 30


def write_prompts() -> None:
    description = (ROOT / "tool_description.md").read_text().replace("{{STUBS}}", (ROOT / "stubs.pyi").read_text())
    with (ROOT / "prompts.jsonl").open("w") as output:
        for model in MODELS:
            for index, task in enumerate(TASKS, 1):
                record = {
                    "id": f"{model}-{index:02d}",
                    "model": model,
                    "task": task,
                    "prompt": description + "\nWrite only the Python script for this task:\n" + task,
                    "destination": f"model-corpus/{model}/{index:02d}.py",
                }
                output.write(json.dumps(record) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--prompts-only", action="store_true", help="write model prompts without replacing the corpus")
    args = parser.parse_args()
    if args.prompts_only:
        write_prompts()
        return
    CORPUS.mkdir(exist_ok=True)
    for path in CORPUS.iterdir():
        if path.is_file():
            path.unlink()
    regular_cases()
    adversarial_cases()
    write_prompts()


if __name__ == "__main__":
    main()
