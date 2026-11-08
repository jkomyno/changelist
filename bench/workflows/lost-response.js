// Lost response: read one page, then label the first open regression. The host applies
// the label but its response never arrives, so the run ends at its deadline.
const page = await tools.call("tracker.list_issues", { project: "atlas", limit: 40 });
const target = page.items.find((issue) => issue.state === "open" && issue.labels.includes("regression"));
const result = await tools.call("tracker.add_label", { id: target.id, label: "needs-triage" });
return { labelled: target.id, result: result };
