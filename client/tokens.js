// Jetons qu'un résultat d'outil coûte à Claude : le texte à 4 octets par jeton, une image à ses pixels.

// Une image se paie à ses pixels, pas à ses octets : ⌈largeur / 28⌉ × ⌈hauteur / 28⌉ jetons, après réduction
// à 2 576 px de long côté et 4 784 jetons au plus (modèles Claude 4.7 et suivants,
// platform.claude.com/docs/en/build-with-claude/vision). Compter son base64 à 4 octets par jeton la
// surestimait d'un facteur 20 à 30 pour une capture d'écran.
const IMAGE_PATCH_PX = 28;
const IMAGE_LONG_EDGE_PX = 2576;
const IMAGE_MAX_TOKENS = 4784;

/** Largeur et hauteur d'une image PNG, JPEG, GIF ou WebP en base64, lues dans son en-tête ; undefined sinon. */
export function imageSize(base64) {
  if (typeof base64 !== 'string' || !base64) return undefined;
  const bytes = Buffer.from(base64.slice(0, 1 << 20), 'base64');
  if (bytes.length >= 24 && bytes.readUInt32BE(0) === 0x89504e47) {
    return { width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20) };
  }
  if (bytes.length >= 10 && bytes.toString('ascii', 0, 3) === 'GIF') {
    return { width: bytes.readUInt16LE(6), height: bytes.readUInt16LE(8) };
  }
  if (bytes.length >= 30 && bytes.toString('ascii', 0, 4) === 'RIFF' && bytes.toString('ascii', 8, 12) === 'WEBP') {
    const chunk = bytes.toString('ascii', 12, 16);
    if (chunk === 'VP8X') return { width: 1 + bytes.readUIntLE(24, 3), height: 1 + bytes.readUIntLE(27, 3) };
    if (chunk === 'VP8 ') return { width: bytes.readUInt16LE(26) & 0x3fff, height: bytes.readUInt16LE(28) & 0x3fff };
    if (chunk === 'VP8L') {
      const bits = bytes.readUInt32LE(21);
      return { width: (bits & 0x3fff) + 1, height: ((bits >> 14) & 0x3fff) + 1 };
    }
    return undefined;
  }
  if (bytes.length >= 4 && bytes[0] === 0xff && bytes[1] === 0xd8) {
    // Les segments JPEG jusqu'au premier SOFn, qui donne hauteur puis largeur.
    let offset = 2;
    while (offset + 9 < bytes.length) {
      if (bytes[offset] !== 0xff) return undefined;
      const marker = bytes[offset + 1];
      const length = bytes.readUInt16BE(offset + 2);
      const frame = marker >= 0xc0 && marker <= 0xcf && ![0xc4, 0xc8, 0xcc].includes(marker);
      if (frame) return { width: bytes.readUInt16BE(offset + 7), height: bytes.readUInt16BE(offset + 5) };
      offset += 2 + length;
    }
  }
  return undefined;
}

/** Les jetons d'une image pour Claude, d'après ses pixels ; le plafond quand sa taille ne se lit pas. */
export function imageTokens(base64) {
  const size = imageSize(base64);
  if (!size || !(size.width > 0) || !(size.height > 0)) return IMAGE_MAX_TOKENS;
  let scale = Math.min(1, IMAGE_LONG_EDGE_PX / Math.max(size.width, size.height));
  const tokensAt = (factor) =>
    Math.ceil((size.width * factor) / IMAGE_PATCH_PX) * Math.ceil((size.height * factor) / IMAGE_PATCH_PX);
  while (tokensAt(scale) > IMAGE_MAX_TOKENS) scale *= Math.sqrt(IMAGE_MAX_TOKENS / tokensAt(scale)) * 0.999;
  return tokensAt(scale);
}

/** Les jetons approximatifs d'un résultat : son texte à 4 octets par jeton, chaque image à ses pixels. Non arrondi,
 * pour qu'une somme d'appels de texte seul égale exactement leurs octets / 4. */
export function resultTokens(result) {
  let images = 0;
  const text = JSON.stringify(result, (_key, value) => {
    if (value && typeof value === 'object' && value.type === 'image' && typeof value.data === 'string') {
      images += imageTokens(value.data);
      return { ...value, data: '' };
    }
    return value;
  });
  return Buffer.byteLength(text ?? '') / 4 + images;
}
