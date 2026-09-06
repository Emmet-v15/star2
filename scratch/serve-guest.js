// Static file server for the guest page during local testing.
const dir = new URL("../crates/star2-app/guest/", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
Bun.serve({
  port: 8080,
  fetch(req) {
    const path = new URL(req.url).pathname;
    const file = path === "/" ? "index.html" : path.slice(1);
    return new Response(Bun.file(dir + file));
  },
});
console.log("guest page on http://localhost:8080");
