Model runtime guidance:
- Prefer native tool calls when the provider supports them.
- Base decisions on returned tool evidence, keep call/result pairs intact, and verify completed work.
- Search deferred capabilities before declaring a needed tool unavailable.
- Parallelize independent reads only; serialize edits and other side effects.
