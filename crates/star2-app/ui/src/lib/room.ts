// Room tokens, ported from star-proto's `room.rs`. The browser mints its own
// for the same reason main.rs does: a room named by hand is guessable, and the
// secret half is the whole invitation. This is the one place the wire format
// lives in two languages - if room.rs changes, change this in the same commit.

const ALPHABET = "0123456789abcdefghjkmnpqrstvwxyz";
const SECRET_LEN = 10;
const MAX_LABEL = 24;

export function slugify(name: string): string {
  let out = "";
  let dash = false;
  for (const c of name) {
    if (out.length >= MAX_LABEL) break;
    if (/[0-9A-Za-z]/.test(c)) {
      out += c.toLowerCase();
      dash = false;
    } else if (out.length > 0 && !dash) {
      out += "-";
      dash = true;
    }
  }
  while (out.endsWith("-")) out = out.slice(0, -1);
  return out || "room";
}

const isSecret = (s: string): boolean =>
  s.length === SECRET_LEN && [...s].every((c) => ALPHABET.includes(c));

// Distinguishes "take me to this exact room" from "make me a room called this".
export function isRoomToken(s: string): boolean {
  const cut = s.lastIndexOf("-");
  return cut > 0 && isSecret(s.slice(cut + 1));
}

export function newRoomToken(name: string): string {
  const b = new Uint8Array(8);
  crypto.getRandomValues(b);
  let bits = 0n;
  for (let i = 7; i >= 0; i--) bits = (bits << 8n) | BigInt(b[i] ?? 0);
  let secret = "";
  for (let i = 0; i < SECRET_LEN; i++) {
    secret += ALPHABET[Number(bits & 31n)];
    bits >>= 5n;
  }
  return `${slugify(name)}-${secret}`;
}
