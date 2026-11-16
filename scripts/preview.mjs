// Показ окон программы в браузере без сборки: `node scripts/preview.mjs`.
// Страница без Tauri подставляет выдуманные данные (см. demo() в src/js/app.js).

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(fileURLToPath(new URL('.', import.meta.url)), '..', 'src');
const port = Number(process.env.PORT ?? 5199);
const types = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
};

createServer(async (request, response) => {
  const path = normalize(decodeURIComponent(new URL(request.url, 'http://x').pathname));
  const file = join(root, path === '\\' || path === '/' ? 'index.html' : path);
  if (!file.startsWith(root)) {
    response.writeHead(403).end();
    return;
  }
  try {
    const body = await readFile(file);
    response.writeHead(200, { 'content-type': types[extname(file)] ?? 'application/octet-stream' });
    response.end(body);
  } catch {
    response.writeHead(404).end('нет такого файла');
  }
}).listen(port, '127.0.0.1', () => console.log(`http://127.0.0.1:${port}`));
