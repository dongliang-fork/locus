## Locus tool environment

Native Read, Write, Edit, Glob, Grep and Bash calls are bridged to the Locus client, not executed in the Claude CLI runtime directory. Use the client OS, shell and Working directory in Environment below. The following rules describe the actual execution behavior:

- Each Bash call starts in the workspace Working directory. Do not prepend `cd` to that same directory. Use `cd` only when the command needs a different directory; the change lasts for that call only.
- Relative file-tool paths resolve against the workspace, not a previous Bash or Python directory. Glob/Grep use the workspace when `path` is omitted. Follow each tool's schema; absolute client paths are also valid.
- On Windows, file-tool paths must use Windows paths such as `F:/Project/src/file.cs` or workspace-relative paths. Do not translate them to Linux or Git Bash `/f/...` paths. File-tool arguments do not expand `~` or shell variables.
- Each Python call also starts in the workspace. `os.chdir()` changes only that Python process's cwd, not the injected `project`, SDK checkout binding, or later tool calls. Use the SDK's `worktree` or `workspace_ref` selector to target another checkout; changing directories does not retarget knowledge, code or Unity tools.
- Glob matches files, not directory trees. Use Bash `ls` for directory listings, without a redundant `cd`. Narrow Glob/Grep searches with `path` and patterns; do not assume generated directories are excluded.
- Third-party MCP tools follow their own server's environment and path contract. Do not assume they inherit the Locus workspace cwd.
- WebSearch and WebFetch run natively in Claude Code, using the CLI host's network (the remote host for SSH connections). Use WebFetch with its URL and extraction prompt; Locus web_fetch is unavailable and is not a fallback.
