// Jetons d'un résultat d'outil : le texte à 4 octets par jeton, une image à ses pixels.
import assert from 'node:assert/strict';
import { imageSize, imageTokens, resultTokens } from './tokens.js';

const png = (width, height) => {
  const bytes = Buffer.alloc(33);
  bytes.writeUInt32BE(0x89504e47, 0);
  bytes.writeUInt32BE(0x0d0a1a0a, 4);
  bytes.writeUInt32BE(13, 8);
  bytes.write('IHDR', 12, 'ascii');
  bytes.writeUInt32BE(width, 16);
  bytes.writeUInt32BE(height, 20);
  return bytes.toString('base64');
};
// SOI, un segment APP0 à sauter, puis SOF0 : longueur, précision, hauteur, largeur.
const jpeg = (width, height, marker = 0xc0) => {
  const app0 = Buffer.from([0xff, 0xe0, 0x00, 0x04, 0x00, 0x00]);
  const sof = Buffer.alloc(11);
  sof.writeUInt8(0xff, 0);
  sof.writeUInt8(marker, 1);
  sof.writeUInt16BE(8, 2);
  sof.writeUInt8(8, 4);
  sof.writeUInt16BE(height, 5);
  sof.writeUInt16BE(width, 7);
  return Buffer.concat([Buffer.from([0xff, 0xd8]), app0, sof, Buffer.alloc(4)]).toString('base64');
};
const gif = (width, height) => {
  const bytes = Buffer.alloc(13);
  bytes.write('GIF89a', 0, 'ascii');
  bytes.writeUInt16LE(width, 6);
  bytes.writeUInt16LE(height, 8);
  return bytes.toString('base64');
};
const webp = (chunk, fill) => {
  const bytes = Buffer.alloc(32);
  bytes.write('RIFF', 0, 'ascii');
  bytes.write('WEBP', 8, 'ascii');
  bytes.write(chunk, 12, 'ascii');
  fill(bytes);
  return bytes.toString('base64');
};

// Une capture de simulateur de 400 × 870 : ⌈400/28⌉ × ⌈870/28⌉ = 15 × 32.
assert.deepEqual(imageSize(png(400, 870)), { width: 400, height: 870 });
assert.equal(imageTokens(png(400, 870)), 480);
assert.deepEqual(imageSize(jpeg(400, 870)), { width: 400, height: 870 }, 'après un segment APP0');
assert.deepEqual(imageSize(jpeg(640, 480, 0xc2)), { width: 640, height: 480 }, 'un JPEG progressif');
assert.equal(imageSize(jpeg(640, 480, 0xc4)), undefined, 'une table de Huffman ne donne pas de taille');
assert.deepEqual(imageSize(gif(28, 56)), { width: 28, height: 56 });
assert.deepEqual(
  imageSize(webp('VP8X', (b) => (b.writeUIntLE(399, 24, 3), b.writeUIntLE(869, 27, 3)))),
  { width: 400, height: 870 }
);
assert.deepEqual(
  imageSize(webp('VP8 ', (b) => (b.writeUInt16LE(400, 26), b.writeUInt16LE(870, 28)))),
  { width: 400, height: 870 }
);
assert.deepEqual(
  imageSize(webp('VP8L', (b) => b.writeUInt32LE(399 | (869 << 14), 21))),
  { width: 400, height: 870 }
);
assert.equal(imageSize(webp('ALPH', () => {})), undefined);
for (const unreadable of [undefined, '', Buffer.from('not an image').toString('base64'), 'AAAA']) {
  assert.equal(imageSize(unreadable), undefined);
  assert.equal(imageTokens(unreadable), 4784, "une image qu'on ne sait pas lire compte au plafond");
}
assert.equal(imageTokens(png(0, 870)), 4784);

// Pleine résolution d'un iPhone 17 Pro, 1206 × 2622 : réduite à 2 576 px de long côté, 1185 × 2576.
assert.equal(imageTokens(png(1206, 2622)), 43 * 92);
// Un très grand carré : réduit sous le plafond de 4 784 jetons, au plus près.
const square = imageTokens(png(5000, 5000));
assert.ok(square <= 4784 && square > 4600, String(square));

// Le texte d'un résultat à 4 octets par jeton, son image à ses pixels, jamais son base64.
const shot = png(400, 870);
const result = { content: [{ type: 'text', text: 'x'.repeat(400) }, { type: 'image', data: shot, mimeType: 'image/png' }] };
const withoutData = JSON.stringify({ content: [result.content[0], { type: 'image', data: '', mimeType: 'image/png' }] });
assert.equal(resultTokens(result), Buffer.byteLength(withoutData) / 4 + 480);
assert.equal(resultTokens({ content: [{ type: 'text', text: 'abcd' }] }), Buffer.byteLength('{"content":[{"type":"text","text":"abcd"}]}') / 4);
assert.equal(resultTokens(undefined), 0);

console.log('tokens ok');
