Use `code` for chained or dependent tool calls, filtering large results, and fan-out. Use a direct tool instead when one call needs no processing.

- Every tool is `async def`; await every call. Positional and keyword arguments follow the stub order.
- Top-level `await` is allowed. Do not wrap the script in `main()`.
- The final expression is returned. `print()` output is returned as well.
- `await asyncio.gather(*aws)` fans calls out. In the full tool, `await gather(*aws)` keeps partial results as `ToolError` values; pass `fail_fast=True` to raise.
- Catch `ToolError` around a failed tool call. Calls already completed remain completed.

Only these modules are available: `asyncio`, `collections`, `dataclasses`, `datetime`, `itertools`, `json`, `math`, `re`, and `typing`. Do not use class inheritance, `match`, or `open()`.

```python
paths = await glob("src/**/*.rs")
sources = await asyncio.gather(*[read_text(path) for path in paths])
needle_counts = {path: source.count("needle") for path, source in zip(paths, sources)}
needle_counts
```

```python
matches = await grep("fn main", "src")
by_file = {}
for match in matches:
    by_file[match["path"]] = by_file.get(match["path"], 0) + 1
by_file
```

```python
try:
    result = await edit("src/main.rs", "old", "new")
except ToolError as error:
    result = str(error)
result
```

```python
{{STUBS}}
```

Not callable here: `task`, `question`, `batch`, `code`, `list`, and `os`/filesystem functions. Use `list_dir()` for directory listings and `glob()`/`read_text()` for files.
