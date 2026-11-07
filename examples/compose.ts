const values: unknown[] = await Promise.all(
  [
    { ms: 100, value: "first" },
    { ms: 5, value: "second" },
    { ms: 100, value: "third" },
    { ms: 5, value: "fourth" },
  ].map((args) => tools.call("mock.wait", args)),
);

return { selected: values.slice(0, 2), count: values.length };
