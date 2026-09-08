// Requires sharp (available in the bundled workspace runtime via NODE_PATH).
const fs = require("node:fs/promises");
const path = require("node:path");
const assert = require("node:assert/strict");
const sharp = require("sharp");

const root = path.resolve(__dirname, "..");
const sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];

async function render(size) {
  const source = size <= 48 ? "app-icon-small.svg" : "app-icon.svg";
  const png = await sharp(path.join(root, "assets", source), { density: 72 * size / (size <= 48 ? 16 : 512) })
    .resize(size, size).ensureAlpha().png().toBuffer();
  const metadata = await sharp(png).metadata();
  assert.equal(metadata.width, size);
  assert.equal(metadata.height, size);
  return png;
}

async function output(relativePath, bytes) {
  const target = path.join(root, relativePath);
  if (process.argv.includes("--check")) {
    assert.deepEqual(await fs.readFile(target), bytes, `${relativePath} needs regeneration`);
  } else {
    await fs.writeFile(target, bytes);
  }
}

async function main() {
  const images = [];
  for (const size of sizes) images.push(await render(size));
  const header = Buffer.alloc(6 + sizes.length * 16);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(sizes.length, 4);
  let offset = header.length;
  for (let i = 0; i < sizes.length; i++) {
    const position = 6 + i * 16;
    header[position] = sizes[i] % 256;
    header[position + 1] = sizes[i] % 256;
    header.writeUInt16LE(1, position + 4);
    header.writeUInt16LE(32, position + 6);
    header.writeUInt32LE(images[i].length, position + 8);
    header.writeUInt32LE(offset, position + 12);
    offset += images[i].length;
  }
  const ico = Buffer.concat([header, ...images]);
  await output("assets/app-icon.ico", ico);
  await output("v2/frontend/public/favicon.ico", ico);
  await output("assets/app-icon.png", await render(512));

  const chunks = [];
  for (const [type, size] of [["ic07", 128], ["ic08", 256], ["ic09", 512], ["ic10", 1024]]) {
    const png = await render(size);
    const chunk = Buffer.alloc(8);
    chunk.write(type);
    chunk.writeUInt32BE(png.length + 8, 4);
    chunks.push(chunk, png);
  }
  const icns = Buffer.alloc(8);
  icns.write("icns");
  icns.writeUInt32BE(8 + chunks.reduce((total, chunk) => total + chunk.length, 0), 4);
  await output("assets/app-icon.icns", Buffer.concat([icns, ...chunks]));
  console.log(`${process.argv.includes("--check") ? "Verified" : "Generated"} PNG, ICO, ICNS and favicon; ICO sizes: ${sizes.join(", ")}`);
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
