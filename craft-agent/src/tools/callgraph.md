Intra-file call graph analysis. Traces function/method call relationships within a single file.

Operations:
- `call_tree`: Show what a symbol calls (and their calls, recursively). Depth-limited.
- `callers`: Show which symbols in the file call the target symbol.
- `impact`: Show all symbols that transitively depend on the target (blast radius).

Limitations:
- Single-file scope only. Cross-file references appear as leaf nodes without expansion.
- Method calls like `obj.method()` are matched by the method name only.
- Cannot resolve dynamic dispatch (traits/interfaces, virtual calls).

Best for: understanding local call chains, finding blast radius of a change, locating callers of a function within a file.
