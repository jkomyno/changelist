await tools.call("mock.echo", { step: "completed before the error" });
await tools.call("mock.fail", { message: "controlled provider failure" });
return "unreachable";
