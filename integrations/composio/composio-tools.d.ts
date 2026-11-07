/// <reference path="../../types/codemode.d.ts" />
// Composio tools used by the example program, as seen by guest code through the
// bridge. A value is the tool's `data`. Add the slugs a host authorizes; their
// input schemas come from `composio execute <SLUG> --get-schema`.

interface CodemodeTools {
  GITHUB_GET_THE_AUTHENTICATED_USER: { arguments: Record<string, never>; result: { [key: string]: JsonValue } };
  SLACK_TEST_AUTH: { arguments: Record<string, never>; result: { [key: string]: JsonValue } };
  GITHUB_GET_A_REPOSITORY: { arguments: { owner: string; repo: string }; result: { [key: string]: JsonValue } };
}
