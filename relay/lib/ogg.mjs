// Reading an Ogg Opus file's length from the file itself.
//
// The relay charges each device for the seconds it sends, so it cannot take the app's word for how
// long a recording is. An Ogg Opus file says: its last page's granule position counts 48 kHz samples,
// including the pre-skip given in the OpusHead header (RFC 7845).

/** Duration in seconds, or null when this is not an Ogg Opus file. */
export function oggOpusSeconds(bytes) {
  const b = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const magic = (at, text) => [...text].every((c, i) => b[at + i] === c.charCodeAt(0));
  if (b.length < 47 || !magic(0, "OggS")) return null;

  // First page: OpusHead right after the page header and its lacing table.
  const firstSegments = b[26];
  const head = 27 + firstSegments;
  if (head + 19 > b.length || !magic(head, "OpusHead")) return null;
  const preSkip = view.getUint16(head + 10, true);

  // Walk every page so a truncated or forged file is refused rather than misread.
  let pos = 0;
  let granule = 0n;
  while (pos < b.length) {
    if (pos + 27 > b.length || !magic(pos, "OggS")) return null;
    const segments = b[pos + 26];
    if (pos + 27 + segments > b.length) return null;
    let body = 0;
    for (let i = 0; i < segments; i++) body += b[pos + 27 + i];
    const g = view.getBigUint64(pos + 6, true);
    if (g !== 0xffffffffffffffffn) granule = g; // -1 marks a page where no packet ends
    pos += 27 + segments + body;
  }
  if (pos !== b.length) return null;
  const samples = Number(granule) - preSkip;
  return samples > 0 ? samples / 48000 : 0;
}
