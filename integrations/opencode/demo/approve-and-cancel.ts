// Guest program for the approval and cancellation check. `publish` waits for an
// approval reply; the slow `wait` call is then aborted through OpenCode.
const published = await tools.call("publish", { message: "approved restock" });
const waited = await tools.call("wait", { ms: 30000, value: "too late" });
return { published, waited };
