/** Standard base64 of bytes, chunked so multi-MB images do not build the string byte by byte. */
export function bytesToBase64(bytes: Uint8Array): string {
  const CHUNK = 32766; // ~32 KiB, multiple of 3, so chunks concatenate without inner padding
  let out = "";
  for (let i = 0; i < bytes.length; i += CHUNK) {
    out += btoa(String.fromCharCode(...bytes.subarray(i, i + CHUNK)));
  }
  return out;
}
