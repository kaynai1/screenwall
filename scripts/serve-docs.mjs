// 本地预览 docs/ 官网：`node scripts/serve-docs.mjs [端口]`，默认 4173。
// 只用 Node 内置模块，模拟 GitHub Pages 的静态行为（`/` → `index.html`）。
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, resolve } from "node:path";

const root = resolve("docs");
const port = Number(process.argv[2] || 4173);
const types = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".md": "text/markdown; charset=utf-8",
};

createServer(async (req, res) => {
  let path = decodeURIComponent(new URL(req.url, "http://localhost").pathname);
  if (path.endsWith("/")) path += "index.html";
  const file = join(root, path);
  if (!file.startsWith(root)) {
    res.writeHead(403).end("403");
    return;
  }
  try {
    const body = await readFile(file);
    res.writeHead(200, {
      "content-type": types[extname(file).toLowerCase()] || "application/octet-stream",
      "cache-control": "no-store",
    });
    res.end(body);
  } catch {
    res.writeHead(404, { "content-type": "text/plain; charset=utf-8" }).end("404");
  }
}).listen(port, () => console.log(`docs: http://localhost:${port}/`));
