// 生成一个简单的 Windows .ico（多尺寸、32 位 BGRA），供 tauri-build 使用。
const fs = require("fs");
const path = require("path");

const SIZES = [16, 24, 32, 48, 64];

function pixelsFor(size) {
  const px = Buffer.alloc(size * size * 4);

  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      const i = (y * size + x) * 4;
      const margin = Math.max(1, Math.round(size / 12));
      const radius = Math.max(2, Math.round(size / 6));

      // 圆角方块
      const cx = Math.min(Math.max(x, margin + radius), size - margin - radius - 1);
      const cy = Math.min(Math.max(y, margin + radius), size - margin - radius - 1);
      const dx = x - cx;
      const dy = y - cy;
      const inside =
        x >= margin &&
        x < size - margin &&
        y >= margin &&
        y < size - margin &&
        dx * dx + dy * dy <= radius * radius;

      if (!inside) {
        px[i + 3] = 0;
        continue;
      }

      // 绿色渐变
      const t = (x + y) / (2 * (size - 1));
      const r = Math.round(28 + t * 46);
      const g = Math.round(150 + t * 78);
      const b = Math.round(92 + t * 48);
      px[i] = b;
      px[i + 1] = g;
      px[i + 2] = r;
      px[i + 3] = 255;

      // 右下角一点高光
      if (x > size * 0.55 && y > size * 0.55 && (x + y) % Math.max(2, size >> 2) === 0) {
        px[i] = 255;
        px[i + 1] = 255;
        px[i + 2] = 255;
      }
    }
  }

  return px;
}

function icoImage(size) {
  const rgba = pixelsFor(size);
  const xor = Buffer.alloc(size * size * 4);

  // ICO 的 BMP 像素是自下而上存储的
  for (let y = 0; y < size; y += 1) {
    const srcY = size - 1 - y;
    rgba.copy(xor, y * size * 4, srcY * size * 4, (srcY + 1) * size * 4);
  }

  const andMask = Buffer.alloc((size * size) / 8);
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(size, 4);
  header.writeInt32LE(size * 2, 8);
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);
  header.writeUInt32LE(0, 16);
  header.writeUInt32LE(xor.length + andMask.length, 20);

  return Buffer.concat([header, xor, andMask]);
}

const images = SIZES.map((size) => ({ size, data: icoImage(size) }));
const dir = Buffer.alloc(6 + 16 * images.length);
dir.writeUInt16LE(0, 0);
dir.writeUInt16LE(1, 2);
dir.writeUInt16LE(images.length, 4);

let offset = dir.length;
images.forEach((image, index) => {
  const entry = 6 + index * 16;
  dir[entry] = image.size >= 256 ? 0 : image.size;
  dir[entry + 1] = image.size >= 256 ? 0 : image.size;
  dir[entry + 2] = 0;
  dir[entry + 3] = 0;
  dir.writeUInt16LE(1, entry + 4);
  dir.writeUInt16LE(32, entry + 6);
  dir.writeUInt32LE(image.data.length, entry + 8);
  dir.writeUInt32LE(offset, entry + 12);
  offset += image.data.length;
});

const output = path.join(__dirname, "..", "crates", "client", "icons", "icon.ico");
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, Buffer.concat([dir, ...images.map((image) => image.data)]));
console.log(`wrote ${output} (${offset} bytes)`);
