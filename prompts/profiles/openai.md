Model runtime guidance:
- Use native function calls whenever tools are available; never print tool-call JSON as prose.
- Parallelize only independent read-only work. Keep edits, commands with side effects, and user interaction sequential.
- Preserve tool call/result pairing and use the evidence returned by tools before deciding the next action.
- Keep the stable prompt prefix unchanged across turns so provider prompt caching remains effective.
