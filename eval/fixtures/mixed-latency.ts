// Eight independent calls submitted together. Each four-call chunk starts with a
// slow call, so a batch barrier leaves three slots idle while it runs; rolling
// dispatch can refill them. Latencies are mirrored in eval/fixtures.json.
const latencies = [200, 25, 25, 25, 200, 25, 25, 25];
return await Promise.all(
  latencies.map((ms, index) => tools.call("mock.wait", { ms, value: index })),
);
