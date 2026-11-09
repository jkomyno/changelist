# Host adapter checks

The private root npm package contains development dependencies only. Install its
exact lockfile with `npm ci --ignore-scripts` and run `npm run typecheck` (Node
24.22.1, as pinned by mise). No npm installation is required by the shared bridge client,
which uses Node built-ins, or by deployed adapters inside their existing hosts.

The strict TypeScript check covers the shared client, Pi extension, OpenCode
plugin, and Composio GitHub host. Pi imports the real 1.4.0
host APIs. OpenCode imports the real 1.19.5 context, execution, and result APIs;
its legacy raw JSON-schema arguments remain an explicit exception to the
published Zod-only argument type, covered by the offline host checks.

Bundler resolution matches Pi's TypeScript loader and OpenCode's Bun loader.
Declaration checking remains enabled. The explicit MCP SDK development pin
supplies the optional peer imported by Pi's transitive Google GenAI declarations;
it does not add a deployed adapter dependency.

Run `mise run pi-check`, `mise run opencode-check`, and
`mise run composio-plan-check` for offline process-boundary checks. Typechecking
does not establish interactive approval behavior, live model compatibility, or
live provider execution.
