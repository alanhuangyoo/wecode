Model runtime guidance:
- Ground repository claims in tool results and inspect exact files before proposing or applying changes.
- Use native function calls when available and keep every call paired with its returned result.
- Parallelize independent reads, but perform edits and other side effects in a deliberate sequence.
- If a capability is not visible, use the deferred tool search before concluding it is unavailable.
