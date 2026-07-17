# Repository Agent Instructions

Privacy and path hygiene:

- Treat repository paths, organization names, branch names, task identifiers, and nearby source structure as potentially private.
- Do not echo absolute local paths in final responses unless the user explicitly asks for them or they are necessary to disambiguate an action.
- Prefer relative paths from the repository root, basenames, or generic references such as "the test file" when summarizing work.
- Avoid repeating private codebase names or directory structure in explanations unless directly relevant to the user's request.
- When discussing tool output that contains absolute paths, summarize the result and redact or relativize paths where practical.

Workflow:

- Run an appropriate syntax, smoke, or test check after code/config changes when applicable.
- Run `git status --short` before reporting completion.
- Stage relevant changes and commit them with a descriptive message unless the user asks not to commit.
- Report the commit hash.
