// Guest program for the approval checks. publish needs approval. The program keeps
// going after a refusal; once an approval is rejected, every later call is refused.
const attempt = (tool, args) => tools.call(tool, args).catch((error) => error.code);
const published = await attempt("publish", { message: "restock A-1" });
const stock = await attempt("lookup", { sku: "A-1" });
return { published, stock };
